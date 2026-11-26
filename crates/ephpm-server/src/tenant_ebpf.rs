//! Per-vhost kernel network policy (`[server.tenant_network] ebpf_policy` and
//! `unix_socket_policy`).
//!
//! Loads the BPF programs (compiled by `build.rs`, embedded via `include_bytes!`)
//! with **aya** — pure Rust, no libbpf, no C runtime dependency — and attaches
//! `bind4/6` and `connect4/6` to ePHPm's own cgroup. Holds the maps so the
//! request path can tag threads and so the real-port pool can be provisioned.
//! See `crates/ephpm-server/bpf/vhostnet.bpf.c` for the kernel side.
//!
//! The AF_UNIX connect fence (`unix_socket_policy`, audit finding F2) rides on
//! the same per-thread tag: [`TenantEbpf`] owns its `unix_allow` map and hands
//! the fds of the shared `tag`/`nscfg`/`unix_allow` maps to
//! [`crate::tenant_unix_fence`], whose raw loader attaches the
//! `cgroup/connect_unix` + `sendmsg_unix` programs. The two policies are
//! independent switches ([`LoadOptions`]): either one being on is what makes
//! `serve()` construct this type and the router tag request threads.
//!
//! Close-time port reclamation (a `sock_release` program returning ports to the
//! pool) is **not implemented** and has no target release: reading a socket's
//! source port in `sock_release`
//! is verifier-rejected on 6.18, and the `bpf_sk_storage` alternative is a map
//! type aya 0.13 cannot load. Real ports and per-vhost counts are therefore
//! reclaimed on process exit; the `assigned` map keeps same-port rebinds
//! idempotent so a restarting sidecar does not consume a second slot.
//!
//! # HARD REQUIREMENT: fd-based links, nothing pinned (no-leak guarantee)
//!
//! Attachments are fd-based `bpf_link`s held in process memory, and NOTHING is
//! pinned to bpffs (`/sys/fs/bpf`). This is not a style preference — it is the
//! mechanism that makes a killed ePHPm leave zero kernel residue:
//!
//! * **Graceful shutdown:** dropping [`TenantEbpf`] drops the owned [`aya::Ebpf`]
//!   (and with it every program, link, and map) → the kernel detaches and frees.
//! * **Crash / `kill -9`:** no `Drop` runs, but the process's fds close, the
//!   kernel refcounts every link/prog/map to zero, and auto-reaps them.
//!
//! Verified empirically on Linux 6.18 (`bpftool prog|map|cgroup show` before/after
//! a `kill -9` reports `0 / 0 / 0`). We NEVER call `link.pin()`, `map.pin()`, or
//! `prog.pin()`, and aya attaches cgroup programs via `BPF_LINK_CREATE` (an
//! fd-based link) on kernels that support it — which every target of this feature
//! (Linux ≥ 5.10) does.
//!
//! # Real-port allocation & per-vhost quotas
//!
//! Real sidecar ports come from a **pool ePHPm owns**, not a scan. [`fill_pool`]
//! loads every port in the configured `sidecar_port_range` into the kernel
//! `port_pool` QUEUE at startup and sets the per-vhost quota. `bind4` pops a free
//! port at bind time; `sock_release` pushes it back. The range MUST NOT overlap
//! the kernel ephemeral range — [`TenantEbpf::assert_no_ephemeral_overlap`]
//! enforces that fail-closed before anything is loaded.
//!
//! [`fill_pool`]: TenantEbpf::fill_pool
//!
//! # Threading & the setjmp/longjmp boundary
//!
//! [`TagGuard`] is created at the top of the router's `run_php` closure and
//! dropped when that closure returns — on the SAME blocking/pool thread that ran
//! PHP. Its `Drop` performs a `bpf_map_delete_elem` **syscall**, not a PHP call,
//! so it does not cross PHP's setjmp/longjmp boundary; `PhpRuntime::execute`
//! catches its own Zend bailout internally and returns a `Result`, so the closure
//! always unwinds normally and the guard always runs. Clearing the tag before the
//! pooled ZTS thread is reused is the one hard invariant — a stale tag would carry
//! a previous request's vhost identity into the next request (a cross-tenant
//! leak of exactly the kind the per-site DB/KV/WS scoping already guards against).

use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::Result;
use ephpm_config::{TcpEgressTarget, UnixSocketTarget};

/// Which policies [`TenantEbpf::load_and_attach`] attaches. The `vhostnet`
/// object (and its maps, including the per-thread `tag`) is loaded whenever
/// any is on; the programs are attached per switch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadOptions {
    /// `[server.tenant_network] ebpf_policy`: the sidecar feature —
    /// `cgroup/bind4+6` transparent port-rewrite + per-vhost quota, and the
    /// `cgroup/connect4+6` loopback-authorization / ownership path.
    pub tcp_policy: bool,
    /// `[server.tenant_network] tcp_egress_policy` is on (audit finding
    /// F-SSRF-1): attach `cgroup/connect4+6` and deny tagged-tenant TCP egress
    /// to IMDS / loopback-except-infra / (optionally) RFC-1918. Shares the
    /// connect programs with [`tcp_policy`](Self::tcp_policy).
    pub tcp_egress: bool,
    /// Under [`tcp_egress`](Self::tcp_egress), also deny RFC-1918
    /// (`10/8`, `172.16/12`, `192.168/16`).
    pub deny_rfc1918: bool,
    /// `[server.tenant_network] unix_socket_policy != "off"`: reserve the
    /// shared-map fds the AF_UNIX fence needs so
    /// [`TenantEbpf::attach_unix_fence`] can be called afterwards.
    pub unix_fence: bool,
}

/// Index of a TCP-egress deny reason in the kernel `egress_denied` counter
/// array (kept in sync with `DENY_*` in `bpf/vhostnet.bpf.c`). Used to label
/// the `ephpm_tenant_tcp_egress_denied_total` metric.
pub const EGRESS_DENY_REASONS: [&str; 3] = ["imds", "loopback", "rfc1918"];

/// One numeric id per canonical site key. The kernel `tag`/`assigned`/
/// `port_owner` maps key on `u32` vhost ids, not strings; this assigns a stable
/// id the first time a site key is seen. Monotonic, never reused within a
/// process.
#[derive(Default)]
pub struct SiteIdRegistry {
    inner: dashmap::DashMap<String, u32>,
    next: std::sync::atomic::AtomicU32,
}

impl SiteIdRegistry {
    /// Stable id for a canonical site key, allocating on first use. `0` is
    /// reserved for "untagged", so ids start at `1`.
    #[must_use]
    pub fn id_for(&self, site_key: &str) -> u32 {
        if let Some(v) = self.inner.get(site_key) {
            return *v;
        }
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        *self.inner.entry(site_key.to_owned()).or_insert(id)
    }
}

// ===========================================================================
// Linux implementation.
// ===========================================================================
#[cfg(target_os = "linux")]
mod imp {
    // FFI + BPF map interop: `libc::gettid`/`setrlimit` and the `aya::Pod` impl.
    // Every unsafe block below carries a SAFETY note.
    #![allow(unsafe_code)]

    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::linux::fs::MetadataExt;
    use std::sync::Mutex;

    use anyhow::{Context, anyhow};
    use aya::maps::lpm_trie::{Key as LpmKey, LpmTrie};
    use aya::maps::{Array, HashMap as AyaHashMap, MapData, Queue};
    use aya::programs::{CgroupAttachMode, CgroupSockAddr};
    use ephpm_config::UNIX_PATH_MAX;

    use super::{Arc, LoadOptions, Result, SiteIdRegistry, TcpEgressTarget, UnixSocketTarget};
    use crate::tenant_unix_fence::{MapFds, UnixFence};

    /// Compiled BPF object, produced by `build.rs` (`clang -target bpf`) and
    /// embedded. The bytes ship in the binary — no runtime file, no libbpf.
    ///
    /// `aya::include_bytes_aligned!` (NOT the std `include_bytes!`) is mandatory:
    /// the `object` ELF parser reads the header via aligned loads, and a plain
    /// `include_bytes!` places the blob at 1-byte alignment, which makes
    /// `EbpfLoader::load` fail with "error parsing ELF data" on an otherwise
    /// valid object. The aligned macro pads it to a suitable boundary.
    static BPF_OBJECT: &[u8] =
        aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/vhostnet.bpf.o"));

    /// `{dev, ino}` of `/proc/self/ns/pid`, written to the kernel `nscfg` map so
    /// the programs resolve the caller's TID in ePHPm's pid-namespace. `#[repr(C)]`
    /// to match `struct nscfg_t` on the BPF side.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsCfg {
        dev: u64,
        ino: u64,
    }
    // SAFETY: `NsCfg` is `#[repr(C)]`, contains only `u64` fields (no padding, no
    // pointers, valid for any bit pattern), matching the kernel `struct nscfg_t`
    // exactly — the invariants `aya::Pod` requires for a map value.
    unsafe impl aya::Pod for NsCfg {}

    /// Key of the kernel `unix_allow` map: `struct unix_allow_key_t` in
    /// `bpf/vhostnet_common.h`. `vhost` is the site's id, `path`/`len` the
    /// destination exactly as the fence normalises it
    /// ([`UnixSocketTarget::sun_path`]). `#[repr(C)]`, no padding: 4 + 4 + 108.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct UnixAllowKey {
        vhost: u32,
        len: u32,
        path: [u8; UNIX_PATH_MAX],
    }
    // SAFETY: `UnixAllowKey` is `#[repr(C)]` and made only of `u32`s and a byte
    // array — no padding (4 + 4 + 108 = 116 bytes, 4-aligned), no pointers,
    // valid for any bit pattern — matching `struct unix_allow_key_t` exactly.
    unsafe impl aya::Pod for UnixAllowKey {}

    /// Data portion of the `egress_cidr` LPM-trie key — the 8 bytes after the
    /// prefix length. `vhost` is matched exactly (every entry has `prefix_len >=
    /// 32`); `addr` is the IPv4 destination. Both are stored **big-endian** so
    /// the kernel's MSB-first prefix match treats the vhost id as the high 32
    /// bits and the address as the next 32. Matches the fields after `prefixlen`
    /// in `struct egress_cidr_key_t`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CidrData {
        vhost: u32, // big-endian
        addr: u32,  // network order (big-endian)
    }
    // SAFETY: `CidrData` is `#[repr(C)]`, two `u32`s only — 8 bytes, no padding,
    // no pointers, valid for any bit pattern — the invariants `aya::Pod`
    // requires. It is the trie key's data half (`Key<CidrData>` prepends the
    // `u32` prefix length), matching `struct egress_cidr_key_t`.
    unsafe impl aya::Pod for CidrData {}

    /// Key of the `egress_hostport` exact-match allowlist. `vhost` and `port`
    /// are host order (hashed as raw bytes, same byte order both sides); `addr`
    /// is network order. Matches `struct egress_hostport_key_t`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EgressHostPortKey {
        vhost: u32, // host order (internal id)
        addr: u32,  // network order
        port: u32,  // host order
    }
    // SAFETY: `EgressHostPortKey` is `#[repr(C)]`, three `u32`s — 12 bytes, no
    // padding, no pointers, valid for any bit pattern — matching `struct
    // egress_hostport_key_t` exactly.
    unsafe impl aya::Pod for EgressHostPortKey {}

    /// Runtime policy bitmask written to the kernel `policy[0]` array at load.
    /// Kept in sync with the `POL_*` macros in `bpf/vhostnet.bpf.c`.
    const POL_SIDECAR: u32 = 1 << 0;
    const POL_EGRESS: u32 = 1 << 1;
    const POL_DENY_RFC1918: u32 = 1 << 2;

    /// Live handle: owns the loaded programs + their fd-links (via the retained
    /// [`aya::Ebpf`]) and the maps the request path and provisioning write.
    pub struct TenantEbpf {
        /// Owns every program, cgroup link, and non-taken map. Dropping it
        /// detaches and frees everything (graceful path); process death closes
        /// the fds and the kernel reaps (kill -9). Never pinned.
        _bpf: aya::Ebpf,
        /// TID → vhost id. Written per request by [`TagGuard`] on the executing
        /// thread; behind a `Mutex` because the map is touched concurrently from
        /// every blocking/pool thread, and aya's typed map writes take `&mut`.
        tag: Mutex<AyaHashMap<MapData, u32, u32>>,
        /// Free real ports; filled once by [`Self::fill_pool`].
        port_pool: Mutex<Queue<MapData, u32>>,
        /// `[0]` = `max_sidecar_ports_per_vhost`; set once by [`Self::fill_pool`].
        quota: Mutex<Array<MapData, u32>>,
        /// Per-site AF_UNIX destination allowlist, written at startup by
        /// [`Self::set_unix_allow`]. Read by the fence programs only.
        unix_allow: Mutex<AyaHashMap<MapData, UnixAllowKey, u8>>,
        /// Per-site TCP egress CIDR allowlist (LPM trie), written at startup by
        /// [`Self::set_egress_allow`]. Read by `connect4/6` only.
        egress_cidr: Mutex<LpmTrie<MapData, CidrData, u8>>,
        /// Per-site TCP egress exact `ip:port` allowlist, written at startup by
        /// [`Self::set_egress_allow`]. Read by `connect4/6` only.
        egress_hostport: Mutex<AyaHashMap<MapData, EgressHostPortKey, u8>>,
        /// Per-reason TCP-egress deny counter (`DENY_*`), incremented in-kernel
        /// and sampled by [`Self::sample_egress_denied`] for the metric.
        egress_denied: Mutex<Array<MapData, u64>>,
        /// Whether the TCP egress fence (`connect4/6` + `POL_EGRESS`) is active
        /// on this handle — set once at construction from [`LoadOptions`].
        tcp_egress: bool,
        /// The cgroup every program attaches to, kept so the fence can be
        /// attached after the base load (its failure policy is the caller's).
        cgroup: File,
        /// Fds of the maps the fence shares with this object. Valid for as long
        /// as `self` (the owning `MapData`s live in the fields above / `_bpf`).
        shared_fds: MapFds,
        /// The attached AF_UNIX fence, once [`Self::attach_unix_fence`] succeeds.
        unix_fence: Mutex<Option<UnixFence>>,
        site_ids: SiteIdRegistry,
    }

    impl TenantEbpf {
        /// Load the `vhostnet` object, populate `nscfg` + `infra_ports`, and —
        /// when `opts.tcp_policy` — attach the four `bind4/6` + `connect4/6`
        /// programs to `cgroup_path` (defaults to ePHPm's own cgroup). Fails
        /// closed: any error here is fatal to startup.
        ///
        /// `infra_ports` are ePHPm's own loopback service ports (the stock
        /// `pdo_mysql` wire listener, the KV RESP listener) that every tagged
        /// vhost is allowed to reach.
        ///
        /// The AF_UNIX fence is **not** attached here — call
        /// [`Self::attach_unix_fence`] next, so `serve()` can apply the
        /// `unix_socket_policy` failure mode to that step alone.
        ///
        /// # Errors
        ///
        /// Returns an error if the BPF object cannot be loaded (missing BTF, an
        /// unsupported map/program type, verifier rejection), the cgroup cannot
        /// be opened, a program cannot be attached (missing `CAP_BPF` /
        /// `CAP_NET_ADMIN`), or the `nscfg` / `infra_ports` maps cannot be
        /// written.
        pub fn load_and_attach(
            cgroup_path: Option<&str>,
            infra_ports: &[u16],
            opts: &LoadOptions,
        ) -> Result<Arc<Self>> {
            bump_memlock_rlimit();

            let mut bpf = aya::EbpfLoader::new()
                .load(BPF_OBJECT)
                .context("loading the embedded vhostnet.bpf.o")?;

            // Open the cgroup ePHPm lives in (v2 unified hierarchy). Every
            // program — the four sockaddr ones here, the two unix ones in
            // `attach_unix_fence` — attaches here.
            let cg_path = match cgroup_path {
                Some(p) => p.to_string(),
                None => self_cgroup_path().context("resolving ePHPm's own cgroup path")?,
            };
            let cgroup = std::fs::File::open(&cg_path)
                .with_context(|| format!("opening cgroup {cg_path} for BPF attach"))?;

            // sockaddr programs. `bind4/6` (sidecar port-rewrite) attach only
            // for the sidecar feature; `connect4/6` attach for EITHER the
            // sidecar feature or the egress fence, which share them. Attaching
            // `bind` for an egress-only node would wrongly rewrite every tenant
            // bind to a sidecar port.
            let mut to_attach: Vec<&str> = Vec::new();
            if opts.tcp_policy {
                to_attach.extend_from_slice(&["vhost_bind4", "vhost_bind6"]);
            }
            if opts.tcp_policy || opts.tcp_egress {
                to_attach.extend_from_slice(&["vhost_connect4", "vhost_connect6"]);
            }
            for name in to_attach {
                let prog: &mut CgroupSockAddr = bpf
                    .program_mut(name)
                    .ok_or_else(|| anyhow!("BPF program {name} missing from object"))?
                    .try_into()
                    .with_context(|| format!("program {name} is not a cgroup/sockaddr program"))?;
                prog.load().with_context(|| format!("verifier rejected {name}"))?;
                prog.attach(&cgroup, CgroupAttachMode::Single)
                    .with_context(|| format!("attaching {name} to {cg_path}"))?;
            }

            // Runtime policy flags the connect programs read (which branches
            // run). Written once; never changes after load.
            {
                let mut flags = 0u32;
                if opts.tcp_policy {
                    flags |= POL_SIDECAR;
                }
                if opts.tcp_egress {
                    flags |= POL_EGRESS;
                    if opts.deny_rfc1918 {
                        flags |= POL_DENY_RFC1918;
                    }
                }
                let mut policy: Array<_, u32> = bpf
                    .map_mut("policy")
                    .ok_or_else(|| anyhow!("policy map missing"))?
                    .try_into()
                    .context("policy map is not an ARRAY")?;
                policy.set(0, flags, 0).context("writing policy flags")?;
            }

            // Write nscfg[0] = {dev, ino} of /proc/self/ns/pid so the kernel can
            // resolve TIDs in ePHPm's pid-namespace (the PoC finding).
            {
                let st = std::fs::metadata("/proc/self/ns/pid")
                    .context("stat /proc/self/ns/pid for nscfg")?;
                let mut nscfg: Array<_, NsCfg> = bpf
                    .map_mut("nscfg")
                    .ok_or_else(|| anyhow!("nscfg map missing"))?
                    .try_into()
                    .context("nscfg map is not an ARRAY")?;
                nscfg
                    .set(0, NsCfg { dev: st.st_dev(), ino: st.st_ino() }, 0)
                    .context("writing nscfg")?;
            }

            // Allow-list ePHPm's own loopback infra ports.
            {
                let mut infra: AyaHashMap<_, u32, u8> = bpf
                    .map_mut("infra_ports")
                    .ok_or_else(|| anyhow!("infra_ports map missing"))?
                    .try_into()
                    .context("infra_ports map is not a HASH")?;
                for &port in infra_ports {
                    infra.insert(u32::from(port), 1u8, 0).context("writing infra_ports")?;
                }
            }

            // Raw fds of the maps the AF_UNIX fence object shares with this one.
            // Captured before `take_map`: moving a map's userspace handle out of
            // `bpf` moves its `OwnedFd` without duplicating it, so the numbers
            // stay valid for as long as the handles below (and `_bpf`) live.
            let raw_fd = |bpf: &aya::Ebpf, name: &str| -> Result<std::os::fd::RawFd> {
                // `aya::maps::Map` is an enum over `MapData`; only the two kinds
                // vhostnet_common.h declares for the shared maps can appear.
                match bpf.map(name).ok_or_else(|| anyhow!("{name} map missing"))? {
                    aya::maps::Map::HashMap(data) | aya::maps::Map::Array(data) => {
                        Ok(data.fd().as_fd().as_raw_fd())
                    }
                    other => Err(anyhow!("{name} map has an unexpected type: {other:?}")),
                }
            };
            let shared_fds = MapFds {
                tag: raw_fd(&bpf, "tag")?,
                nscfg: raw_fd(&bpf, "nscfg")?,
                unix_allow: raw_fd(&bpf, "unix_allow")?,
            };

            // Take long-lived owned handles for the maps written after startup
            // (tag: per request; port_pool/quota: fill_pool; unix_allow:
            // set_unix_allow). The programs already hold kernel references to
            // these maps, so moving the userspace handle out of `bpf` does not
            // unload them.
            let tag: AyaHashMap<_, u32, u32> = bpf
                .take_map("tag")
                .ok_or_else(|| anyhow!("tag map missing"))?
                .try_into()
                .context("tag map is not a HASH")?;
            let port_pool: Queue<_, u32> = bpf
                .take_map("port_pool")
                .ok_or_else(|| anyhow!("port_pool map missing"))?
                .try_into()
                .context("port_pool map is not a QUEUE")?;
            let quota: Array<_, u32> = bpf
                .take_map("quota")
                .ok_or_else(|| anyhow!("quota map missing"))?
                .try_into()
                .context("quota map is not an ARRAY")?;
            let unix_allow: AyaHashMap<_, UnixAllowKey, u8> = bpf
                .take_map("unix_allow")
                .ok_or_else(|| anyhow!("unix_allow map missing"))?
                .try_into()
                .context("unix_allow map is not a HASH")?;
            let egress_cidr: LpmTrie<_, CidrData, u8> = bpf
                .take_map("egress_cidr")
                .ok_or_else(|| anyhow!("egress_cidr map missing"))?
                .try_into()
                .context("egress_cidr map is not an LPM_TRIE")?;
            let egress_hostport: AyaHashMap<_, EgressHostPortKey, u8> = bpf
                .take_map("egress_hostport")
                .ok_or_else(|| anyhow!("egress_hostport map missing"))?
                .try_into()
                .context("egress_hostport map is not a HASH")?;
            let egress_denied: Array<_, u64> = bpf
                .take_map("egress_denied")
                .ok_or_else(|| anyhow!("egress_denied map missing"))?
                .try_into()
                .context("egress_denied map is not an ARRAY")?;

            Ok(Arc::new(Self {
                _bpf: bpf,
                tag: Mutex::new(tag),
                port_pool: Mutex::new(port_pool),
                quota: Mutex::new(quota),
                unix_allow: Mutex::new(unix_allow),
                egress_cidr: Mutex::new(egress_cidr),
                egress_hostport: Mutex::new(egress_hostport),
                egress_denied: Mutex::new(egress_denied),
                tcp_egress: opts.tcp_egress,
                cgroup,
                shared_fds,
                unix_fence: Mutex::new(None),
                site_ids: SiteIdRegistry::default(),
            }))
        }

        /// Load and attach the AF_UNIX fence (`cgroup/connect_unix` +
        /// `sendmsg_unix`) to the same cgroup. Idempotent: a second call while
        /// attached is a no-op. Until this succeeds a tagged thread's unix
        /// connects are **unfenced**, which is why `serve()` treats a failure
        /// here per `unix_socket_policy` (`"required"` ⇒ refuse startup).
        ///
        /// # Errors
        ///
        /// Returns the loader's error: kernel older than 6.7, no BTF, kfuncs
        /// missing, verifier rejection (log attached), or `EPERM` on load /
        /// link (missing `CAP_BPF` + `CAP_NET_ADMIN`).
        ///
        /// # Panics
        ///
        /// Panics if the internal fence mutex is poisoned, which cannot happen:
        /// it is only ever held across infallible assignments.
        pub fn attach_unix_fence(&self) -> Result<()> {
            let mut slot = self.unix_fence.lock().expect("unix_fence mutex poisoned");
            if slot.is_some() {
                return Ok(());
            }
            let fence = UnixFence::load_and_attach(&self.cgroup, &self.shared_fds)?;
            *slot = Some(fence);
            Ok(())
        }

        /// Whether the AF_UNIX fence is attached (so tagged threads are fenced).
        ///
        /// # Panics
        ///
        /// Panics if the internal fence mutex is poisoned (see
        /// [`Self::attach_unix_fence`]).
        #[must_use]
        pub fn unix_fence_active(&self) -> bool {
            self.unix_fence.lock().expect("unix_fence mutex poisoned").is_some()
        }

        /// Allow `site_key` to reach every destination in `targets`, writing
        /// one `unix_allow` entry per target under the site's vhost id (allocated
        /// now, so the id the fence sees at request time is the same one). A
        /// site never named here can reach no unix socket.
        ///
        /// # Errors
        ///
        /// Returns an error if a map write (`bpf_map_update_elem`) fails — e.g.
        /// the map is full (4096 entries).
        ///
        /// # Panics
        ///
        /// Panics if the internal `unix_allow` mutex is poisoned, which can
        /// only follow a panic in a prior map write; in practice this runs once
        /// at startup before any concurrent access.
        pub fn set_unix_allow(&self, site_key: &str, targets: &[UnixSocketTarget]) -> Result<()> {
            let vhost = self.site_ids.id_for(site_key);
            let mut map = self.unix_allow.lock().expect("unix_allow mutex poisoned");
            for target in targets {
                let (path, len) = target.sun_path();
                let key = UnixAllowKey { vhost, len, path };
                map.insert(key, 1u8, 0).with_context(|| {
                    format!("writing unix_allow entry {target:?} for site {site_key:?}")
                })?;
            }
            Ok(())
        }

        /// Allow `site_key` to reach every TCP destination in `targets` even
        /// while the egress fence would otherwise deny it (audit finding
        /// F-SSRF-1). A CIDR/bare-IP entry goes into the `egress_cidr` LPM trie
        /// (any port); an `ip:port` entry into the `egress_hostport` exact map.
        /// The site's vhost id is allocated now so the id the connect program
        /// sees at request time is the same one. An allowlist hit overrides
        /// every deny category, including IMDS.
        ///
        /// # Errors
        ///
        /// Returns an error if a map write (`bpf_map_update_elem`) fails — e.g.
        /// the map is full (4096 entries).
        ///
        /// # Panics
        ///
        /// Panics if a map mutex is poisoned, which can only follow a panic in a
        /// prior map write; in practice this runs once at startup before any
        /// concurrent access.
        pub fn set_egress_allow(&self, site_key: &str, targets: &[TcpEgressTarget]) -> Result<()> {
            let vhost = self.site_ids.id_for(site_key);
            for target in targets {
                match *target {
                    TcpEgressTarget::Cidr { addr, prefix } => {
                        // vhost is matched exactly (32 bits) before the CIDR.
                        // Both halves stored big-endian so the kernel's
                        // MSB-first LPM match treats vhost as the high 32 bits.
                        let data = CidrData { vhost: vhost.to_be(), addr: u32::from(addr).to_be() };
                        let key = LpmKey::new(32 + u32::from(prefix), data);
                        let mut map = self.egress_cidr.lock().expect("egress_cidr mutex poisoned");
                        map.insert(&key, 1u8, 0).with_context(|| {
                            format!("writing egress_cidr {addr}/{prefix} for site {site_key:?}")
                        })?;
                    }
                    TcpEgressTarget::HostPort { addr, port } => {
                        let key = EgressHostPortKey {
                            vhost,
                            addr: u32::from(addr).to_be(),
                            port: u32::from(port),
                        };
                        let mut map =
                            self.egress_hostport.lock().expect("egress_hostport mutex poisoned");
                        map.insert(key, 1u8, 0).with_context(|| {
                            format!("writing egress_hostport {addr}:{port} for site {site_key:?}")
                        })?;
                    }
                }
            }
            Ok(())
        }

        /// Whether the TCP egress fence is active on this handle (the
        /// `connect4/6` programs attached with `POL_EGRESS` set).
        #[must_use]
        pub fn tcp_egress_active(&self) -> bool {
            self.tcp_egress
        }

        /// Snapshot the per-reason TCP-egress deny counters (`[imds, loopback,
        /// rfc1918]`), or `None` if the egress fence is not active. Read on
        /// demand (a Prometheus scrape); a failed read of any slot yields `0`
        /// for that reason rather than failing the whole snapshot.
        #[must_use]
        pub fn sample_egress_denied(&self) -> Option<[u64; 3]> {
            if !self.tcp_egress {
                return None;
            }
            let map = self.egress_denied.lock().ok()?;
            let mut out = [0u64; 3];
            for (i, slot) in out.iter_mut().enumerate() {
                let idx = u32::try_from(i).unwrap_or(0);
                *slot = map.get(&idx, 0).unwrap_or(0);
            }
            Some(out)
        }

        /// Fill `port_pool` with every port in the (validated) `range` and set
        /// `quota[0]` to `max_per_vhost`. Call once, at startup, AFTER
        /// [`Self::assert_no_ephemeral_overlap`] has passed.
        ///
        /// Real ports are then popped by `bind4` at bind time; there is no
        /// per-`(vhost, vport)` provisioning step — a site just binds its virtual
        /// port and the kernel hands out a private real one.
        ///
        /// # Errors
        ///
        /// Returns an error if a port cannot be pushed into `port_pool` or the
        /// quota value cannot be written (both are `bpf_map_update_elem`
        /// syscalls).
        ///
        /// # Panics
        ///
        /// Panics if the internal `port_pool`/`quota` mutex is poisoned — which
        /// can only happen if a prior map write panicked. In practice this is
        /// called once at startup before any concurrent access, so it does not.
        pub fn fill_pool(&self, range: (u16, u16), max_per_vhost: u32) -> Result<()> {
            let mut pool = self.port_pool.lock().expect("port_pool mutex poisoned");
            for p in range.0..=range.1 {
                pool.push(u32::from(p), 0)
                    .with_context(|| format!("pushing port {p} into port_pool"))?;
            }
            drop(pool);
            let mut quota = self.quota.lock().expect("quota mutex poisoned");
            quota.set(0, max_per_vhost, 0).context("setting the per-vhost quota")?;
            Ok(())
        }

        /// Read `net.ipv4.ip_local_port_range` and reject an overlapping sidecar
        /// range. Fail-closed: an overlap is a hard startup error, because a
        /// tenant's outbound `connect()` could be auto-assigned a source port
        /// ePHPm also wants to hand out as a sidecar real port.
        ///
        /// # Errors
        ///
        /// Returns an error if the configured `range` overlaps the kernel
        /// ephemeral range read from `/proc/sys/net/ipv4/ip_local_port_range`.
        pub fn assert_no_ephemeral_overlap(range: (u16, u16)) -> Result<()> {
            let s = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
                .unwrap_or_else(|_| "32768\t60999".into());
            let mut it = s.split_whitespace();
            let elo: u16 = it.next().and_then(|v| v.parse().ok()).unwrap_or(32768);
            let ehi: u16 = it.next().and_then(|v| v.parse().ok()).unwrap_or(60999);
            let overlaps = range.0 <= ehi && elo <= range.1;
            anyhow::ensure!(
                !overlaps,
                "sidecar_port_range {}-{} overlaps the kernel ephemeral range {elo}-{ehi} \
                 (net.ipv4.ip_local_port_range). Move the sidecar range BELOW the ephemeral \
                 floor to avoid source-port collisions.",
                range.0,
                range.1
            );
            Ok(())
        }

        /// Write `tag[gettid()] = vhost_id` for the current thread. Returns a
        /// guard that clears it on drop. Called at the top of `run_php`.
        ///
        /// `gettid()` returns the caller's TID **in ePHPm's pid-namespace**; the
        /// kernel side resolves the same value via `bpf_get_ns_current_pid_tgid`
        /// against `nscfg`. Best-effort: a failed map update logs and continues
        /// (the request still runs; it just loses its kernel network identity for
        /// that call — which fails closed at the bind/connect hooks, never open).
        #[must_use]
        pub fn tag_current_thread(self: &Arc<Self>, site_key: &str) -> TagGuard {
            let vhost = self.site_ids.id_for(site_key);
            let tid = current_tid();
            if let Ok(mut tag) = self.tag.lock()
                && let Err(e) = tag.insert(tid, vhost, 0)
            {
                tracing::warn!(tid, vhost, error = %e, "tenant_ebpf: failed to write tag");
            }
            TagGuard { owner: Arc::clone(self), tid }
        }
    }

    /// Clears `tag[tid]` on drop. Pure syscall — safe across the PHP setjmp
    /// boundary (see the module docs).
    pub struct TagGuard {
        owner: Arc<TenantEbpf>,
        tid: u32,
    }

    impl Drop for TagGuard {
        fn drop(&mut self) {
            if let Ok(mut tag) = self.owner.tag.lock() {
                // `remove` errors only if the key is already gone — harmless.
                let _ = tag.remove(&self.tid);
            }
        }
    }

    /// Current thread's TID in this process's pid-namespace.
    fn current_tid() -> u32 {
        // Use the raw `SYS_gettid` syscall, NOT `libc::gettid()`: the glibc
        // `gettid()` wrapper only exists in glibc >= 2.30, and the release links
        // against an older glibc (in the ephpm-ci container, to keep the binary's
        // glibc floor low), where `gettid` is an undefined symbol at link time.
        // The syscall has always existed and links everywhere.
        //
        // SAFETY: `SYS_gettid` takes no arguments, never fails, and has no memory
        // effects — it just reads the caller's kernel TID.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        // A Linux TID is always positive and far below u32::MAX (pid_max is
        // 2^22 at most), so the conversion never truncates; the default (0) is
        // an unreachable fallback that avoids an `as` cast and its lints.
        u32::try_from(tid).unwrap_or_default()
    }

    /// Raise `RLIMIT_MEMLOCK` to unlimited. A no-op on kernels that account BPF
    /// memory to the cgroup (≥ 5.11), but cheap insurance on older ones.
    fn bump_memlock_rlimit() {
        let lim = libc::rlimit { rlim_cur: libc::RLIM_INFINITY, rlim_max: libc::RLIM_INFINITY };
        // SAFETY: `setrlimit` reads `lim` (a valid, fully-initialized `rlimit`)
        // for the duration of the call and writes nothing back. Ignoring the
        // result is intentional — failure just means we rely on cgroup memory
        // accounting instead, and the subsequent BPF load will surface any real
        // memory problem.
        unsafe {
            libc::setrlimit(libc::RLIMIT_MEMLOCK, &raw const lim);
        }
    }

    /// ePHPm's own cgroup v2 directory, from the `0::<path>` line of
    /// `/proc/self/cgroup` joined onto the unified mount at `/sys/fs/cgroup`.
    fn self_cgroup_path() -> Result<String> {
        let content =
            std::fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
        for line in content.lines() {
            // Unified (v2) hierarchy entry: "0::<path>".
            if let Some(rel) = line.strip_prefix("0::") {
                let rel = rel.trim();
                let rel = rel.strip_prefix('/').unwrap_or(rel);
                return Ok(format!("/sys/fs/cgroup/{rel}"));
            }
        }
        Err(anyhow!(
            "no cgroup v2 (unified) entry in /proc/self/cgroup; the tenant_network feature \
             requires a cgroup v2 host. Set [server.tenant_network] cgroup_path explicitly."
        ))
    }
}

// ===========================================================================
// Non-Linux: no-op stub. Always compiles; every method is inert. `serve()`
// never constructs this because `validate()` already hard-errors when
// `ebpf_policy = true` off Linux — but the type must exist so router.rs and
// lib.rs compile unchanged on Windows/macOS.
// ===========================================================================
#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{Arc, LoadOptions, Result, SiteIdRegistry, TcpEgressTarget, UnixSocketTarget};

    /// Inert non-Linux placeholder with the same public surface as the Linux
    /// [`TenantEbpf`](super::TenantEbpf).
    pub struct TenantEbpf {
        _site_ids: SiteIdRegistry,
    }

    impl TenantEbpf {
        /// Always fails: the feature is Linux-only (and `validate()` already
        /// rejected it before reaching here).
        ///
        /// # Errors
        ///
        /// Always returns an error — the eBPF policies are Linux-only.
        pub fn load_and_attach(
            _cgroup_path: Option<&str>,
            _infra_ports: &[u16],
            _opts: &LoadOptions,
        ) -> Result<Arc<Self>> {
            anyhow::bail!("[server.tenant_network] eBPF policies are Linux-only")
        }

        /// Always fails on non-Linux (never reached: `validate()` rejects the
        /// knob first).
        ///
        /// # Errors
        ///
        /// Always returns an error — the AF_UNIX fence is Linux-only.
        pub fn attach_unix_fence(&self) -> Result<()> {
            anyhow::bail!("[server.tenant_network] unix_socket_policy is Linux-only")
        }

        /// Always `false` on non-Linux.
        #[must_use]
        pub fn unix_fence_active(&self) -> bool {
            false
        }

        /// No-op on non-Linux.
        ///
        /// # Errors
        ///
        /// Never returns an error on non-Linux.
        pub fn set_unix_allow(&self, _site_key: &str, _targets: &[UnixSocketTarget]) -> Result<()> {
            Ok(())
        }

        /// No-op on non-Linux.
        ///
        /// # Errors
        ///
        /// Never returns an error on non-Linux.
        pub fn set_egress_allow(
            &self,
            _site_key: &str,
            _targets: &[TcpEgressTarget],
        ) -> Result<()> {
            Ok(())
        }

        /// Always `false` on non-Linux.
        #[must_use]
        pub fn tcp_egress_active(&self) -> bool {
            false
        }

        /// Always `None` on non-Linux.
        #[must_use]
        pub fn sample_egress_denied(&self) -> Option<[u64; 3]> {
            None
        }

        /// No-op on non-Linux.
        ///
        /// # Errors
        ///
        /// Never returns an error on non-Linux.
        pub fn fill_pool(&self, _range: (u16, u16), _max_per_vhost: u32) -> Result<()> {
            Ok(())
        }

        /// No-op on non-Linux.
        ///
        /// # Errors
        ///
        /// Never returns an error on non-Linux.
        pub fn assert_no_ephemeral_overlap(_range: (u16, u16)) -> Result<()> {
            Ok(())
        }

        /// Returns an inert guard on non-Linux.
        #[must_use]
        pub fn tag_current_thread(self: &Arc<Self>, _site_key: &str) -> TagGuard {
            TagGuard
        }
    }

    /// Zero-sized no-op guard on non-Linux.
    pub struct TagGuard;
}

pub use imp::{TagGuard, TenantEbpf};

// ===========================================================================
// TCP-egress deny counters: on-scrape injection (no background task).
// ===========================================================================

/// The live fence handle whose kernel deny-counters the metrics scrape reads.
/// A `Weak` so registering it never keeps the handle alive past `serve()`; the
/// render path upgrades it per scrape. One server per process, mirroring the
/// `turso_cdc_metrics` global-registry pattern.
static EGRESS_SOURCE: OnceLock<Mutex<Weak<TenantEbpf>>> = OnceLock::new();

/// Register `handle` as the source of the `ephpm_tenant_tcp_egress_denied_total`
/// counters. Called by `serve()` once the fence is loaded. Replacing an earlier
/// registration (e.g. a second server in-process) is fine.
pub fn register_egress_source(handle: &Arc<TenantEbpf>) {
    let slot = EGRESS_SOURCE.get_or_init(|| Mutex::new(Weak::new()));
    if let Ok(mut guard) = slot.lock() {
        *guard = Arc::downgrade(handle);
    }
}

/// Append the `ephpm_tenant_tcp_egress_denied_total{reason=...}` counter family
/// to a rendered Prometheus scrape, sampled live from the kernel `egress_denied`
/// map. Emits nothing when the fence is not active (an absent series, not a
/// zero), matching how `render_site_gauges` handles inactive families.
pub fn render_egress_deny_counters(out: &mut String) {
    let Some(slot) = EGRESS_SOURCE.get() else { return };
    let Some(handle) = slot.lock().ok().and_then(|g| g.upgrade()) else { return };
    let Some(counts) = handle.sample_egress_denied() else { return };
    out.push_str(
        "# HELP ephpm_tenant_tcp_egress_denied_total Tenant TCP connects denied by the egress \
         fence, by reason.\n",
    );
    out.push_str("# TYPE ephpm_tenant_tcp_egress_denied_total counter\n");
    for (reason, count) in EGRESS_DENY_REASONS.iter().zip(counts.iter()) {
        use std::fmt::Write as _;
        // Writing to a String is infallible; the result is ignored deliberately.
        let _ =
            writeln!(out, "ephpm_tenant_tcp_egress_denied_total{{reason=\"{reason}\"}} {count}");
    }
}

// ===========================================================================
// Pure-Rust model of the connect-side egress verdict.
// ===========================================================================

/// A connect verdict, mirroring the return of `do_connect` in
/// `bpf/vhostnet.bpf.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressVerdict {
    /// Allow the connect (possibly after a sidecar port rewrite, not modelled).
    Allow,
    /// Deny the connect; the field is the `DENY_*` reason index.
    Deny(EgressDenyReason),
}

/// Why the egress fence denied a connect. Indices match `DENY_*` in the BPF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressDenyReason {
    /// Link-local `169.254.0.0/16` (cloud IMDS).
    Imds,
    /// A loopback destination that is neither an ePHPm infra port nor
    /// allowlisted.
    Loopback,
    /// An RFC-1918 destination, denied under `POL_DENY_RFC1918`.
    Rfc1918,
}

/// The `POL_*` runtime flags that steer the connect verdict (mirrors the
/// `policy[0]` bitmask in `bpf/vhostnet.bpf.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyFlags {
    /// `POL_SIDECAR` — the `ebpf_policy` sidecar feature is on.
    pub sidecar: bool,
    /// `POL_EGRESS` — the TCP egress fence is on.
    pub egress: bool,
    /// `POL_DENY_RFC1918` — deny RFC-1918 under the egress fence.
    pub deny_rfc1918: bool,
}

/// **Pure-Rust mirror of the egress decision in `bpf/vhostnet.bpf.c`
/// `do_connect`** for an IPv4 destination of a *tagged* tenant thread, covering
/// the branches the TCP egress fence governs (the sidecar virtual-port rewrite
/// and ownership branches are omitted — they only reorder ALLOW outcomes). This
/// is the unit-tested model; the live BPF is exercised by the `#[ignore]`
/// attach test. **KEEP IN SYNC with the C** — the two must agree branch for
/// branch.
///
/// `flags` are the `POL_*` flags, `is_loopback` is the loopback classification,
/// `infra` is true for an ePHPm infra port, `allowed` is true when the per-site
/// allowlist matches, and `dst` is the IPv4 destination.
#[must_use]
pub fn egress_verdict_model(
    flags: PolicyFlags,
    is_loopback: bool,
    infra: bool,
    allowed: bool,
    dst: std::net::Ipv4Addr,
) -> EgressVerdict {
    let PolicyFlags { sidecar, egress, deny_rfc1918 } = flags;
    if is_loopback {
        // (sidecar virtual-port rewrite + ownership omitted — ALLOW reorderings)
        if infra {
            return EgressVerdict::Allow; // ePHPm infra port (mysql wire, KV RESP)
        }
        if allowed {
            return EgressVerdict::Allow; // explicit per-site allowlist
        }
        if sidecar || egress {
            return EgressVerdict::Deny(EgressDenyReason::Loopback); // loopback floor
        }
        return EgressVerdict::Allow;
    }
    // Non-loopback.
    if !egress {
        return EgressVerdict::Allow; // egress fence off
    }
    if allowed {
        return EgressVerdict::Allow; // explicit allow overrides every deny
    }
    if is_link_local(dst) {
        return EgressVerdict::Deny(EgressDenyReason::Imds); // 169.254.0.0/16, always
    }
    if deny_rfc1918 && is_rfc1918(dst) {
        return EgressVerdict::Deny(EgressDenyReason::Rfc1918);
    }
    EgressVerdict::Allow // public egress
}

/// `169.254.0.0/16` — mirrors `is_linklocal4_be` in the BPF.
#[must_use]
fn is_link_local(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 169 && o[1] == 254
}

/// RFC-1918 private ranges — mirrors `is_rfc1918_be` in the BPF.
#[must_use]
fn is_rfc1918(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::{EgressDenyReason, EgressVerdict, PolicyFlags, egress_verdict_model};

    /// The egress fence active, sidecar off — the F-SSRF-1 secure default. The
    /// signature cases the task calls out.
    #[test]
    fn egress_fence_default_decisions() {
        // Helper: egress on, RFC-1918 denied, no allowlist match, not infra.
        let v = |is_loopback: bool, infra: bool, allowed: bool, dst: &str| {
            egress_verdict_model(
                PolicyFlags { sidecar: false, egress: true, deny_rfc1918: true },
                is_loopback,
                infra,
                allowed,
                dst.parse::<Ipv4Addr>().unwrap(),
            )
        };

        // IMDS is always denied.
        assert_eq!(
            v(false, false, false, "169.254.169.254"),
            EgressVerdict::Deny(EgressDenyReason::Imds)
        );

        // Loopback: an ePHPm infra port (e.g. 127.0.0.1:3306 marked infra) is
        // allowed; a non-infra loopback port (e.g. 127.0.0.1:9999) is denied.
        assert_eq!(v(true, true, false, "127.0.0.1"), EgressVerdict::Allow, "infra port allowed");
        assert_eq!(
            v(true, false, false, "127.0.0.1"),
            EgressVerdict::Deny(EgressDenyReason::Loopback),
            "non-infra loopback denied"
        );

        // RFC-1918 denied by default.
        assert_eq!(
            v(false, false, false, "10.0.0.5"),
            EgressVerdict::Deny(EgressDenyReason::Rfc1918)
        );
        assert_eq!(
            v(false, false, false, "172.16.3.4"),
            EgressVerdict::Deny(EgressDenyReason::Rfc1918)
        );
        assert_eq!(
            v(false, false, false, "192.168.1.1"),
            EgressVerdict::Deny(EgressDenyReason::Rfc1918)
        );
        // 172.15/172.32 are NOT in 172.16/12.
        assert_eq!(v(false, false, false, "172.15.0.1"), EgressVerdict::Allow);
        assert_eq!(v(false, false, false, "172.32.0.1"), EgressVerdict::Allow);

        // Public egress stays allowed.
        assert_eq!(v(false, false, false, "1.1.1.1"), EgressVerdict::Allow);
        assert_eq!(v(false, false, false, "8.8.8.8"), EgressVerdict::Allow);
    }

    /// An explicit allowlist hit overrides every deny category, IMDS included.
    #[test]
    fn allowlist_overrides_every_deny() {
        for dst in ["169.254.169.254", "10.0.0.5", "127.0.0.1"] {
            let ip = dst.parse::<Ipv4Addr>().unwrap();
            assert_eq!(
                egress_verdict_model(
                    PolicyFlags { sidecar: false, egress: true, deny_rfc1918: true },
                    dst == "127.0.0.1",
                    false,
                    true,
                    ip
                ),
                EgressVerdict::Allow,
                "allowlisted {dst} must be allowed"
            );
        }
    }

    /// With RFC-1918 NOT denied, IMDS + loopback-floor still apply but RFC-1918
    /// is allowed (the opt-out sub-toggle).
    #[test]
    fn rfc1918_toggle_off_keeps_imds_and_loopback() {
        let m = |deny_rfc1918: bool, is_loopback: bool, infra: bool, dst: &str| {
            egress_verdict_model(
                PolicyFlags { sidecar: false, egress: true, deny_rfc1918 },
                is_loopback,
                infra,
                false,
                dst.parse::<Ipv4Addr>().unwrap(),
            )
        };
        assert_eq!(m(false, false, false, "10.0.0.5"), EgressVerdict::Allow, "rfc1918 allowed");
        assert_eq!(
            m(false, false, false, "169.254.169.254"),
            EgressVerdict::Deny(EgressDenyReason::Imds),
            "IMDS still denied"
        );
        assert_eq!(
            m(false, true, false, "127.0.0.1"),
            EgressVerdict::Deny(EgressDenyReason::Loopback),
            "loopback floor still applies"
        );
    }

    /// With the egress fence OFF (sidecar-only or neither), non-loopback egress
    /// is unrestricted — preserving the historical `ebpf_policy` behavior.
    #[test]
    fn egress_off_allows_all_non_loopback() {
        let sidecar_only = PolicyFlags { sidecar: true, egress: false, deny_rfc1918: false };
        for dst in ["169.254.169.254", "10.0.0.5", "1.1.1.1"] {
            let ip = dst.parse::<Ipv4Addr>().unwrap();
            // sidecar on, egress off.
            assert_eq!(
                egress_verdict_model(sidecar_only, false, false, false, ip),
                EgressVerdict::Allow,
                "egress off must not fence {dst}"
            );
        }
        // Sidecar loopback floor still denies non-infra loopback even with the
        // egress fence off.
        assert_eq!(
            egress_verdict_model(sidecar_only, true, false, false, "127.0.0.1".parse().unwrap()),
            EgressVerdict::Deny(EgressDenyReason::Loopback)
        );
    }
}

//! Per-vhost kernel network policy (`[server.tenant_network] ebpf_policy`).
//!
//! Loads the BPF programs (compiled by `build.rs`, embedded via `include_bytes!`)
//! with **aya** — pure Rust, no libbpf, no C runtime dependency — and attaches
//! `bind4/6` and `connect4/6` to ePHPm's own cgroup. Holds the maps so the
//! request path can tag threads and so the real-port pool can be provisioned.
//! See `crates/ephpm-server/bpf/vhostnet.bpf.c` for the kernel side.
//!
//! Close-time port reclamation (a `sock_release` program returning ports to the
//! pool) is deferred to v0.8.2: reading a socket's source port in `sock_release`
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

use std::sync::Arc;

use anyhow::Result;

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

    use std::os::linux::fs::MetadataExt;
    use std::sync::Mutex;

    use anyhow::{Context, anyhow};
    use aya::maps::{Array, HashMap as AyaHashMap, MapData, Queue};
    use aya::programs::{CgroupAttachMode, CgroupSockAddr};

    use super::{Arc, Result, SiteIdRegistry};

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
        site_ids: SiteIdRegistry,
    }

    impl TenantEbpf {
        /// Load, populate `nscfg` + `infra_ports`, and attach the four
        /// `bind4/6` + `connect4/6` programs to `cgroup_path` (defaults to
        /// ePHPm's own cgroup). Fails closed: any error here is fatal to startup
        /// when `ebpf_policy = true`.
        ///
        /// `infra_ports` are ePHPm's own loopback service ports (the stock
        /// `pdo_mysql` wire listener, the KV RESP listener) that every tagged
        /// vhost is allowed to reach.
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
        ) -> Result<Arc<Self>> {
            bump_memlock_rlimit();

            let mut bpf = aya::EbpfLoader::new()
                .load(BPF_OBJECT)
                .context("loading the embedded vhostnet.bpf.o")?;

            // Open the cgroup ePHPm lives in (v2 unified hierarchy). All five
            // programs attach here.
            let cg_path = match cgroup_path {
                Some(p) => p.to_string(),
                None => self_cgroup_path().context("resolving ePHPm's own cgroup path")?,
            };
            let cgroup = std::fs::File::open(&cg_path)
                .with_context(|| format!("opening cgroup {cg_path} for BPF attach"))?;

            // sockaddr programs (bind/connect rewrite + authorize).
            for name in ["vhost_bind4", "vhost_bind6", "vhost_connect4", "vhost_connect6"] {
                let prog: &mut CgroupSockAddr = bpf
                    .program_mut(name)
                    .ok_or_else(|| anyhow!("BPF program {name} missing from object"))?
                    .try_into()
                    .with_context(|| format!("program {name} is not a cgroup/sockaddr program"))?;
                prog.load().with_context(|| format!("verifier rejected {name}"))?;
                prog.attach(&cgroup, CgroupAttachMode::Single)
                    .with_context(|| format!("attaching {name} to {cg_path}"))?;
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

            // Take long-lived owned handles for the maps written after startup
            // (tag: per request; port_pool/quota: fill_pool). The programs
            // already hold kernel references to these maps, so moving the
            // userspace handle out of `bpf` does not unload them.
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

            Ok(Arc::new(Self {
                _bpf: bpf,
                tag: Mutex::new(tag),
                port_pool: Mutex::new(port_pool),
                quota: Mutex::new(quota),
                site_ids: SiteIdRegistry::default(),
            }))
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
        // SAFETY: `gettid(2)` takes no arguments, never fails, and has no memory
        // effects — it just reads the caller's kernel TID.
        let tid = unsafe { libc::gettid() };
        // A TID is always positive, so the sign bit is never set.
        tid.cast_unsigned()
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
    use super::{Arc, Result, SiteIdRegistry};

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
        /// Always returns an error — the eBPF policy is Linux-only.
        pub fn load_and_attach(
            _cgroup_path: Option<&str>,
            _infra_ports: &[u16],
        ) -> Result<Arc<Self>> {
            anyhow::bail!("[server.tenant_network] ebpf_policy is Linux-only")
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

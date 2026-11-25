#!/usr/bin/env bash
# Verify the eBPF tenant-fence objects compiled into a RELEASE build are REAL
# (non-empty), not the fail-closed EMPTY placeholder that
# crates/ephpm-server/build.rs writes when the clang/eBPF toolchain is missing
# or the compile fails.
#
# Why this is a release gate: the AF_UNIX connect fence (audit finding F2,
# `[server.tenant_network] unix_socket_policy`) defaults to `auto` — ON for a
# multi-tenant Linux >= 6.7 node. A binary built from the empty placeholder
# comes up UNFENCED under that default and only logs about it, so a single
# transient toolchain failure on a release leg could silently ship an unfenced
# release. release.yml makes clang fatal on the Linux legs AND runs this check
# after the build so the real objects are proven to have shipped.
#
# Usage: check-ebpf-objects.sh [<cargo-target-triple>]
#   The triple narrows the search to target/<triple>/release/build/; omitted,
#   the whole target/ tree is searched. Exits non-zero (with a ::error:: line)
#   if either object is missing or empty.
set -euo pipefail

triple="${1:-}"
if [ -n "$triple" ]; then
  search="target/${triple}/release/build"
else
  search="target"
fi

if [ ! -d "$search" ]; then
  echo "::error::eBPF object check: build directory '${search}' does not exist (did the release build run?)"
  exit 1
fi

rc=0
for obj in unixsock.bpf.o vhostnet.bpf.o; do
  # build.rs writes the object to its OUT_DIR, i.e.
  # target/<triple>/release/build/ephpm-server-<hash>/out/<obj>.
  found=""
  while IFS= read -r f; do
    if [ -s "$f" ]; then
      found="$f"
      break
    fi
  done < <(find "$search" -path "*/build/ephpm-server-*/out/${obj}" 2>/dev/null)

  if [ -n "$found" ]; then
    size=$(wc -c < "$found" | tr -d '[:space:]')
    echo "ok: ${obj} is ${size} bytes (${found})"
  else
    any=$(find "$search" -path "*/build/ephpm-server-*/out/${obj}" 2>/dev/null | head -1)
    if [ -n "$any" ]; then
      echo "::error::${obj} is EMPTY (${any}) — build.rs wrote the fail-closed placeholder, so the clang/eBPF toolchain failed. Refusing to ship a release whose AF_UNIX tenant fence (unix_socket_policy=auto) would be UNFENCED."
    else
      echo "::error::${obj} not found under ${search} — the eBPF object did not build. Refusing to ship a release whose AF_UNIX tenant fence (unix_socket_policy=auto) would be UNFENCED."
    fi
    rc=1
  fi
done

exit "$rc"

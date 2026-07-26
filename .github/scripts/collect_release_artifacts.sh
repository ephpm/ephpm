#!/usr/bin/env bash
# Collect this run's release archives across ALL run attempts.
#
# Why not actions/download-artifact@v4: it only sees artifacts uploaded in the
# CURRENT run attempt. On a `gh run rerun --failed`, GitHub starts a new
# attempt and only the reran (previously-failed) legs upload fresh artifacts;
# the legs that already succeeded keep their archives attached to the PREVIOUS
# attempt. download-artifact silently misses those, so the release publish
# fails even though every binary exists somewhere in the run -- which forced
# every retry to be a full all-legs rebuild in one attempt.
#
# The REST run-artifacts endpoint lists every artifact the run produced,
# regardless of attempt. Collecting from it means a single flaky leg can be
# rerun-failed (~15-28 min) and the release still assembles all archives.
#
# Env: GH_TOKEN (actions:read), REPO (owner/name), RUN_ID, optional OUT_DIR
# (default "artifacts"). Extracts every ephpm-v* artifact's contents flat into
# OUT_DIR. Downloads sequentially -- downloading all 12 in parallel saturated
# the self-hosted runner's egress to blob storage and dropped the tail (the
# reason the old download-artifact step was split into two batches).
set -euo pipefail

: "${GH_TOKEN:?GH_TOKEN required}" "${REPO:?REPO required}" "${RUN_ID:?RUN_ID required}"
OUT_DIR="${OUT_DIR:-artifacts}"
API="https://api.github.com"
PREFIX="ephpm-v"

# curl is always present on the runner; jq + unzip usually are. Install any
# that are missing (this pool already apt-installs build deps in other jobs).
missing=""
for tool in curl jq unzip; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  echo "==> installing missing tools:$missing"
  sudo apt-get update -qq
  sudo apt-get install -y -qq $missing
fi

hdr_auth="Authorization: Bearer ${GH_TOKEN}"
hdr_accept="Accept: application/vnd.github+json"
hdr_ver="X-GitHub-Api-Version: 2022-11-28"

# 1. List every ephpm-v* artifact for the run, across all attempts. Emit
#    "name<TAB>id<TAB>archive_download_url" lines.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
page=1
while :; do
  resp="$(curl -sSf -H "$hdr_auth" -H "$hdr_accept" -H "$hdr_ver" \
    "${API}/repos/${REPO}/actions/runs/${RUN_ID}/artifacts?per_page=100&page=${page}")"
  count="$(printf '%s' "$resp" | jq '.artifacts | length')"
  [ "$count" -eq 0 ] && break
  printf '%s' "$resp" | jq -r --arg p "$PREFIX" \
    '.artifacts[] | select(.expired | not) | select(.name | startswith($p))
     | [.name, (.id | tostring), .archive_download_url] | @tsv' >> "$tmp/all.tsv"
  [ "$count" -lt 100 ] && break
  page=$((page + 1))
done
touch "$tmp/all.tsv"

# Dedup by name, keeping the newest artifact id -- a reran leg's fresh upload
# (same name) supersedes the older attempt's copy. Sort name asc, id desc,
# then take the first row per name.
sort -t"$(printf '\t')" -k1,1 -k2,2nr "$tmp/all.tsv" \
  | awk -F"$(printf '\t')" '!seen[$1]++' > "$tmp/dedup.tsv"

n="$(wc -l < "$tmp/dedup.tsv" | tr -d ' ')"
if [ "$n" -eq 0 ]; then
  echo "ERROR: no ${PREFIX}* artifacts found for run ${RUN_ID}" >&2
  exit 1
fi
echo "Found ${n} release artifact(s) across all attempts of run ${RUN_ID}"

# 2. Download + extract each, sequentially. curl follows the 302 to signed
#    blob storage and drops the Authorization header on the cross-host
#    redirect (its default since 7.58), which is exactly what the signed URL
#    wants.
mkdir -p "$OUT_DIR"
while IFS="$(printf '\t')" read -r name id url; do
  [ -n "$name" ] || continue
  echo "-> ${name} (artifact id ${id})"
  curl -sSfL --retry 5 --retry-delay 3 -H "$hdr_auth" -H "$hdr_accept" \
    -o "$tmp/${name}.zip" "$url"
  unzip -o -q "$tmp/${name}.zip" -d "$OUT_DIR"
done < "$tmp/dedup.tsv"

echo "Collected $(find "$OUT_DIR" -type f -name '*.tar.gz' | wc -l | tr -d ' ') archive(s) into ${OUT_DIR}/:"
find "$OUT_DIR" -type f -name '*.tar.gz' -printf '  %f\n' | sort

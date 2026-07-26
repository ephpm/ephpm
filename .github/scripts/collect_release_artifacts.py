#!/usr/bin/env python3
"""Collect this run's release archives across ALL run attempts.

Why this exists instead of actions/download-artifact@v4:

    download-artifact only sees artifacts uploaded in the CURRENT run
    attempt. On a `gh run rerun --failed`, GitHub starts a new attempt and
    only the rerun (previously-failed) legs upload fresh artifacts; the legs
    that already succeeded keep their artifacts attached to the PREVIOUS
    attempt. So download-artifact silently misses those, and the release
    publish fails even though every binary exists somewhere in the run. That
    forced every retry to be a full all-legs rebuild in one attempt.

The REST run-artifacts endpoint lists every artifact the run produced,
regardless of attempt. Collecting from it means a single flaky leg can be
rerun-failed (~15-28 min) and the release still assembles all archives --
no more losing a whole ~2h matrix to one one-off flake.

Env: GH_TOKEN (actions:read), REPO (owner/name), RUN_ID, and optional
OUT_DIR (default "artifacts"). Extracts every ephpm-v* artifact's contents
flat into OUT_DIR. Downloads sequentially -- downloading all 12 in parallel
saturated the self-hosted runner's egress to blob storage and dropped the
tail (the reason the old step split into two batches).
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request
import zipfile

API = "https://api.github.com"
TOKEN = os.environ["GH_TOKEN"]
REPO = os.environ["REPO"]
RUN_ID = os.environ["RUN_ID"]
OUT_DIR = os.environ.get("OUT_DIR", "artifacts")
NAME_PREFIX = "ephpm-v"


def api_json(url):
    req = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {TOKEN}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(req) as resp:
        return json.load(resp)


class _StripAuthOnRedirect(urllib.request.HTTPRedirectHandler):
    """Drop the Authorization header when the artifact download 302-redirects
    to signed blob storage on a different host. Sending GitHub's bearer token
    to the blob host is both wrong and rejected; curl strips it automatically,
    urllib does not."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is not None:
            new.headers = {
                k: v for k, v in new.headers.items() if k.lower() != "authorization"
            }
        return new


_OPENER = urllib.request.build_opener(_StripAuthOnRedirect)


def download(url, dest, attempts=5):
    req = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {TOKEN}",
            "Accept": "application/vnd.github+json",
        },
    )
    for i in range(1, attempts + 1):
        try:
            with _OPENER.open(req) as resp, open(dest, "wb") as f:
                while True:
                    chunk = resp.read(1 << 20)
                    if not chunk:
                        break
                    f.write(chunk)
            return
        except (urllib.error.URLError, TimeoutError) as e:
            print(f"  download attempt {i}/{attempts} failed: {e}", file=sys.stderr)
            if i == attempts:
                raise
            time.sleep(3 * i)


def list_artifacts():
    # Dedup by name, keeping the newest artifact id -- a rerun re-upload of a
    # leg (same name) supersedes the older attempt's copy.
    by_name = {}
    page = 1
    while True:
        data = api_json(
            f"{API}/repos/{REPO}/actions/runs/{RUN_ID}/artifacts"
            f"?per_page=100&page={page}"
        )
        items = data.get("artifacts", [])
        if not items:
            break
        for a in items:
            if a.get("expired") or not a["name"].startswith(NAME_PREFIX):
                continue
            prev = by_name.get(a["name"])
            if prev is None or a["id"] > prev["id"]:
                by_name[a["name"]] = a
        if len(items) < 100:
            break
        page += 1
    return by_name


def main():
    arts = list_artifacts()
    if not arts:
        print(
            f"ERROR: no {NAME_PREFIX}* artifacts found for run {RUN_ID}",
            file=sys.stderr,
        )
        return 1

    os.makedirs(OUT_DIR, exist_ok=True)
    print(
        f"Found {len(arts)} release artifact(s) across all attempts of run {RUN_ID}"
    )
    for name in sorted(arts):
        a = arts[name]
        zpath = f"{name}.zip"
        print(f"-> {name} (artifact id {a['id']})")
        download(a["archive_download_url"], zpath)
        with zipfile.ZipFile(zpath) as z:
            z.extractall(OUT_DIR)
        os.remove(zpath)

    tarballs = sorted(f for f in os.listdir(OUT_DIR) if f.endswith(".tar.gz"))
    print(f"Collected {len(tarballs)} archive(s) into {OUT_DIR}/:")
    for t in tarballs:
        print(f"  {t}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

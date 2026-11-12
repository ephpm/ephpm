//! The destination a scheduled embedded-database backup is written to — and
//! restored from: either S3-compatible object storage ([`crate::s3::S3Client`])
//! or a **local filesystem directory** ([`LocalBackupStore`]).
//!
//! Both are exposed behind one small enum, [`BackupStore`], with the four
//! operations the backup/restore loop needs — `put_file`, `get_to_file`,
//! `delete`, `list` — so the *entire* loop in [`crate::db_backup`] (ownership
//! gating, snapshot + gzip, the empty-snapshot skip, the retention sweep, the
//! metrics) is shared by both. The dispatch is a plain `enum`, not a `dyn`
//! trait object: there are exactly two destinations and the method set is
//! closed.
//!
//! **The object-key shape is identical for both.** An object is always keyed
//! `<prefix>/<site>/<YYYYMMDDTHHMMSSZ>.db.gz`, with `/` separators. The local
//! store maps that key onto `<base>/<prefix>/<site>/<…>.db.gz` on disk and
//! normalises listed keys back to forward slashes, so the timestamp/retention
//! parsing (`object_key`, `parse_key_timestamp`, `select_at_or_before`,
//! `retention_victims`, `list_site_backups`, `to_backup_objects`) is reused
//! unchanged against either destination.
//!
//! Credentials, encryption and the "the object is a plain gzip, not AEAD"
//! stance are documented on [`crate::db_backup`] and [`crate::s3`]; a local
//! directory needs no credentials.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;

use crate::s3::{ObjectMeta, S3Client};

/// Where scheduled backups live: S3-compatible object storage, or a local
/// filesystem directory. One enum so the whole backup/restore loop is shared;
/// see the module docs for the identical key shape both use.
pub enum BackupStore {
    /// S3-compatible object storage (AWS S3, Cloudflare R2, MinIO, GCS).
    S3(S3Client),
    /// A local filesystem directory.
    Local(LocalBackupStore),
}

impl std::fmt::Debug for BackupStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::S3(c) => f.debug_tuple("BackupStore::S3").field(c.location()).finish(),
            Self::Local(l) => f.debug_tuple("BackupStore::Local").field(&l.base).finish(),
        }
    }
}

impl BackupStore {
    /// Upload/copy `file` to `key`.
    ///
    /// # Errors
    ///
    /// Transport or I/O failure (S3: a non-2xx answer; local: a copy, fsync
    /// or rename failure).
    pub async fn put_file(&self, key: &str, file: &Path) -> anyhow::Result<()> {
        match self {
            Self::S3(c) => c.put_file(key, file).await,
            Self::Local(l) => l.put_file(key, file).await,
        }
    }

    /// Download/copy `key` into `file` (created or truncated). `Ok(None)` when
    /// the object does not exist, matching [`S3Client::get_to_file`].
    ///
    /// # Errors
    ///
    /// Transport or I/O failure (a missing object is not an error — it is
    /// `Ok(None)`).
    pub async fn get_to_file(&self, key: &str, file: &Path) -> anyhow::Result<Option<u64>> {
        match self {
            Self::S3(c) => c.get_to_file(key, file).await,
            Self::Local(l) => l.get_to_file(key, file).await,
        }
    }

    /// Delete `key`. Deleting an absent key is success, matching
    /// [`S3Client::delete`].
    ///
    /// # Errors
    ///
    /// Transport or I/O failure.
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        match self {
            Self::S3(c) => c.delete(key).await,
            Self::Local(l) => l.delete(key).await,
        }
    }

    /// Every object whose key starts with `prefix`, matching
    /// [`S3Client::list`]'s string-prefix semantics.
    ///
    /// # Errors
    ///
    /// Transport or I/O failure.
    pub async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        match self {
            Self::S3(c) => c.list(prefix).await,
            Self::Local(l) => l.list(prefix).await,
        }
    }

    /// A human URI for one object `key`, for the status endpoint and the CLI
    /// plan line: `s3://<bucket>/<key>` for S3, the on-disk path for local.
    #[must_use]
    pub fn object_uri(&self, key: &str) -> String {
        match self {
            Self::S3(c) => format!("s3://{}/{key}", c.location().bucket),
            Self::Local(l) => l.object_path(key).display().to_string(),
        }
    }

    /// A human URI for the directory of one object `dir` (a key prefix),
    /// trailing-slashed, for the `ephpm db backup list` location header.
    #[must_use]
    pub fn dir_uri(&self, dir: &str) -> String {
        match self {
            Self::S3(c) => format!("s3://{}/{dir}/", c.location().bucket),
            Self::Local(l) => format!("{}/", l.object_path(dir).display()),
        }
    }

    /// The destination bucket, for the status JSON (`None` for a local store).
    #[must_use]
    pub fn status_bucket(&self) -> Option<&str> {
        match self {
            Self::S3(c) => Some(c.location().bucket.as_str()),
            Self::Local(_) => None,
        }
    }

    /// The destination endpoint, for the status JSON (`None` for a local store).
    #[must_use]
    pub fn status_endpoint(&self) -> Option<&str> {
        match self {
            Self::S3(c) => Some(c.location().endpoint.as_str()),
            Self::Local(_) => None,
        }
    }

    /// The local destination directory, for the status JSON (`None` for S3).
    #[must_use]
    pub fn status_dir(&self) -> Option<String> {
        match self {
            Self::S3(_) => None,
            Self::Local(l) => Some(l.base.display().to_string()),
        }
    }

    /// A short label naming the destination kind + where, for the startup log.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::S3(c) => format!("s3://{} ({})", c.location().bucket, c.location().endpoint),
            Self::Local(l) => format!("local dir {}", l.base.display()),
        }
    }
}

/// A backup destination that is a local filesystem directory. Objects are
/// regular files under `base`, named by their object key
/// (`<prefix>/<site>/<ts>.db.gz`, `/`-separated) — the same key shape S3 uses,
/// so the retention/timestamp parsing is shared.
///
/// Writes are atomic (temp file in the destination directory → fsync → rename
/// → directory fsync on Unix), mirroring [`crate::kv_snapshot`]'s writer and
/// `ephpm db restore`'s install, so a crash mid-copy never leaves a truncated
/// object a later restore would pick.
#[derive(Debug, Clone)]
pub struct LocalBackupStore {
    base: PathBuf,
}

impl LocalBackupStore {
    /// A store rooted at `base`. The directory is created lazily on the first
    /// `put_file` (its parents too), so a not-yet-existing destination is fine.
    #[must_use]
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// `<base>/<key>`, resolving the `/`-separated key to OS path components.
    /// Empty / `.` / `..` components are dropped, so a key can never escape
    /// `base` (keys are internally generated from validated site keys +
    /// timestamps in any case).
    fn object_path(&self, key: &str) -> PathBuf {
        let mut path = self.base.clone();
        for part in key.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                continue;
            }
            path.push(part);
        }
        path
    }

    /// Copy `file` to `<base>/<key>`, atomically (temp + fsync + rename +
    /// directory fsync), creating parent directories.
    async fn put_file(&self, key: &str, file: &Path) -> anyhow::Result<()> {
        let dest = self.object_path(key);
        let src = file.to_path_buf();
        spawn_blocking_result(move || put_file_blocking(&src, &dest)).await
    }

    /// Copy `<base>/<key>` to `file`. `Ok(None)` when the object is absent.
    async fn get_to_file(&self, key: &str, file: &Path) -> anyhow::Result<Option<u64>> {
        let src = self.object_path(key);
        let dst = file.to_path_buf();
        spawn_blocking_result(move || get_to_file_blocking(&src, &dst)).await
    }

    /// Remove `<base>/<key>` (absent is success), then prune the now-empty
    /// `<site>` directory it lived in — never `base` itself.
    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let path = self.object_path(key);
        let base = self.base.clone();
        spawn_blocking_result(move || delete_blocking(&base, &path)).await
    }

    /// Every regular file under `base` whose relative (`/`-normalised) key
    /// starts with `prefix`.
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        let base = self.base.clone();
        let prefix = prefix.to_string();
        spawn_blocking_result(move || list_blocking(&base, &prefix)).await
    }
}

/// Run `f` on the blocking pool, flattening the join error into the result.
async fn spawn_blocking_result<T, F>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.context("local backup task panicked")?
}

/// The temporary file beside `dest` a local write streams into (same scheme as
/// the KV snapshot writer: a hidden, pid+nanos-qualified sibling).
fn temp_path(dest: &Path) -> PathBuf {
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    dest.with_file_name(format!(".{name}.tmp-{}-{nanos}", std::process::id()))
}

/// Rename the finished temporary over the destination and make the rename
/// durable (fsync the directory on Unix).
fn finish_atomic(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, dest)?;
    #[cfg(unix)]
    if let Some(dir) = dest.parent() {
        let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

fn put_file_blocking(src: &Path, dest: &Path) -> anyhow::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = temp_path(dest);
    let copy = (|| -> anyhow::Result<()> {
        let mut input =
            std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
        let out =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let mut writer = std::io::BufWriter::new(out);
        std::io::copy(&mut input, &mut writer).context("copying the backup into place")?;
        writer.flush().context("flushing the staged backup")?;
        let out = writer.into_inner().map_err(|e| anyhow::anyhow!("flushing: {e}"))?;
        out.sync_all().context("fsync the staged backup")?;
        Ok(())
    })();
    if let Err(e) = copy {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = finish_atomic(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("installing {}", dest.display()));
    }
    Ok(())
}

fn get_to_file_blocking(src: &Path, dst: &Path) -> anyhow::Result<Option<u64>> {
    let mut input = match std::fs::File::open(src) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("open {}", src.display())),
    };
    let out = std::fs::File::create(dst).with_context(|| format!("create {}", dst.display()))?;
    let mut writer = std::io::BufWriter::new(out);
    let written = std::io::copy(&mut input, &mut writer)
        .with_context(|| format!("copying {}", src.display()))?;
    let out = writer.into_inner().map_err(|e| anyhow::anyhow!("flushing: {e}"))?;
    out.sync_all().context("fsync the restored backup")?;
    Ok(Some(written))
}

fn delete_blocking(base: &Path, path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
    }
    // Prune the now-empty <site> directory, but never `base`. `remove_dir`
    // only removes an empty directory, so a directory still holding other
    // objects is left alone (the error is ignored).
    if let Some(parent) = path.parent()
        && parent != base
        && parent.starts_with(base)
    {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

fn list_blocking(base: &Path, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
    // Walk from the directory portion of the prefix (everything up to the last
    // '/'), then filter by the full string prefix — matching S3's semantics
    // even when the prefix ends mid-component.
    let dir_part = match prefix.rfind('/') {
        Some(i) => &prefix[..=i],
        None => "",
    };
    let root = {
        let mut p = base.to_path_buf();
        for part in dir_part.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                continue;
            }
            p.push(part);
        }
        p
    };
    let mut out = Vec::new();
    walk_files(&root, base, &mut out)?;
    out.retain(|m| m.key.starts_with(prefix));
    Ok(out)
}

/// Recurse `dir`, pushing one [`ObjectMeta`] per regular file, keyed by its
/// `/`-normalised path relative to `base`.
fn walk_files(dir: &Path, base: &Path, out: &mut Vec<ObjectMeta>) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("reading an entry of {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry.file_type().with_context(|| format!("stat {}", path.display()))?;
        if file_type.is_dir() {
            walk_files(&path, base, out)?;
        } else if file_type.is_file() {
            let key = relative_key(base, &path)?;
            let size = entry.metadata().map_or(0, |m| m.len());
            out.push(ObjectMeta { key, size, last_modified: String::new() });
        }
    }
    Ok(())
}

/// `path` relative to `base`, joined with forward slashes (the object-key
/// shape S3 uses), so the key matches what [`BackupStore::S3`] would list.
fn relative_key(base: &Path, path: &Path) -> anyhow::Result<String> {
    let rel = path
        .strip_prefix(base)
        .with_context(|| format!("{} is not under {}", path.display(), base.display()))?;
    let mut key = String::new();
    for component in rel.components() {
        if let Component::Normal(part) = component {
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(&part.to_string_lossy());
        }
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> BackupStore {
        BackupStore::Local(LocalBackupStore::new(dir))
    }

    /// The full round trip a backup tick + retention sweep performs against a
    /// local directory: put two timestamped objects → list returns both with
    /// the S3 key shape → get round-trips the bytes → delete removes one and
    /// prunes nothing shared.
    #[tokio::test]
    async fn local_round_trip_put_list_get_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());

        // Two source files with distinct bytes.
        let src_a = dir.path().join("a.src");
        let src_b = dir.path().join("b.src");
        std::fs::write(&src_a, b"alpha contents").unwrap();
        std::fs::write(&src_b, vec![0x42u8; 4096]).unwrap();

        // Keys with the exact S3 shape the retention/timestamp code parses.
        let key_a = "ephpm/db/blog/20260101T000000Z.db.gz";
        let key_b = "ephpm/db/blog/20260102T000000Z.db.gz";
        store.put_file(key_a, &src_a).await.unwrap();
        store.put_file(key_b, &src_b).await.unwrap();

        // On-disk layout mirrors the key.
        assert!(dir.path().join("ephpm/db/blog/20260101T000000Z.db.gz").is_file());
        // No stray temp files left behind.
        let leftover: Vec<_> = std::fs::read_dir(dir.path().join("ephpm/db/blog"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftover.is_empty(), "temp files leaked: {leftover:?}");

        // List returns both with relative, forward-slash keys.
        let mut listed = store.list("ephpm/db/blog/").await.unwrap();
        listed.sort_by(|a, b| a.key.cmp(&b.key));
        let keys: Vec<&str> = listed.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, [key_a, key_b]);
        assert_eq!(listed[0].size, "alpha contents".len() as u64);
        assert_eq!(listed[1].size, 4096);

        // Get round-trips the bytes.
        let out = dir.path().join("fetched.bin");
        let n = store.get_to_file(key_b, &out).await.unwrap();
        assert_eq!(n, Some(4096));
        assert_eq!(std::fs::read(&out).unwrap(), vec![0x42u8; 4096]);

        // A missing key is Ok(None), like S3.
        assert_eq!(store.get_to_file("ephpm/db/blog/missing.db.gz", &out).await.unwrap(), None);

        // Delete one; the other survives, and the shared <site> dir survives.
        store.delete(key_a).await.unwrap();
        assert!(!dir.path().join("ephpm/db/blog/20260101T000000Z.db.gz").exists());
        assert!(dir.path().join("ephpm/db/blog/20260102T000000Z.db.gz").is_file());
        let remaining = store.list("ephpm/db/blog/").await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].key, key_b);

        // Deleting an absent key is success.
        store.delete(key_a).await.unwrap();
    }

    /// Deleting the last object of a site prunes the empty `<site>` directory,
    /// but never the base directory.
    #[tokio::test]
    async fn delete_prunes_empty_site_dir_but_not_base() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let src = dir.path().join("x.src");
        std::fs::write(&src, b"x").unwrap();
        let key = "ephpm/db/lonely/20260101T000000Z.db.gz";
        store.put_file(key, &src).await.unwrap();
        store.delete(key).await.unwrap();
        assert!(!dir.path().join("ephpm/db/lonely").exists(), "empty site dir pruned");
        assert!(dir.path().exists(), "base is never pruned");
    }

    /// `list` returns only files matching the string prefix, recursively,
    /// ignoring sibling sites — the behaviour `list_site_backups` relies on.
    #[tokio::test]
    async fn list_is_recursive_and_prefix_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let src = dir.path().join("s");
        std::fs::write(&src, b"s").unwrap();
        for key in [
            "ephpm/db/blog/20260101T000000Z.db.gz",
            "ephpm/db/blog/20260102T000000Z.db.gz",
            "ephpm/db/shop/20260101T000000Z.db.gz",
        ] {
            store.put_file(key, &src).await.unwrap();
        }
        let blog = store.list("ephpm/db/blog/").await.unwrap();
        assert_eq!(blog.len(), 2, "only blog's objects");
        assert!(blog.iter().all(|m| m.key.starts_with("ephpm/db/blog/")));
        // Listing a missing directory is empty, not an error.
        assert_eq!(store.list("ephpm/db/ghost/").await.unwrap().len(), 0);
    }

    #[test]
    fn object_and_dir_uris_render() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let uri = store.object_uri("ephpm/db/blog/20260101T000000Z.db.gz");
        assert!(uri.ends_with("20260101T000000Z.db.gz"), "{uri}");
        assert!(store.status_bucket().is_none());
        assert!(store.status_endpoint().is_none());
        assert_eq!(store.status_dir().as_deref(), Some(dir.path().display().to_string().as_str()));
    }
}

//! Persistent immutable baseline entries, published only under cross-process leases.
//! Fixed lock shards avoid unbounded lock-file accumulation and stale PID locks.
use super::BaselineCompiler;
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) const POLICY: &str = "sonar-baseline-cache-v1";
const OWNER: &str = "sonar-baseline-cache-layout-v1";
const SHARDS: u8 = 64;
const DEFAULT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const DEFAULT_ENTRIES: usize = 8;

#[derive(Clone)]
pub(super) struct Config {
    pub root: PathBuf,
    pub max_bytes: u64,
    pub max_entries: usize,
    pub compiler: bool,
}
impl Config {
    pub fn load() -> Result<Self> {
        let root = if let Some(root) = std::env::var_os("SONAR_BASELINE_CACHE") {
            PathBuf::from(root)
        } else {
            PathBuf::from(
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .context("set SONAR_BASELINE_CACHE when no home directory is available")?,
            )
            .join(".sonar/baselines")
        };
        let max_bytes = std::env::var("SONAR_BASELINE_CACHE_MAX_BYTES")
            .ok()
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(DEFAULT_BYTES);
        let max_entries = std::env::var("SONAR_BASELINE_CACHE_MAX_ENTRIES")
            .ok()
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(DEFAULT_ENTRIES);
        ensure!(
            max_bytes > 0 && max_entries > 0,
            "baseline cache limits must be positive"
        );
        Ok(Self {
            root: std::path::absolute(root)?,
            max_bytes,
            max_entries,
            compiler: std::env::var_os("SONAR_BASELINE_CACHE_COMPILER").is_none_or(|v| v != "0"),
        })
    }
    pub fn initialize(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let marker = self.root.join("owner");
        if !marker.exists() {
            ensure!(
                fs::read_dir(&self.root)?.all(|entry| entry.is_ok_and(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".sonar-baseline-marker-"))),
                "baseline cache directory is not empty and has no ownership marker"
            );
            let mut file = tempfile::Builder::new()
                .prefix(".sonar-baseline-marker-")
                .tempfile_in(&self.root)?;
            file.write_all(OWNER.as_bytes())?;
            file.flush()?;
            match file.persist_noclobber(&marker) {
                Ok(_) => {}
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.error.into()),
            }
        }
        self.check_owner()?;
        for name in ["entries", "locks"] {
            let path = self.root.join(name);
            fs::create_dir_all(&path)?;
            ensure!(
                !fs::symlink_metadata(path)?.file_type().is_symlink(),
                "cache layout directories must not be symlinks"
            );
        }
        Ok(())
    }
    pub fn check_owner(&self) -> Result<()> {
        ensure!(
            fs::read_to_string(self.root.join("owner"))? == OWNER,
            "unrecognized baseline cache ownership marker"
        );
        Ok(())
    }
    pub fn path(&self, key: &str) -> PathBuf {
        self.root.join("entries").join(key)
    }
}

pub(super) struct Lease {
    _connection: Connection,
}
fn shard(key: &str) -> Result<u8> {
    ensure!(valid_key(key), "invalid baseline cache key");
    Ok(u8::from_str_radix(&key[..2], 16)? % SHARDS)
}
fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn lock(config: &Config, slot: u8, wait: bool) -> Result<Option<Lease>> {
    let conn = Connection::open(config.root.join("locks").join(format!("{slot:02x}.sqlite")))?;
    conn.busy_timeout(if wait {
        Duration::from_secs(600)
    } else {
        Duration::ZERO
    })?;
    let result =
        conn.execute_batch("CREATE TABLE IF NOT EXISTS mutex (id INTEGER); BEGIN EXCLUSIVE;");
    match result {
        Ok(()) => Ok(Some(Lease { _connection: conn })),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if !wait
                && matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}
pub(super) fn leases(config: &Config, keys: &[&str]) -> Result<Vec<Lease>> {
    let slots = keys
        .iter()
        .map(|k| shard(k))
        .collect::<Result<BTreeSet<_>>>()?;
    slots
        .into_iter()
        .map(|slot| lock(config, slot, true)?.context("baseline lease unavailable"))
        .collect()
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ExternalInput {
    pub path: PathBuf,
    pub digest: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub policy: String,
    pub key: String,
    pub repository: String,
    pub commit: String,
    pub mode: String,
    pub source_digest: String,
    pub index_digest: String,
    pub external: Vec<ExternalInput>,
    pub compilers: Vec<BaselineCompiler>,
    pub excluded: Vec<String>,
    pub created_ms: u64,
    pub last_used_ms: u64,
    pub bytes: u64,
}
pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(super) fn file_digest(path: &Path) -> Result<String> {
    let before = fs::metadata(path)?;
    let stamp = crate::semantic::inputs::stamp(path)?;
    let mut hash = blake3::Hasher::new();
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let after = fs::metadata(path)?;
    ensure!(
        before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && stamp == crate::semantic::inputs::stamp(path)?,
        "cache input changed while hashing"
    );
    Ok(hash.finalize().to_hex().to_string())
}
fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_owned()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symlink in baseline cache input: {}",
            path.display()
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        } else if metadata.is_file() {
            files.push(path);
        } else {
            anyhow::bail!("unsupported cache input type");
        }
    }
    files.sort();
    Ok(files)
}
pub(super) fn tree_digest(root: &Path) -> Result<String> {
    let mut hash = blake3::Hasher::new();
    let paths = files(root)?;
    for path in &paths {
        let relative = if root.is_file() {
            Path::new("")
        } else {
            path.strip_prefix(root)?
        };
        hash.update(relative.as_os_str().as_encoded_bytes());
        hash.update(b"\0");
        hash.update(file_digest(path)?.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hash.update(&(fs::metadata(path)?.permissions().mode() & 0o111).to_le_bytes());
        }
    }
    ensure!(
        paths == files(root)?,
        "cache input tree changed while hashing"
    );
    Ok(hash.finalize().to_hex().to_string())
}
fn bytes(root: &Path) -> Result<u64> {
    files(root)?.into_iter().try_fold(0u64, |total, p| {
        Ok(total.saturating_add(fs::metadata(p)?.len()))
    })
}
pub(super) fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;
    for file in files(source)? {
        let relative = file.strip_prefix(source)?;
        let target = target.join(relative);
        fs::create_dir_all(target.parent().unwrap())?;
        fs::copy(file, target)?;
    }
    Ok(())
}
fn published(config: &Config, key: &str) -> Result<Option<Manifest>> {
    let path = config.path(key);
    if !path.is_dir() || fs::symlink_metadata(&path)?.file_type().is_symlink() {
        return Ok(None);
    }
    let ready = path.join("ready.json");
    if !ready.is_file() {
        return Ok(None);
    }
    let manifest: Manifest = match serde_json::from_slice(&fs::read(ready)?) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if manifest.key != key {
        return Ok(None);
    }
    Ok(Some(manifest))
}
pub(super) fn read(config: &Config, key: &str) -> Result<Option<Manifest>> {
    Ok(published(config, key)?.filter(|m| m.policy == POLICY))
}
pub(super) fn valid(config: &Config, manifest: &Manifest) -> Result<bool> {
    let path = config.path(&manifest.key);
    Ok(tree_digest(&path.join("source"))? == manifest.source_digest
        && file_digest(&path.join("index.sqlite"))? == manifest.index_digest
        && manifest
            .external
            .iter()
            .all(|input| tree_digest(&input.path).is_ok_and(|digest| digest == input.digest)))
}
pub(super) fn write(config: &Config, manifest: &Manifest) -> Result<()> {
    let path = config.path(&manifest.key);
    let mut file = tempfile::NamedTempFile::new_in(&path)?;
    serde_json::to_writer(&mut file, manifest)?;
    file.flush()?;
    file.persist(path.join("ready.json"))?;
    Ok(())
}
pub(super) fn publish(config: &Config, mut manifest: Manifest) -> Result<()> {
    for _ in 0..4 {
        write(config, &manifest)?;
        let actual = bytes(&config.path(&manifest.key))?;
        if actual == manifest.bytes {
            return Ok(());
        }
        manifest.bytes = actual;
    }
    write(config, &manifest)
}
pub(super) fn remove(config: &Config, key: &str) -> Result<()> {
    ensure!(valid_key(key), "invalid cache key");
    let path = config.path(key);
    if path.exists() {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            fs::remove_file(path)?;
        } else {
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct BaselineCacheEntry {
    pub key: String,
    pub repository: Option<String>,
    pub commit: Option<String>,
    pub mode: Option<String>,
    pub state: String,
    pub bytes: u64,
    pub created_ms: Option<u64>,
    pub last_used_ms: Option<u64>,
}
#[derive(Debug, Serialize)]
pub struct BaselineCacheList {
    pub root: PathBuf,
    pub max_bytes: u64,
    pub max_entries: usize,
    pub total_bytes: u64,
    pub over_limit: bool,
    pub entries: Vec<BaselineCacheEntry>,
}
#[derive(Debug, Serialize)]
pub struct BaselineCachePrune {
    pub removed: Vec<String>,
    pub freed_bytes: u64,
    pub protected_entries: usize,
    pub total_bytes: u64,
    pub entries_remaining: usize,
    pub over_limit: bool,
}
fn list(config: &Config) -> Result<BaselineCacheList> {
    let mut entries = Vec::new();
    let directory = config.root.join("entries");
    if config.root.exists() {
        config.check_owner()?;
    }
    if directory.is_dir() {
        ensure!(
            !fs::symlink_metadata(&directory)?.file_type().is_symlink(),
            "cache entries directory must not be a symlink"
        );
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let key = entry.file_name().to_string_lossy().into_owned();
            if !valid_key(&key) {
                continue;
            }
            let manifest = published(config, &key)?;
            let size = manifest
                .as_ref()
                .map_or_else(|| bytes(&entry.path()).unwrap_or(0), |m| m.bytes);
            entries.push(BaselineCacheEntry {
                key,
                repository: manifest.as_ref().map(|m| m.repository.clone()),
                commit: manifest.as_ref().map(|m| m.commit.clone()),
                mode: manifest.as_ref().map(|m| m.mode.clone()),
                state: if manifest.as_ref().is_some_and(|m| m.policy == POLICY) {
                    "ready"
                } else if manifest.is_some() {
                    "obsolete"
                } else {
                    "incomplete"
                }
                .into(),
                bytes: size,
                created_ms: manifest.as_ref().map(|m| m.created_ms),
                last_used_ms: manifest.as_ref().map(|m| m.last_used_ms),
            });
        }
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(BaselineCacheList {
        root: config.root.clone(),
        max_bytes: config.max_bytes,
        max_entries: config.max_entries,
        total_bytes: entries.iter().map(|e| e.bytes).sum(),
        over_limit: entries.len() > config.max_entries
            || entries.iter().map(|e| e.bytes).sum::<u64>() > config.max_bytes,
        entries,
    })
}
pub(super) fn prune(config: &Config, all: bool) -> Result<BaselineCachePrune> {
    let mut listing = list(config)?;
    let mut total = listing.total_bytes;
    let mut remaining = listing.entries.len();
    listing
        .entries
        .sort_by(|a, b| (a.last_used_ms, &a.key).cmp(&(b.last_used_ms, &b.key)));
    let mut report = BaselineCachePrune {
        removed: Vec::new(),
        freed_bytes: 0,
        protected_entries: 0,
        total_bytes: total,
        entries_remaining: remaining,
        over_limit: false,
    };
    for entry in listing.entries {
        if !all
            && total <= config.max_bytes
            && remaining <= config.max_entries
            && entry.state == "ready"
        {
            continue;
        }
        let Some(_guard) = lock(config, shard(&entry.key)?, false)? else {
            report.protected_entries += 1;
            continue;
        };
        // A reader may have touched this entry since enumeration. Recheck under its lease.
        let current = read(config, &entry.key)?;
        if !all
            && current
                .as_ref()
                .is_some_and(|m| Some(m.last_used_ms) != entry.last_used_ms)
        {
            continue;
        }
        remove(config, &entry.key)?;
        total = total.saturating_sub(entry.bytes);
        remaining = remaining.saturating_sub(1);
        report.freed_bytes = report.freed_bytes.saturating_add(entry.bytes);
        report.removed.push(entry.key);
    }
    report.total_bytes = total;
    report.entries_remaining = remaining;
    report.over_limit = total > config.max_bytes || remaining > config.max_entries;
    Ok(report)
}
pub fn baseline_cache_list() -> Result<BaselineCacheList> {
    list(&Config::load()?)
}
pub fn baseline_cache_prune(
    all: bool,
    max_bytes: Option<u64>,
    max_entries: Option<usize>,
) -> Result<BaselineCachePrune> {
    let mut config = Config::load()?;
    if let Some(n) = max_bytes {
        ensure!(n > 0, "max_bytes must be positive");
        config.max_bytes = n;
    }
    if let Some(n) = max_entries {
        ensure!(n > 0, "max_entries must be positive");
        config.max_entries = n;
    }
    if !config.root.exists() {
        return prune(&config, all);
    }
    config.initialize()?;
    prune(&config, all)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(root: &Path) -> Config {
        let config = Config {
            root: root.join("cache"),
            max_bytes: 1024 * 1024,
            max_entries: 8,
            compiler: true,
        };
        config.initialize().unwrap();
        config
    }
    fn key(n: u8) -> String {
        format!("{n:02x}{:062x}", n as u64 + 1)
    }
    fn entry(config: &Config, n: u8) -> Manifest {
        let key = key(n);
        let path = config.path(&key);
        fs::create_dir_all(path.join("source")).unwrap();
        fs::write(path.join("source/lib.rs"), "fn value() {}\n").unwrap();
        fs::write(path.join("index.sqlite"), b"fixture").unwrap();
        Manifest {
            policy: POLICY.into(),
            key,
            repository: "fixture".into(),
            commit: "commit".into(),
            mode: "syntax".into(),
            source_digest: tree_digest(&path.join("source")).unwrap(),
            index_digest: file_digest(&path.join("index.sqlite")).unwrap(),
            external: Vec::new(),
            compilers: Vec::new(),
            excluded: Vec::new(),
            created_ms: 1,
            last_used_ms: n as u64,
            bytes: 0,
        }
    }
    #[test]
    fn publication_integrity_and_external_content_are_validated() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        let mut manifest = entry(&config, 1);
        assert!(read(&config, &manifest.key).unwrap().is_none());
        let dependency = temp.path().join("dependency");
        fs::create_dir(&dependency).unwrap();
        fs::write(dependency.join("lib.rs"), "old").unwrap();
        manifest.external.push(ExternalInput {
            path: dependency.clone(),
            digest: tree_digest(&dependency).unwrap(),
        });
        publish(&config, manifest.clone()).unwrap();
        let saved = read(&config, &manifest.key).unwrap().unwrap();
        assert!(valid(&config, &saved).unwrap());
        assert_eq!(saved.bytes, bytes(&config.path(&saved.key)).unwrap());
        fs::write(
            config.path(&manifest.key).join("source/lib.rs"),
            "corrupted",
        )
        .unwrap();
        assert!(!valid(&config, &saved).unwrap());
        fs::write(
            config.path(&manifest.key).join("source/lib.rs"),
            "fn value() {}\n",
        )
        .unwrap();
        assert!(valid(&config, &saved).unwrap());
        fs::write(dependency.join("new.rs"), "new dependency input").unwrap();
        assert!(!valid(&config, &saved).unwrap());
        fs::remove_file(dependency.join("new.rs")).unwrap();
        fs::write(config.path(&manifest.key).join("index.sqlite"), b"broken").unwrap();
        assert!(!valid(&config, &saved).unwrap());
    }
    #[test]
    fn cleanup_respects_leases_and_reports_active_quota_exceptions() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = config(temp.path());
        for n in [1, 2] {
            publish(&config, entry(&config, n)).unwrap();
        }
        let held = leases(&config, &[&key(1)]).unwrap();
        let report = prune(&config, true).unwrap();
        assert_eq!(report.removed, vec![key(2)]);
        assert_eq!(report.protected_entries, 1);
        config.max_bytes = 1;
        let report = prune(&config, false).unwrap();
        assert!(report.over_limit);
        assert_eq!(report.protected_entries, 1);
        drop(held);
        let report = prune(&config, false).unwrap();
        assert!(!report.over_limit);
        assert_eq!(report.entries_remaining, 0);
    }
    #[test]
    fn obsolete_and_incomplete_entries_are_recoverable_and_lru_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = config(temp.path());
        let mut old = entry(&config, 1);
        old.policy = "old-analysis-policy".into();
        publish(&config, old).unwrap();
        assert!(read(&config, &key(1)).unwrap().is_none());
        assert_eq!(list(&config).unwrap().entries[0].state, "obsolete");
        entry(&config, 2);
        let report = prune(&config, false).unwrap();
        assert_eq!(report.removed.len(), 2);
        for n in [3, 4, 5] {
            publish(&config, entry(&config, n)).unwrap();
        }
        config.max_entries = 1;
        let report = prune(&config, false).unwrap();
        assert_eq!(report.entries_remaining, 1);
        assert_eq!(list(&config).unwrap().entries[0].key, key(5));
    }
    #[test]
    fn concurrent_reader_waits_until_the_completed_entry_is_published() {
        let temp = tempfile::tempdir().unwrap();
        let config = config(temp.path());
        let manifest = entry(&config, 1);
        let guard = leases(&config, &[&manifest.key]).unwrap();
        let other = config.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            send.send(()).unwrap();
            let _lease = leases(&other, &[&key(1)]).unwrap();
            read(&other, &key(1)).unwrap().unwrap().source_digest
        });
        receive.recv().unwrap();
        assert!(
            lock(&config, shard(&manifest.key).unwrap(), false)
                .unwrap()
                .is_none()
        );
        publish(&config, manifest.clone()).unwrap();
        drop(guard);
        assert_eq!(reader.join().unwrap(), manifest.source_digest);
    }
    #[test]
    fn inspection_and_pruning_do_not_adopt_unowned_directories() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            root: temp.path().join("unowned"),
            max_bytes: 1,
            max_entries: 1,
            compiler: true,
        };
        fs::create_dir(&config.root).unwrap();
        fs::write(config.root.join("important"), "preserve").unwrap();
        assert!(config.initialize().is_err());
        assert!(prune(&config, true).is_err());
        assert_eq!(
            fs::read_to_string(config.root.join("important")).unwrap(),
            "preserve"
        );
    }
}

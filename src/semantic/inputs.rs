//! Content hashes shared by compiler input manifests.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) type FileStamp = (u64, u64, u64, i64, i64, i64, i64);

/// Persist only precise identities: ctime detects content changes even if mtime is restored.
pub(crate) fn stamp(path: &Path) -> Result<Option<FileStamp>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || (metadata.ctime_nsec() == 0 && metadata.mtime_nsec() == 0) {
            return Ok(None);
        }
        Ok(Some((
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

pub(crate) fn hash(path: &Path) -> Result<Option<String>> {
    #[cfg(unix)]
    {
        use std::{
            collections::BTreeMap,
            os::unix::fs::MetadataExt,
            sync::{Mutex, OnceLock},
        };
        type Stamp = (u64, u64, u64, i64, i64, i64, i64);
        static CACHE: OnceLock<Mutex<BTreeMap<PathBuf, (Stamp, String)>>> = OnceLock::new();
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if metadata.is_dir() {
            return Ok(None);
        }
        let stamp = |m: &fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        let identity = stamp(&metadata);
        // ctime also detects equal-length writes followed by restored mtime.
        if metadata.ctime_nsec() != 0 || metadata.mtime_nsec() != 0 {
            let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
            if let Some((old, hash)) = cache
                .lock()
                .map_err(|_| anyhow::anyhow!("analysis cache poisoned"))?
                .get(path)
                && *old == identity
            {
                return Ok(Some(hash.clone()));
            }
            let hash = format!("{:x}", Sha256::digest(fs::read(path)?));
            ensure!(
                stamp(&fs::metadata(path)?) == identity,
                "analysis input changed while hashing"
            );
            let mut entries = cache
                .lock()
                .map_err(|_| anyhow::anyhow!("analysis cache poisoned"))?;
            if entries.len() > 16384 {
                entries.clear();
            }
            entries.insert(path.into(), (identity, hash.clone()));
            return Ok(Some(hash));
        }
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(format!("{:x}", Sha256::digest(bytes)))),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => {
            Err(error).with_context(|| format!("reading analysis input {}", path.display()))
        }
    }
}

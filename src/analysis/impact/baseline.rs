//! Isolated baseline preparation. No checkout, ref, index, or worktree metadata is changed.
use super::{BaselineCompiler, BaselineInfo, VersionedDeclaration, cache, cache_inputs, git};
use crate::{explore, store};
use anyhow::Result;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

struct CacheUse {
    config: cache::Config,
    leases: Vec<cache::Lease>,
    discard: BTreeSet<String>,
}
impl Drop for CacheUse {
    fn drop(&mut self) {
        for key in &self.discard {
            let _ = cache::remove(&self.config, key);
        }
        self.leases.clear();
        let _ = cache::prune(&self.config, false);
    }
}

pub(super) struct Prepared {
    _temporary: Option<tempfile::TempDir>,
    _cache: Option<CacheUse>,
    pub db: PathBuf,
    pub compilers: Vec<BaselineCompiler>,
    pub excluded: Vec<String>,
}

fn resources(current: &explore::Snapshot<'_>) -> Result<crate::ResourcePolicy> {
    Ok(
        if store::metadata(current.conn, "resources")?.as_deref() == Some("yaml-v1") {
            crate::ResourcePolicy::Yaml
        } else {
            crate::ResourcePolicy::None
        },
    )
}
fn syntax(
    directory: &Path,
    root: &Path,
    commit: &str,
    current: &explore::Snapshot<'_>,
) -> Result<Vec<String>> {
    let checkout = directory.join("source");
    std::fs::create_dir_all(&checkout)?;
    let excluded = git::materialize(root, commit, &checkout)?;
    let db = directory.join("index.sqlite");
    crate::index_with_resources(&checkout, &db, crate::Chunking::Lines, resources(current)?)?;
    crate::index_navigation(&db)?;
    Ok(excluded)
}
fn indexed_languages(db: &Path) -> Result<BTreeSet<String>> {
    let conn = store::open(db, false)?;
    Ok(store::file_hashes(&conn)?
        .keys()
        .filter_map(|p| crate::source_language(p).map(str::to_owned))
        .collect())
}
fn resolve(
    directory: &Path,
    current: &explore::Snapshot<'_>,
    requested: bool,
) -> Result<Vec<BaselineCompiler>> {
    let db = directory.join("index.sqlite");
    let languages = indexed_languages(&db)?;
    let mut compilers = Vec::new();
    for adapter in crate::languages::ADAPTERS {
        if !languages.contains(adapter.id()) {
            continue;
        }
        let (status, error) = if !requested {
            ("not_requested", None)
        } else {
            match adapter.resolve_baseline(&db, current.conn, directory) {
                Ok(true) => ("prepared", None),
                Ok(false) => (
                    "unavailable",
                    Some("No baseline resolver settings or provider are available.".into()),
                ),
                Err(error) => (
                    "failed",
                    Some(
                        format!("{error:#}")
                            .replace(directory.to_string_lossy().as_ref(), "<baseline>")
                            .chars()
                            .take(2000)
                            .collect(),
                    ),
                ),
            }
        };
        compilers.push(BaselineCompiler {
            language: adapter.id().into(),
            status: status.into(),
            error,
        });
    }
    Ok(compilers)
}
fn clone_syntax(source: &Path, destination: &Path) -> Result<()> {
    cache::copy_tree(&source.join("source"), &destination.join("source"))?;
    std::fs::copy(
        source.join("index.sqlite"),
        destination.join("index.sqlite"),
    )?;
    let conn = store::open(&destination.join("index.sqlite"), true)?;
    conn.execute(
        "UPDATE metadata SET value=?1 WHERE key='root'",
        [destination
            .join("source")
            .canonicalize()?
            .to_string_lossy()
            .as_ref()],
    )?;
    Ok(())
}
fn loaded(config: &cache::Config, key: &str, resolved: bool) -> Result<Option<cache::Manifest>> {
    let manifest = cache::read(config, key)?;
    match manifest {
        Some(manifest)
            if cache::valid(config, &manifest).unwrap_or(false)
                && cache_inputs::reusable(
                    &config.path(key).join("index.sqlite"),
                    &config.path(key).join("source"),
                    &manifest.compilers,
                    resolved,
                )
                .unwrap_or(false) =>
        {
            let mut manifest = manifest;
            manifest.last_used_ms = cache::now();
            cache::write(config, &manifest)?;
            Ok(Some(manifest))
        }
        _ => Ok(None),
    }
}
fn manifest(
    config: &cache::Config,
    key: &str,
    repository: (&Path, &str),
    mode: &str,
    compilers: Vec<BaselineCompiler>,
    excluded: Vec<String>,
    external: Vec<cache::ExternalInput>,
) -> Result<cache::Manifest> {
    let path = config.path(key);
    let now = cache::now();
    Ok(cache::Manifest {
        policy: cache::POLICY.into(),
        key: key.into(),
        repository: repository.0.to_string_lossy().into_owned(),
        commit: repository.1.into(),
        mode: mode.into(),
        source_digest: cache::tree_digest(&path.join("source"))?,
        index_digest: cache::file_digest(&path.join("index.sqlite"))?,
        external,
        compilers,
        excluded,
        created_ms: now,
        last_used_ms: now,
        bytes: 0,
    })
}
fn ensure_syntax(
    config: &cache::Config,
    key: &str,
    root: &Path,
    commit: &str,
    current: &explore::Snapshot<'_>,
) -> Result<cache::Manifest> {
    if let Some(manifest) = loaded(config, key, false)? {
        return Ok(manifest);
    }
    cache::remove(config, key)?;
    std::fs::create_dir_all(config.path(key))?;
    let excluded = syntax(&config.path(key), root, commit, current)?;
    let compilers = resolve(&config.path(key), current, false)?;
    let manifest = manifest(
        config,
        key,
        (root, commit),
        "syntax",
        compilers,
        excluded,
        Vec::new(),
    )?;
    cache::publish(config, manifest.clone())?;
    Ok(manifest)
}

impl Prepared {
    pub fn new(
        root: &Path,
        commit: &str,
        current: &explore::Snapshot<'_>,
        requested: bool,
    ) -> Result<Self> {
        let config = match cache::Config::load().and_then(|config| {
            config.initialize()?;
            Ok(config)
        }) {
            Ok(config) => config,
            Err(_) => {
                let temporary = tempfile::Builder::new()
                    .prefix("sonar-baseline-")
                    .tempdir()?;
                let excluded = syntax(temporary.path(), root, commit, current)?;
                let compilers = resolve(temporary.path(), current, requested)?;
                return Ok(Self {
                    db: temporary.path().join("index.sqlite"),
                    compilers,
                    excluded,
                    _temporary: Some(temporary),
                    _cache: None,
                });
            }
        };
        let providers = crate::languages::ADAPTERS
            .iter()
            .map(|a| (a.id(), a.dependency_provider().map(|p| p.id())))
            .collect::<Vec<_>>();
        let key = explore::request((
            cache::POLICY,
            env!("CARGO_PKG_VERSION"),
            crate::store::LINES_EXTRACTION_VERSION,
            root,
            commit,
            store::metadata(current.conn, "resources")?,
            providers,
        ))?;
        let first = cache::leases(&config, &[&key])?;
        let syntax = ensure_syntax(&config, &key, root, commit, current)?;
        if !requested {
            return Ok(Self {
                db: config.path(&key).join("index.sqlite"),
                compilers: syntax.compilers,
                excluded: syntax.excluded,
                _temporary: None,
                _cache: Some(CacheUse {
                    config,
                    leases: first,
                    discard: BTreeSet::new(),
                }),
            });
        }
        let languages = indexed_languages(&config.path(&key).join("index.sqlite"))?;
        let identity = if config.compiler {
            cache_inputs::identity(current, &config.path(&key).join("source"), &languages)
        } else {
            Err(anyhow::anyhow!("compiler caching disabled"))
        };
        let identity = match identity {
            Ok(identity) => identity,
            Err(_) => {
                let temporary = tempfile::Builder::new()
                    .prefix("sonar-baseline-")
                    .tempdir()?;
                clone_syntax(&config.path(&key), temporary.path())?;
                let compilers = resolve(temporary.path(), current, true)?;
                return Ok(Self {
                    db: temporary.path().join("index.sqlite"),
                    compilers,
                    excluded: syntax.excluded,
                    _temporary: Some(temporary),
                    _cache: Some(CacheUse {
                        config,
                        leases: first,
                        discard: BTreeSet::new(),
                    }),
                });
            }
        };
        let compiled_key = explore::request((cache::POLICY, "compiler", &key, &identity))?;
        drop(first);
        // Always acquire all shards in ascending order, including hash-collision cases.
        let leases = cache::leases(&config, &[&key, &compiled_key])?;
        let mut use_guard = CacheUse {
            config: config.clone(),
            leases,
            discard: BTreeSet::new(),
        };
        let syntax = ensure_syntax(&config, &key, root, commit, current)?;
        if let Some(manifest) = loaded(&config, &compiled_key, true)?
            && cache_inputs::identity(
                current,
                &config.path(&compiled_key).join("source"),
                &languages,
            )
            .is_ok_and(|observed| observed == identity)
        {
            return Ok(Self {
                db: config.path(&compiled_key).join("index.sqlite"),
                compilers: manifest.compilers,
                excluded: manifest.excluded,
                _temporary: None,
                _cache: Some(use_guard),
            });
        }
        cache::remove(&config, &compiled_key)?;
        std::fs::create_dir_all(config.path(&compiled_key))?;
        use_guard.discard.insert(compiled_key.clone());
        clone_syntax(&config.path(&key), &config.path(&compiled_key))?;
        let anticipated = languages
            .iter()
            .map(|language| BaselineCompiler {
                language: language.clone(),
                status: "prepared".into(),
                error: None,
            })
            .collect::<Vec<_>>();
        let before_inputs = cache_inputs::dependencies(
            &config.path(&compiled_key).join("index.sqlite"),
            &anticipated,
        );
        let compilers = resolve(&config.path(&compiled_key), current, true)?;
        let path = config.path(&compiled_key);
        let cacheable = compilers.iter().all(|c| c.status != "failed")
            && cache::tree_digest(&path.join("source"))? == syntax.source_digest
            && cache_inputs::reusable(
                &path.join("index.sqlite"),
                &path.join("source"),
                &compilers,
                true,
            )
            .unwrap_or(false);
        if cacheable
            && let Ok(external) = cache_inputs::dependencies(&path.join("index.sqlite"), &compilers)
            && before_inputs
                .as_ref()
                .is_ok_and(|before| before == &external)
        {
            // Re-read tool/environment identity after preparation; do not publish a mixed generation.
            if cache_inputs::identity(current, &path.join("source"), &languages).is_ok_and(|now| {
                explore::request((cache::POLICY, "compiler", &key, now))
                    .ok()
                    .as_deref()
                    == Some(&compiled_key)
            }) {
                let manifest = manifest(
                    &config,
                    &compiled_key,
                    (root, commit),
                    "compiler",
                    compilers.clone(),
                    syntax.excluded.clone(),
                    external,
                )?;
                if cache::publish(&config, manifest).is_ok() {
                    use_guard.discard.remove(&compiled_key);
                }
            }
        }
        Ok(Self {
            db: path.join("index.sqlite"),
            compilers,
            excluded: syntax.excluded,
            _temporary: None,
            _cache: Some(use_guard),
        })
    }
    pub fn info(
        &self,
        s: &explore::Snapshot<'_>,
        commit: &str,
        requested: bool,
    ) -> Result<BaselineInfo> {
        let parse_error_files = s.conn.query_row(
            "SELECT count(*) FROM files WHERE parse_errors!=0",
            [],
            |r| r.get::<_, u32>(0),
        )? as usize;
        Ok(BaselineInfo {
            commit: commit.into(),
            syntax_snapshot: s.index.snapshot.clone(),
            compiler_requested: requested,
            compilers: self.compilers.clone(),
            excluded_entries: self.excluded.iter().take(20).cloned().collect(),
            excluded_entries_total: self.excluded.len(),
            parse_error_files,
            notices: crate::semantic::coverage_notices(s.conn, &s.index.languages)?,
        })
    }
}

#[derive(Serialize)]
pub(super) struct Declaration {
    pub location: VersionedDeclaration,
    pub source_hash: String,
    pub signature_hash: String,
    pub shape_hash: Option<String>,
}

pub(super) struct Catalog {
    pub declarations: BTreeMap<i64, Declaration>,
    pub invalid_files: BTreeSet<String>,
}

impl Catalog {
    pub fn load(s: &explore::Snapshot<'_>, snapshot: &str, commit: Option<&str>) -> Result<Self> {
        let mut declarations = BTreeMap::new();
        let mut invalid_files = BTreeSet::new();
        for file in s
            .sources
            .values()
            .filter(|f| crate::source_language(&f.path).is_some())
        {
            let parsed =
                crate::languages::extract(&file.path, &file.source, crate::Chunking::Lines)?;
            if parsed.has_errors {
                invalid_files.insert(file.path.clone());
                continue;
            }
            let mut stmt=s.conn.prepare("SELECT id,start_byte,end_byte,kind,symbol FROM locations WHERE file_id=(SELECT id FROM files WHERE path=?1) AND kind NOT IN ('file','documentation')")?;
            let ids = stmt
                .query_map([&file.path], |r| {
                    Ok((
                        (
                            r.get::<_, u32>(1)? as usize,
                            r.get::<_, u32>(2)? as usize,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<String>>(4)?,
                        ),
                        r.get::<_, i64>(0)?,
                    ))
                })?
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let mut offsets = vec![0];
            offsets.extend(
                file.source
                    .bytes()
                    .enumerate()
                    .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
            );
            let line = |byte: usize| offsets.partition_point(|start| *start <= byte).max(1);
            for location in parsed
                .locations
                .into_iter()
                .filter(|l| !matches!(l.kind, "file" | "documentation"))
            {
                let key = (
                    location.start_byte,
                    location.end_byte,
                    location.kind.to_owned(),
                    location.symbol.clone(),
                );
                let Some(&id) = ids.get(&key) else {
                    continue;
                };
                let source = &file.source[location.start_byte..location.end_byte];
                let shape_hash = if location.name_start >= location.start_byte
                    && location.name_end <= location.end_byte
                    && location.name_start < location.name_end
                    && location.name_end - location.name_start < source.len()
                {
                    let mut hash = blake3::Hasher::new();
                    hash.update(&file.source.as_bytes()[location.start_byte..location.name_start]);
                    hash.update(b"<declaration-name>");
                    hash.update(&file.source.as_bytes()[location.name_end..location.end_byte]);
                    Some(hash.finalize().to_hex().to_string())
                } else {
                    None
                };
                let stable = explore::request((
                    snapshot,
                    commit,
                    &s.index.snapshot,
                    &file.path,
                    location.kind,
                    &location.symbol,
                    location.start_byte,
                    location.end_byte,
                ))?;
                let full_signature = location.signature.trim();
                declarations.insert(
                    id,
                    Declaration {
                        location: VersionedDeclaration {
                            id: format!("decl:{stable}"),
                            snapshot: snapshot.into(),
                            commit: commit.map(str::to_owned),
                            path: file.path.clone(),
                            language: crate::source_language(&file.path).unwrap().into(),
                            symbol: location.symbol,
                            kind: location.kind.into(),
                            range: [
                                line(location.start_byte),
                                line(location.end_byte.saturating_sub(1).max(location.start_byte)),
                            ],
                            signature: full_signature.chars().take(2048).collect(),
                            signature_truncated: full_signature.chars().count() > 2048,
                        },
                        source_hash: blake3::hash(source.as_bytes()).to_hex().to_string(),
                        signature_hash: blake3::hash(full_signature.as_bytes())
                            .to_hex()
                            .to_string(),
                        shape_hash,
                    },
                );
            }
        }
        Ok(Self {
            declarations,
            invalid_files,
        })
    }
    pub fn overlap(
        &self,
        path: &str,
        ranges: impl Iterator<Item = (usize, usize)>,
    ) -> BTreeSet<i64> {
        let ranges: Vec<_> = ranges.filter(|(_, count)| *count > 0).collect();
        self.declarations
            .iter()
            .filter(|(_, d)| {
                d.location.path == path
                    && ranges.iter().any(|(start, count)| {
                        d.location.range[0] <= start.saturating_add(count - 1)
                            && d.location.range[1] >= *start
                    })
            })
            .map(|(id, _)| *id)
            .collect()
    }
    pub fn fingerprint(&self) -> Result<String> {
        let mut declarations = self.declarations.values().collect::<Vec<_>>();
        declarations.sort_by(|a, b| a.location.id.cmp(&b.location.id));
        explore::request((declarations, &self.invalid_files))
    }
}

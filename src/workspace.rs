use std::{io::Read, path::Path};

use anyhow::{Context, Result, ensure};
use ignore::WalkBuilder;

pub(crate) struct SourceFile {
    pub path: String,
    pub hash: String,
    pub source: String,
}

/// Scan the same source boundary for indexing and freshness checks. Symlinks are not followed.
pub(crate) fn scan_index(conn: &rusqlite::Connection, root: &Path) -> Result<Vec<SourceFile>> {
    let policy = match crate::store::metadata(conn, "resources")?.as_deref() {
        None | Some("none") => crate::ResourcePolicy::None,
        Some("yaml-v1") => crate::ResourcePolicy::Yaml,
        Some(value) => anyhow::bail!("unsupported resource policy: {value}"),
    };
    let languages = indexed_languages(conn)?;
    scan_languages(
        root,
        policy,
        &languages.iter().map(String::as_str).collect::<Vec<_>>(),
    )
}

fn indexed_languages(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    // Existing indexes retain the original source boundary until explicitly reindexed.
    let policy = crate::store::metadata(conn, "languages")?;
    let languages: Vec<_> = policy
        .as_deref()
        .unwrap_or("rust")
        .split(',')
        .map(str::to_owned)
        .collect();
    ensure!(
        languages
            .iter()
            .all(|l| crate::supported_languages().contains(&l.as_str())),
        "unsupported indexed language policy"
    );
    Ok(languages)
}

pub(crate) fn scan(root: &Path, resources: crate::ResourcePolicy) -> Result<Vec<SourceFile>> {
    scan_languages(root, resources, &crate::supported_languages())
}

fn walker(root: &Path) -> Result<ignore::Walk> {
    ensure!(
        root.is_dir(),
        "repository directory is unavailable: {}",
        root.display()
    );
    Ok(WalkBuilder::new(root)
        .require_git(false)
        .git_global(false)
        .parents(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|kind| kind.is_dir())
                || !matches!(
                    entry.file_name().to_str(),
                    Some(
                        "target"
                            | "vendor"
                            | "vendored"
                            | "node_modules"
                            | ".git"
                            | ".sonar"
                            | "venv"
                            | ".venv"
                            | "__pycache__"
                            | "site-packages"
                    )
                )
        })
        .build())
}

/// Detect supported source files outside an older index's language policy without
/// reading their contents or mutating its snapshot. Most indexes need no extra walk.
pub(crate) fn uncovered_language_paths(
    conn: &rusqlite::Connection,
    root: &Path,
) -> Result<Vec<String>> {
    let indexed = indexed_languages(conn)?;
    if crate::supported_languages()
        .iter()
        .all(|l| indexed.iter().any(|i| i == l))
    {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in walker(root)? {
        let entry = entry.context("detecting repository languages")?;
        if let Some(error) = entry.error() {
            anyhow::bail!("reading ignore rules: {error}");
        }
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && let Some(language) = entry.path().to_str().and_then(crate::source_language)
            && !indexed.iter().any(|l| l == language)
        {
            paths.push(
                entry
                    .path()
                    .strip_prefix(root)?
                    .to_str()
                    .context("source paths must be valid UTF-8")?
                    .replace(std::path::MAIN_SEPARATOR, "/"),
            );
        }
    }
    paths.sort();
    Ok(paths)
}

fn scan_languages(
    root: &Path,
    resources: crate::ResourcePolicy,
    languages: &[&str],
) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    for entry in walker(root)? {
        let entry = entry.context("walking repository")?;
        if let Some(error) = entry.error() {
            anyhow::bail!("reading ignore rules: {error}");
        }
        let extension = entry.path().extension().and_then(|ext| ext.to_str());
        let yaml =
            resources == crate::ResourcePolicy::Yaml && matches!(extension, Some("yaml" | "yml"));
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || (!yaml
                && extension != Some("md")
                && !entry
                    .path()
                    .to_str()
                    .and_then(crate::source_language)
                    .is_some_and(|language| languages.contains(&language)))
        {
            continue;
        }
        let path = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("source paths must be valid UTF-8")?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let bytes = if yaml {
            let mut bytes = Vec::new();
            std::fs::File::open(entry.path())?
                .take(256 * 1024 + 1)
                .read_to_end(&mut bytes)
                .with_context(|| format!("reading {path}"))?;
            ensure!(
                bytes.len() <= 256 * 1024,
                "YAML resource exceeds 256 KiB: {path}"
            );
            bytes
        } else {
            std::fs::read(entry.path()).with_context(|| format!("reading {path}"))?
        };
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let source = String::from_utf8(bytes).with_context(|| format!("{path} is not UTF-8"))?;
        files.push(SourceFile { path, hash, source });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

pub(crate) fn snapshot(files: &[SourceFile]) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sonar-rust-markdown-v2\0");
    for file in files {
        hash.update(file.path.as_bytes());
        hash.update(b"\0");
        hash.update(file.hash.as_bytes());
        hash.update(b"\0");
    }
    hash.finalize().to_hex().to_string()
}

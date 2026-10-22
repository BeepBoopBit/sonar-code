use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::path::Path;

/// Local build inputs, including Cargo manifests/lockfiles and hidden .cargo configuration.
/// External dependency contents and arbitrary build-script environment reads are outside this boundary.
pub(crate) fn semantic_inputs(root: &Path) -> Result<String> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .parents(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|kind| kind.is_dir())
                || !matches!(
                    entry.file_name().to_str(),
                    Some("target" | "node_modules" | ".git" | ".sonar" | ".venv" | "__pycache__")
                )
        })
        .build();
    for entry in walker {
        let entry = entry?;
        if let Some(error) = entry.error() {
            anyhow::bail!("reading semantic input ignore rules: {error}");
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("input paths must be UTF-8")?
            .to_owned();
        let bytes = std::fs::read(entry.path())?;
        files.push((relative, blake3::hash(&bytes)));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hash = blake3::Hasher::new();
    hash.update(b"sonar-local-analysis-inputs-v2\0");
    for (path, digest) in files {
        hash.update(path.as_bytes());
        hash.update(b"\0");
        hash.update(digest.as_bytes());
    }
    for name in [
        "RUSTUP_TOOLCHAIN",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
    ] {
        hash.update(name.as_bytes());
        hash.update(b"\0");
        hash.update(
            std::env::var_os(name)
                .unwrap_or_default()
                .as_encoded_bytes(),
        );
        hash.update(b"\0");
    }
    Ok(hash.finalize().to_hex().to_string())
}

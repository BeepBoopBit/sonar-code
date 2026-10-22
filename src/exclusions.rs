//! Explicit search exclusions use the same literal component boundary as scope.
use anyhow::{Context, Result, ensure};

pub(crate) fn normalize(paths: &[String]) -> Result<Vec<String>> {
    ensure!(paths.len() <= 64, "at most 64 exclusion paths are allowed");
    let mut result = Vec::new();
    for path in paths {
        let path = crate::search::normalize_scope(Some(path)).context("invalid exclusion path")?;
        ensure!(
            !path.is_empty(),
            "exclusion must name a file or directory, not the repository root"
        );
        result.push(path);
    }
    result.sort();
    result.dedup();
    Ok(result)
}

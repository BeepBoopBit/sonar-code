//! Continuations bind the query and its observed analysis state, not just an offset.
use anyhow::{Context, Result, ensure};

pub(super) fn offset(
    cursor: Option<&str>,
    request: &str,
    limit: usize,
    total: usize,
) -> Result<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    ensure!(cursor.len() <= 128, "invalid analysis cursor");
    let parts: Vec<_> = cursor.split(':').collect();
    ensure!(
        parts.len() == 3 && parts[0] == "sd2" && parts[1] == request,
        "analysis cursor belongs to another query, repository, snapshot, or analysis state; restart the query"
    );
    let offset: usize = parts[2].parse().context("invalid analysis cursor offset")?;
    ensure!(
        offset > 0 && offset < total && offset.is_multiple_of(limit),
        "invalid analysis cursor offset"
    );
    Ok(offset)
}

pub(super) fn next(request: &str, offset: usize, limit: usize, total: usize) -> Option<String> {
    let next = offset.saturating_add(limit);
    (next < total).then(|| format!("sd2:{request}:{next}"))
}

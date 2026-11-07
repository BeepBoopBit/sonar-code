//! Git is queried read-only with literal pathspecs and external diff/textconv disabled.
use super::DiffHunk;
use crate::{search, workspace::SourceFile};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{collections::BTreeMap, path::Path, process::Command};

#[derive(Debug, Clone, Serialize)]
pub(super) struct Change {
    pub path: String,
    pub status: String,
    pub hunks: Vec<DiffHunk>,
    patch_hash: String,
}

pub(super) struct Diff {
    pub base_commit: String,
    pub changes: Vec<Change>,
    pub fingerprint: String,
}

pub(super) fn output(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let result = Command::new("git")
        .args(["--no-pager", "--literal-pathspecs", "-C"])
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("running Git for impact analysis")?;
    ensure!(
        result.status.success(),
        "Git impact query failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(result.stdout)
}

fn nul_strings(bytes: &[u8]) -> Result<Vec<&str>> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| std::str::from_utf8(s).context("impact requires UTF-8 Git paths"))
        .collect()
}

fn count_range(token: &str, prefix: char) -> Result<(usize, usize)> {
    let range = token
        .strip_prefix(prefix)
        .context("invalid Git hunk range")?;
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    Ok((start.parse()?, count.parse()?))
}

fn hunks(bytes: &[u8]) -> Result<Vec<DiffHunk>> {
    let mut hunks = Vec::new();
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|line| line.starts_with(b"@@ -"))
    {
        // Only the ASCII range tokens matter; function context may use another encoding.
        let mut parts = line
            .split(u8::is_ascii_whitespace)
            .filter(|part| !part.is_empty());
        ensure!(
            parts.next() == Some(b"@@".as_slice()),
            "invalid Git hunk header"
        );
        let (old_start, old_lines) = count_range(
            std::str::from_utf8(parts.next().context("missing old hunk range")?)?,
            '-',
        )?;
        let (new_start, new_lines) = count_range(
            std::str::from_utf8(parts.next().context("missing new hunk range")?)?,
            '+',
        )?;
        ensure!(
            parts.next() == Some(b"@@".as_slice()),
            "invalid Git hunk header"
        );
        hunks.push(DiffHunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
        });
    }
    Ok(hunks)
}

pub(super) fn read(
    root: &Path,
    base: &str,
    scope: &str,
    sources: &BTreeMap<String, SourceFile>,
) -> Result<Diff> {
    let top = output(root, &["rev-parse", "--show-toplevel"])?;
    let top = std::str::from_utf8(&top)?.trim_end_matches('\n');
    ensure!(
        Path::new(top).canonicalize()? == root.canonicalize()?,
        "impact requires an index at the Git repository root"
    );
    let revision = format!("{base}^{{commit}}");
    let base_commit = String::from_utf8(output(
        root,
        &["rev-parse", "--verify", "--end-of-options", &revision],
    )?)?
    .trim()
    .to_owned();
    ensure!(
        [40, 64].contains(&base_commit.len()) && base_commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid resolved Git commit"
    );
    let status = output(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-status",
            "-z",
            &base_commit,
            "--",
        ],
    )?;
    let fields = nul_strings(&status)?;
    ensure!(fields.len().is_multiple_of(2), "invalid Git change listing");
    let mut changes = Vec::new();
    for pair in fields.as_chunks::<2>().0 {
        let (status, path) = (pair[0], pair[1]);
        if !search::in_scope(path, scope) {
            continue;
        }
        let patch = output(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--no-color",
                "--unified=0",
                "--inter-hunk-context=0",
                &base_commit,
                "--",
                path,
            ],
        )?;
        changes.push(Change {
            path: path.into(),
            status: status.into(),
            hunks: hunks(&patch)?,
            patch_hash: blake3::hash(&patch).to_hex().to_string(),
        });
    }
    let untracked = output(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in nul_strings(&untracked)? {
        if !search::in_scope(path, scope) {
            continue;
        }
        let (lines, hash) = sources
            .get(path)
            .map(|s| (s.source.lines().count(), s.hash.clone()))
            .unwrap_or((0, String::new()));
        changes.push(Change {
            path: path.into(),
            status: "untracked".into(),
            hunks: if lines > 0 {
                vec![DiffHunk {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 1,
                    new_lines: lines,
                }]
            } else {
                vec![]
            },
            patch_hash: hash,
        });
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    let fingerprint = crate::explore::request((&base_commit, &changes))?;
    Ok(Diff {
        base_commit,
        changes,
        fingerprint,
    })
}

/// Materialize regular tracked blobs without checkout hooks, filters, or worktree registration.
/// Symlinks and submodules are reported rather than followed.
pub(super) fn materialize(root: &Path, commit: &str, destination: &Path) -> Result<Vec<String>> {
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        path::Component,
        process::Stdio,
    };
    let tree = output(root, &["ls-tree", "-r", "-z", "--full-tree", commit])?;
    let mut entries = Vec::new();
    let mut excluded = Vec::new();
    for entry in tree.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let split = entry
            .iter()
            .position(|b| *b == b'\t')
            .context("invalid Git tree entry")?;
        let header = std::str::from_utf8(&entry[..split])?;
        let path = std::str::from_utf8(&entry[split + 1..])?;
        ensure!(
            Path::new(path)
                .components()
                .all(|p| matches!(p, Component::Normal(_)))
                && !Path::new(path)
                    .components()
                    .any(|p| p.as_os_str() == ".git"),
            "unsafe baseline tree path"
        );
        let parts: Vec<_> = header.split_whitespace().collect();
        ensure!(parts.len() == 3, "invalid Git tree metadata");
        if !matches!(parts[0], "100644" | "100755") || parts[1] != "blob" {
            excluded.push(path.to_owned());
            continue;
        }
        entries.push((path.to_owned(), parts[2].to_owned(), parts[0] == "100755"));
    }
    let mut child = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["cat-file", "--batch"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().context("baseline batch stdin missing")?;
    let ids: Vec<_> = entries.iter().map(|(_, id, _)| id.clone()).collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for id in ids {
            writeln!(input, "{id}")?;
        }
        Ok(())
    });
    let result = (|| -> Result<()> {
        let mut output = BufReader::new(
            child
                .stdout
                .take()
                .context("baseline batch stdout missing")?,
        );
        for (path, id, executable) in entries {
            let mut header = String::new();
            output.read_line(&mut header)?;
            let fields: Vec<_> = header.split_whitespace().collect();
            ensure!(
                fields.len() == 3 && fields[0] == id && fields[1] == "blob",
                "invalid baseline blob response"
            );
            let size: u64 = fields[2].parse()?;
            let target = destination.join(path);
            fs::create_dir_all(target.parent().unwrap())?;
            let mut file = fs::File::create(&target)?;
            ensure!(
                std::io::copy(&mut output.by_ref().take(size), &mut file)? == size,
                "truncated baseline blob"
            );
            let mut newline = [0];
            output.read_exact(&mut newline)?;
            ensure!(newline == *b"\n", "invalid baseline blob terminator");
            #[cfg(unix)]
            if executable {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
            }
            #[cfg(not(unix))]
            let _ = executable;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let written = writer
        .join()
        .map_err(|_| anyhow::anyhow!("baseline batch writer panicked"))?;
    result?;
    written?;
    ensure!(status.success(), "Git baseline blob export failed");
    Ok(excluded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_zero_length_and_default_ranges_without_reading_patch_content() {
        let result = hunks(
            b"diff --git a/x b/x\n@@ -0,0 +1,2 @@\n+one\n+two\n@@ -8 +9,0 @@ label\n-removed\n",
        )
        .unwrap();
        assert_eq!(
            result,
            vec![
                DiffHunk {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 1,
                    new_lines: 2
                },
                DiffHunk {
                    old_start: 8,
                    old_lines: 1,
                    new_start: 9,
                    new_lines: 0
                }
            ]
        );
    }
    #[test]
    fn non_utf8_function_context_does_not_hide_non_code_changes() {
        assert_eq!(hunks(b"@@ -1 +1 @@ \xff\xfe\n").unwrap().len(), 1);
    }
}

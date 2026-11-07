//! Resolver identity and dependency validation, separate from cache storage mechanics.
use super::cache::{self, ExternalInput};
use crate::{explore, languages, store};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

fn executable(command: &Path) -> Result<PathBuf> {
    if command.components().count() > 1 {
        return Ok(std::path::absolute(command)?.canonicalize()?);
    }
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let path = directory.join(command);
        if path.is_file() {
            return Ok(path.canonicalize()?);
        }
        #[cfg(windows)]
        for extension in std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
        {
            let candidate = directory.join(format!("{}{}", command.to_string_lossy(), extension));
            if candidate.is_file() {
                return Ok(candidate.canonicalize()?);
            }
        }
    }
    anyhow::bail!("baseline tool is unavailable: {}", command.display())
}

fn normalized_output(bytes: &[u8], source: &Path) -> serde_json::Value {
    let mut value = serde_json::from_slice(bytes)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned()));
    fn normalize(value: &mut serde_json::Value, source: &str) {
        match value {
            serde_json::Value::String(text) => *text = text.replace(source, "<baseline-source>"),
            serde_json::Value::Array(values) => {
                for value in values {
                    normalize(value, source);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    normalize(value, source);
                }
            }
            _ => {}
        }
    }
    normalize(&mut value, source.to_string_lossy().as_ref());
    value
}

pub(super) fn identity(
    current: &explore::Snapshot<'_>,
    source: &Path,
    languages: &BTreeSet<String>,
) -> Result<String> {
    let mut providers = Vec::new();
    for adapter in languages::ADAPTERS {
        if !languages.contains(adapter.id()) {
            continue;
        }
        let Some(profile) = adapter.baseline_cache_profile(current.conn, source)? else {
            providers.push(serde_json::json!({"language":adapter.id(),"settings":null}));
            continue;
        };
        let mut tools = Vec::new();
        for (command, args) in profile.tools {
            let path = executable(&command)?;
            let result = Command::new(&command)
                .args(args)
                .current_dir(source)
                .env("GOTOOLCHAIN", "local")
                .output()?;
            ensure!(
                result.status.success(),
                "baseline tool version check failed"
            );
            tools.push((
                path.clone(),
                cache::file_digest(&path)?,
                normalized_output(&result.stdout, source),
                normalized_output(&result.stderr, source),
            ));
        }
        providers.push(
            serde_json::json!({"language":adapter.id(),"settings":profile.settings,"tools":tools}),
        );
    }
    // Preserve arbitrary build-script variables without storing their values in manifests.
    // Session/navigation and cache-control variables do not affect compiler inputs.
    let mut environment = std::env::vars_os()
        .filter(|(name, _)| {
            !matches!(
                name.to_str(),
                Some(
                    "PWD"
                        | "OLDPWD"
                        | "SHLVL"
                        | "_"
                        | "SONAR_BASELINE_CACHE"
                        | "SONAR_BASELINE_CACHE_MAX_BYTES"
                        | "SONAR_BASELINE_CACHE_MAX_ENTRIES"
                        | "SONAR_BASELINE_CACHE_COMPILER"
                )
            )
        })
        .map(|(k, v)| (k.as_encoded_bytes().to_vec(), v.as_encoded_bytes().to_vec()))
        .collect::<Vec<_>>();
    environment.sort();
    explore::request((providers, environment))
}

pub(super) fn dependencies(
    db: &Path,
    prepared: &[super::BaselineCompiler],
) -> Result<Vec<ExternalInput>> {
    let mut paths = BTreeSet::new();
    for adapter in languages::ADAPTERS {
        if prepared
            .iter()
            .any(|c| c.language == adapter.id() && c.status == "prepared")
        {
            for path in adapter.baseline_cache_dependencies(db)? {
                paths.insert(path);
            }
        }
    }
    paths
        .into_iter()
        .map(|path| {
            Ok(ExternalInput {
                digest: cache::tree_digest(&path)?,
                path,
            })
        })
        .collect()
}

pub(super) fn reusable(
    db: &Path,
    source: &Path,
    compilers: &[super::BaselineCompiler],
    resolved: bool,
) -> Result<bool> {
    let conn = store::open(db, false)?;
    ensure!(
        store::metadata(&conn, "root")?.as_deref() == source.canonicalize()?.to_str(),
        "cached baseline index moved"
    );
    drop(conn);
    explore::read(db, |snapshot| {
        let _ = super::super::graph::load(snapshot)?;
        ensure!(
            snapshot.navigation_current,
            "cached navigation policy changed"
        );
        Ok(!resolved
            || compilers
                .iter()
                .filter(|c| c.status == "prepared")
                .all(|c| {
                    snapshot
                        .index
                        .languages
                        .iter()
                        .any(|l| l.language == c.language && l.semantic_freshness == "current")
                }))
    })
    .context("validating baseline analysis")
}

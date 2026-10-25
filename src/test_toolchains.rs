//! Compiler discovery shared by optional local tests and mandatory CI checks.
#![allow(dead_code)]

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn unavailable(variable: &str, message: &str) -> Option<PathBuf> {
    assert!(
        env::var_os("SONAR_REQUIRE_COMPILERS").as_deref() != Some("1".as_ref())
            && env::var_os(variable).is_none(),
        "{message}. Install the compiler or correct {variable}; compiler tests are required"
    );
    eprintln!("Skipping compiler integration: {message}. Set {variable} to enable it");
    None
}

fn executable(variable: &str, default: &str, version_argument: &str) -> Option<PathBuf> {
    let path = env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| default.into());
    if Command::new(&path)
        .arg(version_argument)
        .output()
        .is_ok_and(|output| output.status.success())
    {
        Some(path)
    } else {
        unavailable(variable, &format!("{} is unavailable", path.display()))
    }
}

pub(crate) fn go() -> Option<PathBuf> {
    executable("SONAR_TEST_GO", "go", "version")
}

pub(crate) fn rust_analyzer() -> Option<PathBuf> {
    executable("SONAR_TEST_RUST_ANALYZER", "rust-analyzer", "--version")
}

pub(crate) fn typescript() -> Option<PathBuf> {
    let path = env::var_os("SONAR_TEST_TYPESCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".sonar/typescript/oracle/node_modules/typescript")
        });
    if !path.join("package.json").is_file() {
        return unavailable(
            "SONAR_TEST_TYPESCRIPT",
            &format!("TypeScript package {} is unavailable", path.display()),
        );
    }
    if !Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return unavailable("SONAR_TEST_TYPESCRIPT", "Node.js is unavailable");
    }
    Some(path)
}

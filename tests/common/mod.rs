//! Helpers shared by the CLI and differential test suites. Each test file
//! is its own crate, so not every helper is used by every crate.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub fn compiler(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_Compiler"))
        .args(args)
        .output()
        .expect("failed to run compiler binary")
}

/// Like `compiler`, but run from `dir` — for tests where relative paths
/// (e.g. the default build output) must resolve against a scratch dir.
pub fn compiler_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_Compiler"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run compiler binary")
}

/// A per-process scratch directory; tests use distinct file names.
pub fn tempdir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

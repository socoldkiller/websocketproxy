use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let build_version = git_build_version()
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| String::from("0.0.0")));

    println!("cargo:rustc-env=BUILD_VERSION={build_version}");
    println!("cargo:rerun-if-changed=build.rs");

    if let Some(git_dir) = git_dir() {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join("packed-refs").display()
        );
        println!("cargo:rerun-if-changed={}", git_dir.join("refs").display());
    }
}

fn git_build_version() -> Option<String> {
    git_output(&["describe", "--tags", "--exact-match", "HEAD"])
        .or_else(|| git_output(&["rev-parse", "--short=12", "HEAD"]))
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let version = stdout.trim().to_owned();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

fn git_dir() -> Option<PathBuf> {
    git_output(&["rev-parse", "--git-dir"]).map(|path| normalize_git_dir(Path::new(&path)))
}

fn normalize_git_dir(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").ok();
    manifest_dir.map_or_else(
        || PathBuf::from(path),
        |base| PathBuf::from(base).join(path),
    )
}

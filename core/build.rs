use std::path::Path;
use std::process::Command;

fn main() {
    println!(
        "cargo::rustc-env=SMUDGY_BUILD_NAME={}-{}-{}",
        std::env::var("PROFILE").unwrap(),
        std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap(),
        std::env::var("CARGO_CFG_TARGET_ARCH").unwrap()
    );

    println!("cargo::rerun-if-env-changed=SMUDGY_GIT_COMMIT");
    println!("cargo::rerun-if-env-changed=GITHUB_SHA");
    println!("cargo::rerun-if-env-changed=CI_COMMIT_SHA");

    if let Some(commit) = commit_hash() {
        println!("cargo::rustc-env=SMUDGY_GIT_COMMIT={commit}");
    }
    watch_git_head();
}

fn commit_hash() -> Option<String> {
    ["SMUDGY_GIT_COMMIT", "GITHUB_SHA", "CI_COMMIT_SHA"]
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .and_then(|value| short_hash(&value))
        })
        .or_else(|| {
            let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")?;
            let output = Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .current_dir(manifest_dir)
                .output()
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
                .and_then(|value| short_hash(&value))
        })
}

fn short_hash(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() < 7 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(value[..value.len().min(12)].to_ascii_lowercase())
}

/// Make a branch advance refresh the embedded hash even if no Rust source changed.
fn watch_git_head() {
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        return;
    };
    let repo = Path::new(&manifest_dir);
    emit_git_path(repo, "HEAD");

    let Ok(output) = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(repo)
        .output()
    else {
        return;
    };
    if output.status.success() {
        emit_git_path(repo, String::from_utf8_lossy(&output.stdout).trim());
    }
}

fn emit_git_path(repo: &Path, name: &str) {
    let Ok(output) = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-path", name])
        .current_dir(repo)
        .output()
    else {
        return;
    };
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout);
        println!("cargo::rerun-if-changed={}", path.trim());
    }
}

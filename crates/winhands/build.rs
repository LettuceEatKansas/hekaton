use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=HANDS_SOURCE_COMMIT");
    println!("cargo:rerun-if-env-changed=HANDS_SOURCE_DIRTY");
    for path in [
        "src",
        "Cargo.toml",
        "../../Cargo.toml",
        "../wh-core/src",
        "../wh-win/src",
        "../wh-mac/src",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    // A worktree's .git is a pointer file. Ask Git for the actual metadata paths.
    for name in ["HEAD", "index", "refs/heads/main"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &reference])
    {
        println!("cargo:rerun-if-changed={path}");
    }
    let commit = std::env::var("HANDS_SOURCE_COMMIT")
        .ok()
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    let dirty = std::env::var("HANDS_SOURCE_DIRTY").ok().unwrap_or_else(|| {
        git(&["status", "--porcelain", "--untracked-files=normal"])
            .map_or_else(|| "unknown".into(), |s| (!s.is_empty()).to_string())
    });
    println!("cargo:rustc-env=HANDS_SOURCE_COMMIT={commit}");
    println!("cargo:rustc-env=HANDS_SOURCE_DIRTY={dirty}");
    println!(
        "cargo:rustc-env=HANDS_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
}

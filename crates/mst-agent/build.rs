//! Embeds the source revision, so a binary identifies the exact source it was
//! built from (see LICENSING.md: rebuilding with a modified libvirt protocol
//! definition starts from that source).

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    // Release builds outside a git checkout can set this explicitly.
    println!("cargo:rerun-if-env-changed=MST_SOURCE_REVISION");
    let revision = std::env::var("MST_SOURCE_REVISION").ok().or_else(|| {
        let commit = git(&["rev-parse", "--short=12", "HEAD"])?;
        let dirty =
            git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        Some(if dirty {
            format!("{commit}-modified")
        } else {
            commit
        })
    });
    println!(
        "cargo:rustc-env=MST_SOURCE_REVISION={}",
        revision.as_deref().unwrap_or("unknown")
    );

    // Rebuild when HEAD moves or the index changes.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let git_dir = Path::new(&git_dir);
        for f in ["HEAD", "index", "packed-refs"] {
            println!("cargo:rerun-if-changed={}", git_dir.join(f).display());
        }
        if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!(
                "cargo:rerun-if-changed={}",
                git_dir.join(head_ref).display()
            );
        }
    }
}

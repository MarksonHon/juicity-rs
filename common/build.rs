use std::process::Command;

fn main() {
    // ---- Build timestamp ----
    let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
    println!("cargo:rustc-env=JUICITY_BUILD_TIMESTAMP={}", timestamp);

    // ---- Git commit hash ----
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=JUICITY_GIT_HASH={}", git_hash);

    // ---- Git tag ----
    let git_tag = Command::new("git")
        .args(["describe", "--tags", "--exact-match", "HEAD"])
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            // Not on an exact tag — include the describe string for traceability
            let describe = Command::new("git")
                .args(["describe", "--tags"])
                .output()
                .ok()
                .and_then(|out| {
                    if out.status.success() {
                        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
                    } else {
                        None
                    }
                });
            match describe {
                Some(d) => format!("unstable ({})", d),
                None => "unstable".to_string(),
            }
        });
    println!("cargo:rustc-env=JUICITY_GIT_TAG={}", git_tag);

    // Rerun build script only when Git HEAD changes (or if build.rs itself changes)
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/tags");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
}

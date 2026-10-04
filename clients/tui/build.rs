use std::env;
use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .or_else(|| {
            Command::new("jj")
                .args(["log", "-r", "@", "--no-graph", "-T", "commit_id.short()"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let rust = Command::new("rustc")
        .args(["--version"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());

    let build_time = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    // The release version is printed separately (CARGO_PKG_VERSION), so this
    // carries only what distinguishes one build of that version from another.
    let build_details = format!("{hash}-{profile}-{build_time} / {rust}");

    println!("cargo:rustc-env=BUILD_DETAILS={build_details}");
}

#[path = "build_support/android_manifest.rs"]
mod android_manifest;

fn main() {
    tauri_build::build();
    patch_android_manifest();
}

/// Edit the manifest `tauri android init` generated: declare the camera, and
/// turn off auto-backup.
///
/// `gen/android` is regenerated and gitignored, so a hand edit to its manifest
/// does not survive `tauri android init`; this rewrites the manifest on every
/// build instead. It is a no-op for any target but Android, and when there is
/// no generated project.
fn patch_android_manifest() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let Some(project) = std::env::var_os("TAURI_ANDROID_PROJECT_PATH") else {
        return;
    };
    let path = std::path::Path::new(&project).join("app/src/main/AndroidManifest.xml");
    // `package-android.sh` deletes and regenerates `gen/android` while `target/`
    // survives, so without this Cargo sees nothing changed, skips the script,
    // and the fresh manifest never gets its edits.
    println!("cargo:rerun-if-changed={}", path.display());
    let Ok(manifest) = std::fs::read_to_string(&path) else {
        return;
    };
    let rewritten = android_manifest::with_camera_block(&manifest)
        .and_then(|manifest| android_manifest::with_backup_disabled(&manifest))
        .unwrap_or_else(|why| panic!("{why}"));
    if rewritten != manifest {
        std::fs::write(&path, rewritten).expect("failed to update AndroidManifest.xml");
    }
}

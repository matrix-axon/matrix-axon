#[path = "build_support/android_manifest.rs"]
mod android_manifest;

fn main() {
    tauri_build::build();
    patch_android_manifest();
    give_android_library_a_build_id();
}

/// Link the Android library with a GNU build ID.
///
/// Play matches the native debug symbols it was given to a crash by the
/// library's build ID, which is how a native stack trace in Android vitals
/// becomes readable function names. Rust's Android build does not add one by
/// itself: the library had a `.note.android.ident` and no `.note.gnu.build-id`,
/// so even a symbols file would have had nothing to match.
///
/// A link argument from the build script rather than `rustflags` in
/// `.cargo/config.toml`: the Tauri CLI passes its own through
/// `CARGO_TARGET_*_RUSTFLAGS`, and an environment variable replaces the config
/// file's value instead of adding to it. `-cdylib` limits it to the library.
fn give_android_library_a_build_id() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-arg-cdylib=-Wl,--build-id=sha1");
    }
}

/// Edit the manifest `tauri android init` generated: declare the camera and
/// `POST_NOTIFICATIONS`, and turn off auto-backup.
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
        .and_then(|manifest| android_manifest::with_post_notifications(&manifest))
        .and_then(|manifest| android_manifest::with_backup_disabled(&manifest))
        .unwrap_or_else(|why| panic!("{why}"));
    if rewritten != manifest {
        std::fs::write(&path, rewritten).expect("failed to update AndroidManifest.xml");
    }
}

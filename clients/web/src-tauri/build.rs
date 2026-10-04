#[path = "build_support/android_manifest.rs"]
mod android_manifest;

fn main() {
    tauri_build::build();
    declare_android_camera();
}

/// Declare `CAMERA` in the generated Android manifest, for the ADR 0097 QR
/// sign-in flow.
///
/// Without it `getUserMedia` fails without ever showing a prompt — the WebView
/// can only ask the OS for a permission the app has declared — and the page
/// reports "unavailable in this browser" rather than a denial. The same
/// failure ADR 0102 records for macOS's missing `NSCameraUsageDescription`.
///
/// `gen/android` is regenerated and gitignored, so a hand edit to its manifest
/// does not survive `tauri android init`; this rewrites the manifest on every
/// build instead. The block is bracketed by the same marker comments the
/// deep-link plugin uses for its intent filter, and replaces its own previous
/// output, so a rebuild does not stack copies. It is a no-op for any target but
/// Android, and when there is no generated project.
///
/// Written here rather than with `tauri_utils::build::update_android_manifest`,
/// which does exactly this: that function is behind `tauri-utils`'s `build`
/// feature, which is not on for build scripts and pulls in 42 crates
/// (`kuchikiki` and its HTML parser among them) to save twenty lines.
///
/// `required="false"` on the feature: a tablet or a Chromebook with no camera
/// should still be able to install the app and sign in some other way.
fn declare_android_camera() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    let Some(project) = std::env::var_os("TAURI_ANDROID_PROJECT_PATH") else {
        return;
    };
    let path = std::path::Path::new(&project).join("app/src/main/AndroidManifest.xml");
    // `package-android.sh` deletes and regenerates `gen/android` while `target/`
    // survives, so without this Cargo sees nothing changed, skips the script,
    // and the fresh manifest never gets the block.
    println!("cargo:rerun-if-changed={}", path.display());
    let Ok(manifest) = std::fs::read_to_string(&path) else {
        return;
    };
    let rewritten =
        android_manifest::with_camera_block(&manifest).unwrap_or_else(|why| panic!("{why}"));
    if rewritten != manifest {
        std::fs::write(&path, rewritten).expect("failed to update AndroidManifest.xml");
    }
}

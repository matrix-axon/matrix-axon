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
    let rewritten = with_backup_disabled(&with_camera_block(&manifest));
    if rewritten != manifest {
        std::fs::write(&path, rewritten).expect("failed to update AndroidManifest.xml");
    }
}

/// `manifest` with `android:allowBackup="false"` on `<application>`.
///
/// The template sets nothing, and the default is on: Auto Backup then copies
/// the app's data directory to the user's Google account and restores it onto
/// a new device. That directory holds the WebView's `localStorage`, where the
/// sign-in token lives (`auth/persistence.ts`), so the token would leave the
/// device and reappear on another without anyone signing in there.
fn with_backup_disabled(manifest: &str) -> String {
    const ATTR: &str = "android:allowBackup=\"";
    if let Some(start) = manifest.find(ATTR) {
        // Already declared: force it off rather than trust a template change.
        let value_start = start + ATTR.len();
        if let Some(len) = manifest[value_start..].find('"') {
            let mut out = String::with_capacity(manifest.len());
            out.push_str(&manifest[..value_start]);
            out.push_str("false");
            out.push_str(&manifest[value_start + len..]);
            return out;
        }
        return manifest.to_string();
    }
    manifest.replacen(
        "<application",
        "<application\n        android:allowBackup=\"false\"",
        1,
    )
}

/// Brackets the camera block so a rebuild can find and replace it.
const MARKER: &str = "<!-- CAMERA. AUTO-GENERATED. DO NOT REMOVE. -->";

/// Declare `CAMERA` in the generated Android manifest, for the ADR 0097 QR
/// sign-in flow.
///
/// Without it `getUserMedia` fails without ever showing a prompt — the WebView
/// can only ask the OS for a permission the app has declared — and the page
/// reports "unavailable in this browser" rather than a denial. The same
/// failure ADR 0102 records for macOS's missing `NSCameraUsageDescription`.
///
/// The block is bracketed by the same marker comments the deep-link plugin
/// uses for its intent filter, and replaces its own previous output, so a
/// rebuild does not stack copies.
///
/// Written here rather than with `tauri_utils::build::update_android_manifest`,
/// which does exactly this: that function is behind `tauri-utils`'s `build`
/// feature, which is not on for build scripts and pulls in 42 crates
/// (`kuchikiki` and its HTML parser among them) to save twenty lines.
///
/// `required="false"` on the feature: a tablet or a Chromebook with no camera
/// should still be able to install the app and sign in some other way.
///
/// Returns `manifest` with the block placed just before `</manifest>`, and any
/// earlier copy of it removed.
fn with_camera_block(manifest: &str) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for line in manifest.split('\n') {
        if line.contains(MARKER) {
            inside = !inside;
            continue;
        }
        if inside {
            continue;
        }
        if line.contains("</manifest>") {
            out.push(format!("    {MARKER}"));
            out.push(r#"    <uses-permission android:name="android.permission.CAMERA" />"#.into());
            out.push(
                r#"    <uses-feature android:name="android.hardware.camera" android:required="false" />"#
                    .into(),
            );
            out.push(format!("    {MARKER}"));
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

//! Rewrites of the generated `AndroidManifest.xml`.
//!
//! Pure string functions, in their own file so that `build.rs` (which cannot be
//! a test target) and `tests/android_manifest.rs` both include the same code
//! with `#[path]`.

/// Opens and closes the block this build script owns. The same comment the
/// deep-link plugin brackets its intent filter with, so a reader recognises it.
pub const MARKER: &str = "<!-- CAMERA. AUTO-GENERATED. DO NOT REMOVE. -->";

/// `manifest` with the camera block placed just before `</manifest>`, and any
/// earlier copy of it removed.
///
/// An odd number of marker lines (a hand edit that removed one, or a merge
/// conflict) is an error: reading it as "inside the block until the end of the
/// file" would drop everything after it, `</manifest>` included, and leave an
/// invalid manifest that fails much later with an obscure aapt error.
pub fn with_camera_block(manifest: &str) -> Result<String, String> {
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
    if inside {
        return Err(format!(
            "AndroidManifest.xml has an unbalanced `{MARKER}` marker; delete the marker lines and the block between them, or regenerate gen/android"
        ));
    }
    Ok(out.join("\n"))
}

//! Rewrites of the generated `AndroidManifest.xml`.
//!
//! Pure string functions, in their own file so that `build.rs` (which cannot be
//! a test target) and `tests/android_manifest.rs` both include the same code
//! with `#[path]`.

/// Opens and closes the camera block this build script owns. The same comment
/// the deep-link plugin brackets its intent filter with, so a reader recognises
/// it.
pub const MARKER: &str = "<!-- CAMERA. AUTO-GENERATED. DO NOT REMOVE. -->";

/// Opens and closes the notification-permission block.
pub const NOTIFICATION_MARKER: &str = "<!-- NOTIFICATIONS. AUTO-GENERATED. DO NOT REMOVE. -->";

/// `manifest` with the camera block placed just before `</manifest>`, and any
/// earlier copy of it removed.
///
/// The block declares `CAMERA`, for the ADR 0097 QR sign-in flow. Without it
/// `getUserMedia` fails without ever showing a prompt — the WebView can only
/// ask the OS for a permission the app has declared — and the page reports
/// "unavailable in this browser" rather than a denial. The same failure ADR
/// 0102 records for macOS's missing `NSCameraUsageDescription`. It replaces its
/// own previous output, so a rebuild does not stack copies.
///
/// Written here rather than with `tauri_utils::build::update_android_manifest`,
/// which does exactly this: that function is behind `tauri-utils`'s `build`
/// feature, which is not on for build scripts and pulls in 42 crates
/// (`kuchikiki` and its HTML parser among them) to save twenty lines.
///
/// `required="false"` on the feature: a tablet or a Chromebook with no camera
/// should still be able to install the app and sign in some other way.
///
/// An odd number of marker lines (a hand edit that removed one, or a merge
/// conflict) is an error: reading it as "inside the block until the end of the
/// file" would drop everything after it, `</manifest>` included, and leave an
/// invalid manifest that fails much later with an obscure aapt error.
pub fn with_camera_block(manifest: &str) -> Result<String, String> {
    with_marked_block(
        manifest,
        MARKER,
        &[
            r#"    <uses-permission android:name="android.permission.CAMERA" />"#,
            r#"    <uses-feature android:name="android.hardware.camera" android:required="false" />"#,
        ],
    )
}

/// `manifest` with `POST_NOTIFICATIONS` declared just before `</manifest>`.
///
/// Android 13 and newer refuse to post a notification unless the app manifest
/// declares this permission; the runtime prompt cannot grant what was never
/// declared. `tauri-plugin-notification` also declares it in its own library
/// manifest, which the Gradle merger folds in later. This copy lives in the
/// app manifest so the declaration does not depend on that merger, and so the
/// camera rewrite — which drops only its own marker block — cannot be the
/// thing that removes it. A second copy of the same permission merges into one.
pub fn with_post_notifications(manifest: &str) -> Result<String, String> {
    with_marked_block(
        manifest,
        NOTIFICATION_MARKER,
        &[r#"    <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />"#],
    )
}

/// Replace one marked block, placed just before `</manifest>`.
///
/// An odd number of marker lines (a hand edit that removed one, or a merge
/// conflict) is an error: reading it as "inside the block until the end of the
/// file" would drop everything after it, `</manifest>` included, and leave an
/// invalid manifest that fails much later with an obscure aapt error.
fn with_marked_block(manifest: &str, marker: &str, lines: &[&str]) -> Result<String, String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in manifest.split('\n') {
        if line.contains(marker) {
            inside = !inside;
            continue;
        }
        if inside {
            continue;
        }
        if line.contains("</manifest>") {
            out.push(format!("    {marker}"));
            for block_line in lines {
                out.push((*block_line).to_string());
            }
            out.push(format!("    {marker}"));
        }
        out.push(line.to_string());
    }
    if inside {
        return Err(format!(
            "AndroidManifest.xml has an unbalanced `{marker}` marker; delete the marker lines and the block between them, or regenerate gen/android"
        ));
    }
    Ok(out.join("\n"))
}

/// `manifest` with `android:allowBackup="false"` on the `<application>` start
/// tag, and nowhere else.
///
/// The template sets nothing, and the default is on: Auto Backup then copies
/// the app's data directory to the user's Google account and restores it onto
/// a new device. That directory holds the WebView's `localStorage`, where the
/// sign-in token lives (`auth/persistence.ts`), so the token would leave the
/// device and reappear on another without anyone signing in there.
///
/// An existing value is forced off rather than trusted, in case the template
/// changes. A manifest with no `<application>` is returned unchanged. A start
/// tag or attribute that never closes is an error: leaving backup on silently
/// would defeat the point.
pub fn with_backup_disabled(manifest: &str) -> Result<String, String> {
    const ATTR: &str = "android:allowBackup=\"";
    let Some(start) = application_tag_start(manifest) else {
        return Ok(manifest.to_string());
    };
    let end = start_tag_end(manifest, start)
        .ok_or("AndroidManifest.xml has an <application> tag that never closes")?;
    let tag = &manifest[start..end];
    let rewritten = match tag.find(ATTR) {
        Some(attr) => {
            let value = attr + ATTR.len();
            let len = tag[value..]
                .find('"')
                .ok_or("AndroidManifest.xml has an unterminated android:allowBackup value")?;
            format!("{}false{}", &tag[..value], &tag[value + len..])
        }
        None => tag.replacen(
            "<application",
            "<application\n        android:allowBackup=\"false\"",
            1,
        ),
    };
    Ok(format!(
        "{}{}{}",
        &manifest[..start],
        rewritten,
        &manifest[end..]
    ))
}

/// Where the `<application` element starts: not `<application-foo`.
fn application_tag_start(manifest: &str) -> Option<usize> {
    manifest
        .match_indices("<application")
        .find_map(|(at, name)| {
            let next = manifest[at + name.len()..].chars().next()?;
            (next.is_whitespace() || next == '>' || next == '/').then_some(at)
        })
}

/// Just past the `>` that ends the start tag beginning at `start`, ignoring any
/// `>` inside a quoted attribute value.
fn start_tag_end(manifest: &str, start: usize) -> Option<usize> {
    let mut quoted = false;
    for (offset, ch) in manifest[start..].char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '>' if !quoted => return Some(start + offset + 1),
            _ => {}
        }
    }
    None
}

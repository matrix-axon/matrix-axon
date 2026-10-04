//! The manifest rewrite `build.rs` applies to the generated Android project.
//! A build script is not a test target, so the function lives in
//! `build_support/android_manifest.rs` and is included here.

#[path = "../build_support/android_manifest.rs"]
mod android_manifest;

use android_manifest::{with_camera_block, MARKER};

const BARE: &str = "<manifest>\n    <application />\n</manifest>";

fn block_count(manifest: &str) -> usize {
    manifest.matches(MARKER).count() / 2
}

#[test]
fn a_fresh_manifest_gets_the_block_before_the_closing_tag() {
    let out = with_camera_block(BARE).unwrap();
    assert_eq!(block_count(&out), 1);
    assert!(out.contains("android.permission.CAMERA"));
    assert!(out.contains(r#"android:name="android.hardware.camera" android:required="false""#));
    let block = out.find(MARKER).unwrap();
    assert!(block < out.find("</manifest>").unwrap());
    assert!(out.find("<application />").unwrap() < block);
}

#[test]
fn a_second_run_changes_nothing() {
    let once = with_camera_block(BARE).unwrap();
    assert_eq!(with_camera_block(&once).unwrap(), once);
}

#[test]
fn an_earlier_block_is_replaced_not_stacked() {
    let stale = format!(
        "<manifest>\n    {MARKER}\n    <uses-permission android:name=\"old\" />\n    {MARKER}\n    <application />\n</manifest>"
    );
    let out = with_camera_block(&stale).unwrap();
    assert_eq!(block_count(&out), 1);
    assert!(!out.contains("\"old\""));
    assert!(out.contains("android.permission.CAMERA"));
    assert!(out.contains("<application />"));
}

#[test]
fn a_manifest_with_no_closing_tag_is_left_alone() {
    let broken = "<manifest>\n    <application />";
    assert_eq!(with_camera_block(broken).unwrap(), broken);
}

#[test]
fn an_unbalanced_marker_is_an_error_not_a_truncated_manifest() {
    let odd = format!("<manifest>\n    {MARKER}\n    <application />\n</manifest>");
    let why = with_camera_block(&odd).unwrap_err();
    assert!(why.contains("unbalanced"), "{why}");
}

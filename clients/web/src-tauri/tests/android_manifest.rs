//! The manifest rewrite `build.rs` applies to the generated Android project.
//! A build script is not a test target, so the function lives in
//! `build_support/android_manifest.rs` and is included here.

#[path = "../build_support/android_manifest.rs"]
mod android_manifest;

use android_manifest::{with_backup_disabled, with_camera_block, MARKER};

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

const APP: &str =
    "<manifest>\n    <application android:label=\"Axon\">\n    </application>\n</manifest>";

#[test]
fn backup_is_switched_off_when_the_template_says_nothing() {
    let out = with_backup_disabled(APP).unwrap();
    assert!(
        out.contains("<application\n        android:allowBackup=\"false\" android:label=\"Axon\">")
    );
    assert_eq!(out.matches("allowBackup").count(), 1);
}

#[test]
fn backup_true_is_forced_off() {
    let on = "<manifest>\n    <application android:allowBackup=\"true\" android:label=\"Axon\">\n</manifest>";
    let out = with_backup_disabled(on).unwrap();
    assert!(out.contains("android:allowBackup=\"false\" android:label=\"Axon\""));
    assert!(!out.contains("\"true\""));
}

#[test]
fn a_second_run_leaves_the_manifest_byte_for_byte_alone() {
    let once = with_backup_disabled(APP).unwrap();
    assert_eq!(with_backup_disabled(&once).unwrap(), once);
}

#[test]
fn only_the_application_tag_is_rewritten() {
    let other = "<manifest>\n    <!-- android:allowBackup=\"true\" -->\n    <uses-sdk android:allowBackup=\"true\" />\n    <application android:label=\"Axon\">\n</manifest>";
    let out = with_backup_disabled(other).unwrap();
    assert_eq!(out.matches("android:allowBackup=\"true\"").count(), 2);
    assert!(out.contains("<application\n        android:allowBackup=\"false\""));
}

#[test]
fn a_look_alike_element_is_not_the_application() {
    let near = "<manifest>\n    <application-foo />\n</manifest>";
    assert_eq!(with_backup_disabled(near).unwrap(), near);
}

#[test]
fn a_greater_than_inside_a_value_does_not_end_the_tag() {
    let tricky = "<manifest>\n    <application android:label=\"a>b\" android:allowBackup=\"true\">\n</manifest>";
    let out = with_backup_disabled(tricky).unwrap();
    assert!(out.contains("android:label=\"a>b\" android:allowBackup=\"false\""));
}

#[test]
fn an_unterminated_value_or_tag_is_an_error() {
    let value = "<manifest>\n    <application android:allowBackup=\"true>\n</manifest>";
    assert!(with_backup_disabled(value).is_err());
    let tag = "<manifest>\n    <application android:label=\"Axon\"";
    assert!(with_backup_disabled(tag).is_err());
}

#[test]
fn both_rewrites_compose() {
    let out = with_camera_block(APP)
        .and_then(|m| with_backup_disabled(&m))
        .unwrap();
    assert!(out.contains("android.permission.CAMERA"));
    assert!(out.contains("android:allowBackup=\"false\""));
}

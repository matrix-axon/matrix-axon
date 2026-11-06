const COMMANDS: &[&str] = &[
    "capabilities",
    "apple_sign_in",
    "secret_load",
    "secret_set",
    "secret_delete",
    "share",
];

fn main() {
    // SwiftLinker watches the plugin's ios directory, but not sibling package
    // dependencies. Preview security fixes must also rebuild the main-app bridge.
    // Watch source inputs only, excluding SwiftPM's mutable .build directory.
    for path in [
        "../incoming-share/Package.swift",
        "../incoming-share/Package.resolved",
        "../incoming-share/Sources",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    tauri_plugin::Builder::new(COMMANDS).ios_path("ios").build();
}

const COMMANDS: &[&str] = &[
    "capabilities",
    "apple_sign_in",
    "secret_load",
    "secret_set",
    "secret_delete",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).ios_path("ios").build();
}

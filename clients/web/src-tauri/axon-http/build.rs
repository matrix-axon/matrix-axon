const COMMANDS: &[&str] = &["send", "read_body", "cancel", "drop_body", "reset", "stats"];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}

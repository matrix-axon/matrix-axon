const COMMANDS: &[&str] = &["lookup", "cancel"];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}

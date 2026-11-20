const COMMANDS: &[&str] = &[
    "send",
    "read_body",
    "cancel",
    "drop_body",
    "reset",
    "stats",
    "set_media_stream_base",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS).build();
}

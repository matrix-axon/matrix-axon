const COMMANDS: &[&str] = &[
    "capabilities",
    "device_token",
    "delivered_event_ids",
    "pending_wake",
    "post_local",
    "register_listener",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .ios_path("ios")
        .android_path("android")
        .build();
}

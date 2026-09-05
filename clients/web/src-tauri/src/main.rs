// Hide the console window on a Windows release build. Without this, launching
// the app from Explorer pops a terminal behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    axon_shell_lib::run()
}

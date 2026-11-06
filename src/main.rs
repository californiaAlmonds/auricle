// Show the console window (with logs) for dev builds; hide it for release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
  auricle_lib::launch();
}

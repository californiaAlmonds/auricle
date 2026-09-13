// Show the console window (with logs) for dev builds; hide it for release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
  if let Err(err) = auricle_lib::run_native_shell() {
    eprintln!("Failed to launch native shell: {err}");
  }
}

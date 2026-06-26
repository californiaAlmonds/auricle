// Prevents additional console window on Windows
#![windows_subsystem = "windows"]

fn main() {
  if let Err(err) = auricle_lib::run_native_shell() {
    eprintln!("Failed to launch native shell: {err}");
  }
}

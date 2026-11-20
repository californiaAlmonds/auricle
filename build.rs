fn main() {
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=icons/icon.ico");

  slint_build::compile("ui/native_shell.slint").expect("Failed to compile Slint UI");

  if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
    embed_windows_resources();
  }
}

#[cfg(windows)]
fn embed_windows_resources() {
  let mut res = winresource::WindowsResource::new();
  res.set_icon("icons/icon.ico");
  res.set("ProductName", "Auricle");
  res.set("FileDescription", "Auricle");
  if let Err(e) = res.compile() {
    println!("cargo:warning=Failed to embed Windows resources: {e}");
  }
}

#[cfg(not(windows))]
fn embed_windows_resources() {}

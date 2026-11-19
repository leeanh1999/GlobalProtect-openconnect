fn main() {
  println!("cargo:rerun-if-changed=res/app.rc");
  println!("cargo:rerun-if-changed=res/app.manifest");
  println!("cargo:rerun-if-changed=res/app.ico");
  if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
    return;
  }
  // The icon is generated with `gpgui-win --export-icons res`; build without it until then.
  let macros: &[&str] = if std::path::Path::new("res/app.ico").exists() {
    &["HAVE_APP_ICON"]
  } else {
    &[]
  };
  embed_resource::compile("res/app.rc", macros)
    .manifest_required()
    .expect("embed Windows resources");
}

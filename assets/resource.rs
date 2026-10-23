// Shared by the crates' build.rs (`include!`): embeds Quickseek's icon
// (assets/quickseek.ico, resource 1: the exe's icon in Explorer, the window,
// taskbar and tray) and a VERSIONINFO, so file properties show the same
// version as the registry entry and the winget manifest.

#[allow(dead_code)]
fn embed_icon_and_version(description: &str, file: &str) {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/quickseek.ico")
        .canonicalize()
        .expect("assets/quickseek.ico (python tools/make-icon.py)");
    println!("cargo:rerun-if-changed={}", icon.display());
    let v = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut n: Vec<u16> = v
        .split(['.', '-', '+'])
        .filter_map(|p| p.parse().ok())
        .collect();
    n.resize(4, 0);
    let (a, b, c, d) = (n[0], n[1], n[2], n[3]);
    let icon_path = icon.to_string_lossy().replace('\\', "\\\\").replace(r"\\\\?\\", "");
    let internal = file.trim_end_matches(".exe");
    let rc = format!(
        r#"1 ICON "{icon_path}"
1 VERSIONINFO
FILEVERSION {a},{b},{c},{d}
PRODUCTVERSION {a},{b},{c},{d}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Ossian Hanning"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{v}"
      VALUE "InternalName", "{internal}"
      VALUE "OriginalFilename", "{file}"
      VALUE "ProductName", "Quickseek"
      VALUE "ProductVersion", "{v}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let path = out.join("quickseek.rc");
    std::fs::write(&path, rc).unwrap();
    embed_resource::compile(&path, embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}

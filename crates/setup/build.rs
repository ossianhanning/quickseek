// Embeds the binaries to install (zstd-compressed) from the folder in
// RAPIDFIND_SETUP_PAYLOAD (tools\build-release.ps1 sets it to the release
// output). Without it the installer builds with an empty payload and says so
// when run. Also embeds a manifest asking for administrator rights (the
// service and Program Files need them) and Common Controls v6.

const FILES: [&str; 3] = ["rapidfind.exe", "rapidfind-gui.exe", "rapidfind-mcp.exe"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RAPIDFIND_SETUP_PAYLOAD");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let payload = std::env::var_os("RAPIDFIND_SETUP_PAYLOAD").map(std::path::PathBuf::from);
    for f in FILES {
        let data = match &payload {
            Some(dir) => {
                let p = dir.join(f);
                println!("cargo:rerun-if-changed={}", p.display());
                let raw =
                    std::fs::read(&p).unwrap_or_else(|e| panic!("payload {}: {e}", p.display()));
                zstd::encode_all(&raw[..], 19).unwrap()
            }
            None => Vec::new(),
        };
        std::fs::write(out.join(format!("{f}.zst")), data).unwrap();
    }
    version_resource(&out);
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator'");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}

/// A VERSIONINFO resource, so the installer's file properties (and anything
/// that checks them) show the same version the registry entry and the winget
/// manifest carry: all three come from the workspace version.
fn version_resource(out: &std::path::Path) {
    let v = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut n: Vec<u16> = v
        .split(['.', '-', '+'])
        .filter_map(|p| p.parse().ok())
        .collect();
    n.resize(4, 0);
    let (a, b, c, d) = (n[0], n[1], n[2], n[3]);
    let rc = format!(
        r#"1 VERSIONINFO
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
      VALUE "FileDescription", "rapidfind setup"
      VALUE "FileVersion", "{v}"
      VALUE "InternalName", "rapidfind-setup"
      VALUE "OriginalFilename", "rapidfind-setup.exe"
      VALUE "ProductName", "rapidfind"
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
    let path = out.join("version.rc");
    std::fs::write(&path, rc).unwrap();
    embed_resource::compile(&path, embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}

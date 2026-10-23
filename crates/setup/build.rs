// Embeds the binaries to install (zstd-compressed) from the folder in
// QUICKSEEK_SETUP_PAYLOAD (tools\build-release.ps1 sets it to the release
// output). Without it the installer builds with an empty payload and says so
// when run. Also embeds a manifest asking for administrator rights (the
// service and Program Files need them) and Common Controls v6.

include!("../../assets/resource.rs");

const FILES: [&str; 3] = ["quickseek.exe", "quickseek-gui.exe", "quickseek-mcp.exe"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=QUICKSEEK_SETUP_PAYLOAD");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let payload = std::env::var_os("QUICKSEEK_SETUP_PAYLOAD").map(std::path::PathBuf::from);
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
    embed_icon_and_version("Quickseek Setup", "quickseek-setup.exe");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator'");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}

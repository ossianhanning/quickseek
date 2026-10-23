// Embeds a manifest asking for Common Controls v6 (visual styles, cue
// banners, double-buffered list views), and the icon and version.
include!("../../assets/resource.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    embed_icon_and_version("Quickseek", "quickseek-gui.exe");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}

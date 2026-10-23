// Embeds Quickseek's icon and the version.
include!("../../assets/resource.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    embed_icon_and_version("Quickseek MCP server", "quickseek-mcp.exe");
}

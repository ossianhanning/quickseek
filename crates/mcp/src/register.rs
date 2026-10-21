//! Registering `rapidfind-mcp` with AI clients, per user and without admin:
//! Claude Code (through its own `claude mcp` command), Claude Desktop,
//! Cursor, VS Code (stable and Insiders) and Windsurf (by merging their JSON
//! config files).
//!
//! Config files are edited carefully: key order is preserved, a backup is
//! written next to the file first, other servers are left alone, and a file
//! that isn't plain JSON (VS Code allows comments) is never rewritten.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value, json};

/// The name the server is registered under.
pub const NAME: &str = "rapidfind";
pub const EXE: &str = "rapidfind-mcp.exe";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    VsCode,
    VsCodeInsiders,
    Windsurf,
}

pub const ALL: [Client; 6] = [
    Client::ClaudeCode,
    Client::ClaudeDesktop,
    Client::Cursor,
    Client::VsCode,
    Client::VsCodeInsiders,
    Client::Windsurf,
];

/// How a client keeps its servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// `{"mcpServers": {"name": {"command": ..., "args": []}}}`
    McpServers,
    /// VS Code: `{"servers": {"name": {"type": "stdio", "command": ...}}}`
    VsCode,
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

impl Client {
    pub fn label(self) -> &'static str {
        match self {
            Client::ClaudeCode => "Claude Code",
            Client::ClaudeDesktop => "Claude Desktop",
            Client::Cursor => "Cursor",
            Client::VsCode => "VS Code",
            Client::VsCodeInsiders => "VS Code Insiders",
            Client::Windsurf => "Windsurf",
        }
    }

    /// Short id for command lines (`--client cursor`).
    pub fn id(self) -> &'static str {
        match self {
            Client::ClaudeCode => "claude-code",
            Client::ClaudeDesktop => "claude-desktop",
            Client::Cursor => "cursor",
            Client::VsCode => "vscode",
            Client::VsCodeInsiders => "vscode-insiders",
            Client::Windsurf => "windsurf",
        }
    }

    pub fn from_id(s: &str) -> Option<Client> {
        ALL.into_iter().find(|c| c.id().eq_ignore_ascii_case(s))
    }

    /// The JSON file holding this client's servers (Claude Code: read-only view).
    pub fn config_path(self) -> Option<PathBuf> {
        let appdata = env_path("APPDATA");
        let home = env_path("USERPROFILE");
        Some(match self {
            Client::ClaudeCode => home?.join(".claude.json"),
            Client::ClaudeDesktop => appdata?.join("Claude").join("claude_desktop_config.json"),
            Client::Cursor => home?.join(".cursor").join("mcp.json"),
            Client::VsCode => appdata?.join("Code").join("User").join("mcp.json"),
            Client::VsCodeInsiders => appdata?
                .join("Code - Insiders")
                .join("User")
                .join("mcp.json"),
            Client::Windsurf => home?
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
        })
    }

    fn format(self) -> Format {
        match self {
            Client::VsCode | Client::VsCodeInsiders => Format::VsCode,
            _ => Format::McpServers,
        }
    }

    /// Is the client installed (its config folder exists)?
    pub fn detected(self) -> bool {
        match self {
            Client::ClaudeCode => {
                claude_cli().is_some() || self.config_path().is_some_and(|p| p.exists())
            }
            _ => self
                .config_path()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .is_some_and(|d| d.is_dir()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub client: Client,
    pub detected: bool,
    /// The command it's registered with, if registered.
    pub registered: Option<String>,
    /// A problem reading its config (e.g. comments in VS Code's file).
    pub problem: Option<String>,
    /// Claude Code only: the rapidfind skill is installed.
    pub skill: bool,
}

// ---- where the exe lives ------------------------------------------------------

/// Per-user install folder (no admin): `%LOCALAPPDATA%\Programs\rapidfind`.
pub fn user_dir() -> PathBuf {
    env_path("LOCALAPPDATA")
        .unwrap_or_else(std::env::temp_dir)
        .join("Programs")
        .join("rapidfind")
}

/// A folder that won't be rebuilt or cleaned under the client's feet:
/// Program Files or the per-user Programs folder (not a build directory).
pub fn is_stable_dir(dir: &Path) -> bool {
    let d = dir.to_string_lossy().to_lowercase();
    let under =
        |v: &str| env_path(v).is_some_and(|p| d.starts_with(&p.to_string_lossy().to_lowercase()));
    under("ProgramFiles")
        || under("ProgramW6432")
        || d.starts_with(
            &user_dir()
                .parent()
                .unwrap()
                .to_string_lossy()
                .to_lowercase(),
        )
}

/// The `rapidfind-mcp.exe` to register: `src` itself when it already lives in
/// a stable folder, else a copy in [`user_dir`] (refreshed if it changed).
pub fn stable_exe(src: &Path) -> Result<PathBuf, String> {
    if src.parent().is_some_and(is_stable_dir) {
        return Ok(src.to_path_buf());
    }
    let dir = user_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let dst = dir.join(EXE);
    let same = std::fs::read(&dst)
        .ok()
        .is_some_and(|d| std::fs::read(src).ok().is_some_and(|s| s == d));
    if !same {
        std::fs::copy(src, &dst).map_err(|e| {
            format!(
                "copying to {} (is a client running it? close it and retry): {e}",
                dst.display()
            )
        })?;
    }
    Ok(dst)
}

/// `rapidfind-mcp.exe` next to the running program, if it's there.
pub fn sibling_exe() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join(EXE);
    p.exists().then_some(p)
}

// ---- JSON clients ----------------------------------------------------------------

fn entry(format: Format, exe: &Path) -> Value {
    let cmd = exe.to_string_lossy().into_owned();
    match format {
        Format::McpServers => json!({ "command": cmd, "args": [] }),
        Format::VsCode => json!({ "type": "stdio", "command": cmd, "args": [] }),
    }
}

fn key(format: Format) -> &'static str {
    match format {
        Format::McpServers => "mcpServers",
        Format::VsCode => "servers",
    }
}

fn read_json(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Ok(Some(Map::new()));
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(Some(m)),
        Ok(_) => Err(format!(
            "{} isn't a JSON object; edit it by hand",
            path.display()
        )),
        Err(e) => Err(format!(
            "{} isn't plain JSON ({e}); comments or trailing commas? Add the server by hand",
            path.display()
        )),
    }
}

fn registered_in(m: &Map<String, Value>, format: Format) -> Option<String> {
    m.get(key(format))?
        .get(NAME)?
        .get("command")?
        .as_str()
        .map(String::from)
}

/// Adds (or replaces) our entry in a JSON config file.
pub fn add_to_file(path: &Path, format_vscode: bool, exe: &Path) -> Result<(), String> {
    let format = if format_vscode {
        Format::VsCode
    } else {
        Format::McpServers
    };
    let mut m = read_json(path)?.unwrap_or_default();
    let servers = m
        .entry(key(format))
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(servers) = servers else {
        return Err(format!(
            "\"{}\" in {} isn't an object",
            key(format),
            path.display()
        ));
    };
    servers.insert(NAME.into(), entry(format, exe));
    write_json(path, &m)
}

/// Removes our entry from a JSON config file (other servers stay).
pub fn remove_from_file(path: &Path, format_vscode: bool) -> Result<bool, String> {
    let format = if format_vscode {
        Format::VsCode
    } else {
        Format::McpServers
    };
    let Some(mut m) = read_json(path)? else {
        return Ok(false);
    };
    let removed = m
        .get_mut(key(format))
        .and_then(Value::as_object_mut)
        .is_some_and(|s| s.remove(NAME).is_some());
    if removed {
        write_json(path, &m)?;
    }
    Ok(removed)
}

fn write_json(path: &Path, m: &Map<String, Value>) -> Result<(), String> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
    }
    if path.exists() {
        let backup = path.with_extension("json.rapidfind-backup");
        std::fs::copy(path, &backup).map_err(|e| format!("backing up {}: {e}", path.display()))?;
    }
    let text = serde_json::to_string_pretty(m).unwrap() + "\n";
    let tmp = path.with_extension("json.rapidfind-tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("replacing {}: {e}", path.display()))
}

// ---- Claude Code -------------------------------------------------------------------

/// The `claude` command (native `claude.exe`, or npm's `claude.cmd`).
fn claude_cli() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    if let Some(h) = env_path("USERPROFILE") {
        dirs.push(h.join(".local").join("bin"));
    }
    if let Some(a) = env_path("APPDATA") {
        dirs.push(a.join("npm"));
    }
    for d in dirs {
        for name in ["claude.exe", "claude.cmd"] {
            let p = d.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn claude(args: &[&str]) -> Result<String, String> {
    let cli = claude_cli().ok_or("the `claude` command isn't on PATH")?;
    // No console window: the GUI and the installer have none to share, so
    // Windows would open one per call (`claude.cmd` runs through cmd.exe).
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = Command::new(&cli)
        .args(args)
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("running {}: {e}", cli.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("claude {}: {}", args.join(" "), text.trim()))
    }
}

/// Claude Code's user-scope registration, read from `~/.claude.json`.
fn claude_code_registered() -> Option<String> {
    let m = read_json(&Client::ClaudeCode.config_path()?).ok()??;
    registered_in(&m, Format::McpServers)
}

// ---- the Claude Code skill ------------------------------------------------------------

/// `~/.claude/skills/rapidfind-disk-space/SKILL.md`: Claude Code loads it by
/// itself when a request is about disk space or finding files.
pub fn skill_path() -> Option<PathBuf> {
    Some(
        env_path("USERPROFILE")?
            .join(".claude")
            .join("skills")
            .join(crate::guide::SKILL_NAME)
            .join("SKILL.md"),
    )
}

pub fn skill_installed() -> bool {
    skill_path().is_some_and(|p| p.exists())
}

pub fn install_skill() -> Result<(), String> {
    let p = skill_path().ok_or("no profile folder")?;
    std::fs::create_dir_all(p.parent().unwrap())
        .map_err(|e| format!("creating {}: {e}", p.display()))?;
    std::fs::write(&p, crate::guide::SKILL_MD).map_err(|e| format!("writing {}: {e}", p.display()))
}

pub fn remove_skill() -> Result<bool, String> {
    let Some(p) = skill_path() else {
        return Ok(false);
    };
    if !p.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&p).map_err(|e| format!("removing {}: {e}", p.display()))?;
    let _ = std::fs::remove_dir(p.parent().unwrap()); // only if now empty
    Ok(true)
}

// ---- per client ----------------------------------------------------------------

pub fn status(c: Client) -> Status {
    let detected = c.detected();
    let (registered, problem) = match c {
        Client::ClaudeCode => (claude_code_registered(), None),
        _ => match c.config_path().map(|p| read_json(&p)) {
            Some(Ok(Some(m))) => (registered_in(&m, c.format()), None),
            Some(Ok(None)) | None => (None, None),
            Some(Err(e)) => (None, Some(e)),
        },
    };
    let skill = c == Client::ClaudeCode && skill_installed();
    Status {
        client: c,
        detected,
        registered,
        problem,
        skill,
    }
}

pub fn status_all() -> Vec<Status> {
    ALL.into_iter().map(status).collect()
}

/// Registers `exe` (an absolute path) with `c`.
pub fn register(c: Client, exe: &Path) -> Result<(), String> {
    match c {
        Client::ClaudeCode => {
            // Replace any older registration (a different path).
            let _ = claude(&["mcp", "remove", "--scope", "user", NAME]);
            claude(&[
                "mcp",
                "add",
                "--scope",
                "user",
                NAME,
                "--",
                &exe.to_string_lossy(),
            ])?;
            install_skill()
        }
        _ => add_to_file(
            &c.config_path().ok_or("no profile folder")?,
            c.format() == Format::VsCode,
            exe,
        ),
    }
}

/// Removes the registration from `c` (true if there was one).
pub fn unregister(c: Client) -> Result<bool, String> {
    match c {
        Client::ClaudeCode => {
            let skill = remove_skill()?;
            if claude_code_registered().is_none() {
                return Ok(skill);
            }
            claude(&["mcp", "remove", "--scope", "user", NAME]).map(|_| true)
        }
        _ => remove_from_file(
            &c.config_path().ok_or("no profile folder")?,
            c.format() == Format::VsCode,
        ),
    }
}

/// What to tell the user after registering with `c`.
pub fn restart_hint(c: Client) -> &'static str {
    match c {
        Client::ClaudeCode => {
            "new Claude Code sessions have it, with the rapidfind skill (`/mcp` lists the server)"
        }
        Client::ClaudeDesktop => "quit Claude Desktop (tray icon → Quit) and start it again",
        Client::Cursor => "Cursor picks it up in Settings → MCP (toggle it on if needed)",
        Client::VsCode | Client::VsCodeInsiders => {
            "start it from the MCP view or `MCP: List Servers` in VS Code (agent mode)"
        }
        Client::Windsurf => "refresh MCP servers in Windsurf's Cascade panel",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rapidfind-reg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn merges_keeping_other_servers_and_order() {
        let d = tmp("merge");
        let p = d.join("mcp.json");
        std::fs::write(&p, "{\n  \"zeta\": 1,\n  \"mcpServers\": { \"unity\": { \"url\": \"http://x\" } },\n  \"alpha\": true\n}").unwrap();
        let exe = Path::new(r"C:\Users\me\AppData\Local\Programs\rapidfind\rapidfind-mcp.exe");
        add_to_file(&p, false, exe).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["mcpServers"]["unity"]["url"], "http://x");
        assert_eq!(
            v["mcpServers"]["rapidfind"]["command"],
            exe.to_string_lossy().as_ref()
        );
        assert!(
            text.find("zeta").unwrap() < text.find("mcpServers").unwrap()
                && text.find("mcpServers").unwrap() < text.find("alpha").unwrap(),
            "key order kept: {text}"
        );
        assert!(d.join("mcp.json.rapidfind-backup").exists());
        // Re-registering replaces, removing leaves the rest.
        add_to_file(&p, false, Path::new(r"D:\other\rapidfind-mcp.exe")).unwrap();
        assert!(remove_from_file(&p, false).unwrap());
        assert!(!remove_from_file(&p, false).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["mcpServers"].get("rapidfind").is_none() && v["mcpServers"]["unity"].is_object());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn vscode_format_new_files_and_refusals() {
        let d = tmp("vscode");
        let p = d.join("sub").join("mcp.json");
        add_to_file(&p, true, Path::new(r"C:\x\rapidfind-mcp.exe")).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["servers"]["rapidfind"]["type"], "stdio");
        // Comments (JSONC) and non-objects are left untouched.
        let c = d.join("commented.json");
        let original = "{ // my servers\n \"servers\": {} }";
        std::fs::write(&c, original).unwrap();
        assert!(
            add_to_file(&c, true, Path::new(r"C:\x.exe"))
                .unwrap_err()
                .contains("isn't plain JSON")
        );
        assert_eq!(std::fs::read_to_string(&c).unwrap(), original);
        let bom = d.join("bom.json");
        std::fs::write(&bom, "\u{feff}{}").unwrap();
        add_to_file(&bom, false, Path::new(r"C:\x.exe")).unwrap();
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn stable_dirs_and_ids() {
        assert!(is_stable_dir(&user_dir()));
        assert!(!is_stable_dir(Path::new(
            r"S:\dev\rapidfind\target\release"
        )));
        if let Some(pf) = env_path("ProgramFiles") {
            assert!(is_stable_dir(&pf.join("rapidfind")));
        }
        for c in ALL {
            assert_eq!(Client::from_id(c.id()), Some(c));
        }
    }
}

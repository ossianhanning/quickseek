//! rapidfind-mcp: a Model Context Protocol server (stdio) over the rapidfind
//! service, for finding files and freeing disk space from an AI assistant.
//!
//! Register it with an MCP client as a stdio server, e.g. for Claude Code:
//! `claude mcp add rapidfind -- C:\path\to\rapidfind-mcp.exe`.
//! It needs the rapidfind service running (or `RAPIDFIND_PIPE`), and runs
//! as the user who starts it: deletes happen with that user's rights.

use rapidfind_mcp::tools;

use std::io::{BufRead, Write};

use rapidfind_core::ipc::{self, Backend};
use serde_json::{Value, json};

const PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

use rapidfind_mcp::guide;

struct Server {
    be: Option<Box<dyn Backend>>,
}

impl Server {
    fn backend(&mut self) -> Result<&mut Box<dyn Backend>, String> {
        if self.be.is_none() {
            let c = ipc::Client::connect().map_err(|e| format!("can't reach the rapidfind service ({e}). Install/start it with `rapidfind service install` from an elevated terminal."))?;
            self.be = Some(Box::new(ipc::Remote(c)));
        }
        Ok(self.be.as_mut().unwrap())
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        let r = tools::call(&mut **self.backend()?, name, args);
        // A broken pipe (service restarted): reconnect once and retry.
        match r {
            Err(e) if e.starts_with("service connection") => {
                self.be = None;
                tools::call(&mut **self.backend()?, name, args)
            }
            r => r,
        }
    }

    /// Answers one JSON-RPC message (`None` for notifications).
    fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let id = id?; // notifications (initialized, cancelled, ...) need no answer
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOLS[0]);
                let version = if PROTOCOLS.contains(&asked) { asked } else { PROTOCOLS[0] };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false }, "prompts": { "listChanged": false } },
                    "serverInfo": { "name": "rapidfind", "title": "rapidfind file search", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": guide::INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(tools::list()),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                Ok(match self.call_tool(name, &args) {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap() }],
                        "structuredContent": v,
                        "isError": false,
                    }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
                })
            }
            "resources/list" => Ok(json!({ "resources": [] })),
            "prompts/list" => Ok(guide::prompts_list()),
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                guide::prompt_get(name, params.get("arguments").unwrap_or(&Value::Null)).map_err(|e| (-32602, e))
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        })
    }
}

fn main() {
    let mut server = Server { be: None };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        // Some shells (Windows PowerShell) start piped input with a byte-order mark.
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(Value::Array(batch)) => {
                let r: Vec<Value> = batch.iter().filter_map(|m| server.handle(m)).collect();
                (!r.is_empty()).then_some(Value::Array(r))
            }
            Ok(msg) => server.handle(&msg),
            Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } })),
        };
        if let Some(r) = reply
            && writeln!(out, "{r}").and_then(|_| out.flush()).is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rapidfind_core::engine::Engine;
    use rapidfind_core::index::Index;
    use rapidfind_core::ntfs::{Meta, Scan, ScanName};

    use super::*;

    /// C:\Users\Me\{Report.PDF 1000, notes.txt 24}, C:\Windows\notepad.exe 200.
    fn sample() -> Index {
        let items: &[(u32, u32, &str, bool, u64)] = &[
            (30, 5, "Users", true, 0),
            (31, 30, "Me", true, 0),
            (32, 31, "Report.PDF", false, 1000),
            (33, 31, "notes.txt", false, 24),
            (34, 5, "Windows", true, 0),
            (35, 34, "notepad.exe", false, 200),
        ];
        let mut s = Scan { has_sizes: true, ..Default::default() };
        s.meta = vec![Meta::default(); 36];
        s.meta[5] = Meta { in_use: true, is_dir: true, ..Default::default() };
        s.names.push(ScanName { record: 5, parent: 5, off: 0, len: 1 });
        s.strings.push(b'.');
        for &(r, p, n, d, size) in items {
            s.meta[r as usize] = Meta { in_use: true, is_dir: d, size, modified: 134_037_072_000_000_000, ..Default::default() };
            s.names.push(ScanName { record: r, parent: p, off: s.strings.len() as u32, len: n.len() as u32 });
            s.strings.extend_from_slice(n.as_bytes());
        }
        Index::from_scan('C', &s)
    }

    fn server() -> Server {
        Server { be: Some(Box::new(ipc::Local::new(Arc::new(Engine::from_indexes(vec![sample()]))))) }
    }

    fn call(s: &mut Server, name: &str, args: Value) -> Value {
        let r = s.handle(&json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": name, "arguments": args } })).unwrap();
        assert_eq!(r["id"], 7);
        assert_eq!(r["result"]["isError"], false, "{r}");
        r["result"]["structuredContent"].clone()
    }

    #[test]
    fn protocol_basics() {
        let mut s = server();
        let init = s.handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26", "capabilities": {} } })).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "rapidfind");
        assert!(s.handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
        let list = s.handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["search_files", "folder_sizes", "folder_growth", "disk_overview", "cleanup_candidates", "delete_paths", "empty_recycle_bin"]);
        assert!(init["result"]["capabilities"]["prompts"].is_object());
        assert!(init["result"]["instructions"].as_str().unwrap().contains("search_files"));
        let prompts = s.handle(&json!({ "jsonrpc": "2.0", "id": 5, "method": "prompts/list" })).unwrap();
        let names: Vec<&str> = prompts["result"]["prompts"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["free_disk_space", "whats_using_space", "whats_growing", "find_files", "find_large_files"]);
        let got = s.handle(&json!({ "jsonrpc": "2.0", "id": 6, "method": "prompts/get", "params": { "name": "free_disk_space", "arguments": { "drive": "D:" } } })).unwrap();
        let text = got["result"]["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains(r"D:\") && text.contains("cleanup_candidates"), "{text}");
        let missing = s.handle(&json!({ "jsonrpc": "2.0", "id": 7, "method": "prompts/get", "params": { "name": "find_files" } })).unwrap();
        assert_eq!(missing["error"]["code"], -32602);
        let unknown = s.handle(&json!({ "jsonrpc": "2.0", "id": 3, "method": "nope" })).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        let bad = s.handle(&json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "search_files", "arguments": { "query": "size:banana" } } })).unwrap();
        assert_eq!(bad["result"]["isError"], true);
    }

    #[test]
    fn search_and_sizes() {
        let mut s = server();
        let r = call(&mut s, "search_files", json!({ "extensions": ["pdf", "txt"], "sort": "size" }));
        assert_eq!(r["total"], 2);
        assert_eq!(r["results"][0]["path"], r"C:\Users\Me\Report.PDF");
        assert_eq!(r["results"][0]["size"], 1000);
        let r = call(&mut s, "search_files", json!({ "query": r"users\*\*.txt" }));
        assert_eq!(r["results"][0]["path"], r"C:\Users\Me\notes.txt");
        let r = call(&mut s, "search_files", json!({ "path": r"C:\Users", "type": "file", "limit": 1, "offset": 1, "sort": "name" }));
        assert_eq!((r["total"].as_u64(), r["results"][0]["path"].as_str(), r["has_more"].as_bool()), (Some(2), Some(r"C:\Users\Me\Report.PDF"), Some(false)));

        let r = call(&mut s, "folder_sizes", json!({ "depth": 3 }));
        let c = &r["children"][0];
        assert_eq!(c["name"], "C:");
        assert_eq!(c["children"][0]["name"], "Users");
        assert_eq!(c["children"][0]["size"], 1024);
        assert_eq!(c["children"][0]["children"][0]["name"], "Me");
        let r = call(&mut s, "folder_sizes", json!({ "path": r"C:\Users\Me", "limit": 1 }));
        assert_eq!(r["children"][0]["name"], "Report.PDF");
        assert_eq!(r["children"][0]["percent_of_parent"], 97.7);
        assert_eq!(r["children"][1]["name"], "(1 more)");
        let r = call(&mut s, "folder_sizes", json!({ "query": "*.exe" }));
        assert_eq!(r["size"], 200);
        let r = call(&mut s, "disk_overview", json!({}));
        assert_eq!(r["volumes"][0]["volume"], "C");
        assert_eq!(r["volumes"][0]["files"], 3);
    }

    #[test]
    fn cleanup_and_delete_dry_run() {
        let mut s = server();
        let r = call(&mut s, "cleanup_candidates", json!({}));
        assert!(r["categories"].is_array());
        let r = call(&mut s, "delete_paths", json!({ "paths": [r"C:\", r"C:\Windows", r"Z:\definitely\not\here.txt"], "dry_run": true }));
        assert_eq!(r["failed"], 3);
        // A real temp file: dry run keeps it, a delete recycles it.
        let dir = std::env::temp_dir().join(format!("rapidfind-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("junk.bin");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        let p = f.to_string_lossy().into_owned();
        let r = call(&mut s, "delete_paths", json!({ "paths": [p], "dry_run": true }));
        assert_eq!((r["failed"].as_u64(), r["results"][0]["size"].as_u64()), (Some(0), Some(4096)));
        assert!(f.exists());
        let r = call(&mut s, "delete_paths", json!({ "paths": [p], "permanent": true }));
        assert_eq!(r["freed_now"], 4096, "{r}");
        assert!(!f.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}

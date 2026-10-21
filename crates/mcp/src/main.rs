//! rapidfind-mcp: a Model Context Protocol server (stdio) over the rapidfind
//! service, for finding files and freeing disk space from an AI assistant.
//!
//! Register it with an MCP client as a stdio server, e.g. for Claude Code:
//! `claude mcp add rapidfind -- C:\path\to\rapidfind-mcp.exe`.
//! It needs the rapidfind service running (or `RAPIDFIND_PIPE`), and runs
//! as the user who starts it: deletes happen with that user's rights.
//!
//! Updates: AI apps keep a server running for as long as they run, so after
//! rapidfind is updated an old server can be left talking to a newer service.
//! The service still speaks the previous protocol version, so that usually
//! just works; when the two share no version, the call fails with a clear
//! error and, if this program's file has been replaced since it started, the
//! server exits so the app starts the new one on the next call. A service that
//! is restarting or not running is not an update: those calls fail (or are
//! retried once) and the server keeps running.

use rapidfind_mcp::tools;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use rapidfind_core::ipc::{self, Backend, Request, Response};
use serde_json::{Value, json};

const PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

use rapidfind_mcp::guide;

/// Exit code after an update was found (EX_TEMPFAIL: start me again).
const EXIT_UPDATED: i32 = 75;
/// How long one service call may take before the tool call fails.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

// What went wrong talking to the service during a tool call (worst wins).
const FINE: u8 = 0;
const BROKEN: u8 = 1;
const TIMED_OUT: u8 = 2;
const MISMATCH: u8 = 3;

/// The service over its pipe. Calls run on a worker thread so that a service
/// that doesn't answer fails the call after a timeout instead of hanging the
/// AI app; problems are recorded in `fault` for [`Server::call_tool`].
struct Link {
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<io::Result<Response>>,
    timeout: Duration,
    fault: Arc<AtomicU8>,
    /// A call timed out: its answer may still come, so the pipe is unusable.
    dead: bool,
}

impl Link {
    fn connect(pipe: &str, timeout: Duration, fault: Arc<AtomicU8>) -> io::Result<Link> {
        let mut c = ipc::Client::connect_to(pipe)?;
        eprintln!(
            "rapidfind-mcp: connected to the service ({}, protocol v{})",
            c.service_build().unwrap_or("an older build"),
            c.protocol()
        );
        let (tx, requests) = mpsc::channel::<Request>();
        let (answers, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for req in requests {
                if answers.send(c.call(&req)).is_err() {
                    break;
                }
            }
        });
        Ok(Link {
            tx,
            rx,
            timeout,
            fault,
            dead: false,
        })
    }

    fn fail(&self, kind: u8, e: io::Error) -> io::Result<Response> {
        self.fault.fetch_max(kind, Ordering::Relaxed);
        Err(e)
    }
}

impl Backend for Link {
    fn call(&mut self, req: &Request) -> io::Result<Response> {
        if self.dead || self.tx.send(req.clone()).is_err() {
            return self.fail(
                BROKEN,
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the connection to the service was lost",
                ),
            );
        }
        match self.rx.recv_timeout(self.timeout) {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) if ipc::is_version_mismatch(&e) => self.fail(MISMATCH, e),
            // A request this service's version doesn't have: not a connection problem.
            Ok(Err(e)) if e.kind() == io::ErrorKind::Unsupported => Err(e),
            Ok(Err(e)) => self.fail(BROKEN, e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.dead = true;
                self.fail(
                    TIMED_OUT,
                    io::Error::new(io::ErrorKind::TimedOut, "the service didn't answer in time"),
                )
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => self.fail(
                BROKEN,
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the connection to the service was lost",
                ),
            ),
        }
    }
    fn label(&self) -> &'static str {
        "service"
    }
}

/// The program file this server started from, to notice it being replaced.
struct ExeStamp {
    path: PathBuf,
    stamp: Option<(u64, SystemTime)>,
}

impl ExeStamp {
    fn of(path: PathBuf) -> ExeStamp {
        let stamp = Self::read(&path);
        ExeStamp { path, stamp }
    }
    fn read(path: &std::path::Path) -> Option<(u64, SystemTime)> {
        std::fs::metadata(path)
            .ok()
            .and_then(|m| Some((m.len(), m.modified().ok()?)))
    }
    /// A different file is there now (an update moved the running one aside).
    fn replaced(&self) -> bool {
        self.stamp.is_some() && Self::read(&self.path).is_some_and(|now| Some(now) != self.stamp)
    }
}

struct Server {
    be: Option<Box<dyn Backend>>,
    pipe: String,
    timeout: Duration,
    fault: Arc<AtomicU8>,
    exe: Option<ExeStamp>,
    /// Exit with this code once the current answer is written.
    exit: Option<i32>,
}

impl Server {
    fn new() -> Server {
        let pipe = std::env::var("RAPIDFIND_PIPE")
            .ok()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| ipc::PIPE_NAME.to_string());
        Server {
            be: None,
            pipe,
            timeout: CALL_TIMEOUT,
            fault: Arc::default(),
            exe: std::env::current_exe().ok().map(ExeStamp::of),
            exit: None,
        }
    }

    fn backend(&mut self) -> Result<&mut Box<dyn Backend>, String> {
        if self.be.is_none() {
            match Link::connect(&self.pipe, self.timeout, self.fault.clone()) {
                Ok(l) => self.be = Some(Box::new(l)),
                Err(e) if ipc::is_version_mismatch(&e) => return Err(self.mismatch(&e.to_string())),
                Err(e) => {
                    return Err(format!(
                        "can't reach the rapidfind service ({e}). Install/start it with `rapidfind service install` from an elevated terminal."
                    ));
                }
            }
        }
        Ok(self.be.as_mut().unwrap())
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        let mut retried = false;
        loop {
            self.fault.store(FINE, Ordering::Relaxed);
            let r = tools::call(&mut **self.backend()?, name, args);
            match self.fault.swap(FINE, Ordering::Relaxed) {
                FINE => return r,
                MISMATCH => {
                    self.be = None;
                    return Err(self.mismatch("the rapidfind service is a different version"));
                }
                TIMED_OUT => {
                    self.be = None;
                    return Err(self.timed_out());
                }
                _ => {
                    // A broken pipe (service restarted): reconnect, and retry once if that failed the call.
                    self.be = None;
                    if r.is_ok() || retried {
                        return r;
                    }
                    retried = true;
                }
            }
        }
    }

    /// The error for a service of another version. If this program's file
    /// has been replaced (rapidfind was updated), also arranges to exit, so
    /// the AI app starts the new one.
    fn mismatch(&mut self, detail: &str) -> String {
        if self.exe.as_ref().is_some_and(ExeStamp::replaced) {
            eprintln!(
                "rapidfind-mcp: {detail}; this server ({}) was updated on disk: exiting so the new one is started",
                ipc::BUILD
            );
            self.exit = Some(EXIT_UPDATED);
            format!(
                "rapidfind was updated and this MCP server is still the old version ({detail}). It is restarting to load the new one: retry the call."
            )
        } else {
            let exe = self
                .exe
                .as_ref()
                .map_or_else(String::new, |x| format!(" ({})", x.path.display()));
            format!(
                "{detail}. Update or reinstall rapidfind (rapidfind-setup.exe) so the service and this MCP server{exe} are the same version, then retry."
            )
        }
    }

    /// The error for a call the service didn't answer in time, with what its
    /// status says (asked on a new connection, briefly).
    fn timed_out(&mut self) -> String {
        let mut msg = format!(
            "the rapidfind service didn't answer within {} s",
            self.timeout.as_secs_f32()
        );
        let status = Link::connect(&self.pipe, Duration::from_secs(3), Arc::default())
            .ok()
            .and_then(|mut l| l.status());
        match status.map(|s| tools::index_warnings(&s)) {
            Some(w) if !w.is_empty() => msg = format!("{msg}: {}", w.join("; ")),
            Some(_) => msg.push_str(" (it may be busy loading or rescanning a drive); try again shortly"),
            None => msg.push_str(" and isn't answering status requests either; restarting it (`rapidfind service stop`, then `start`, elevated) may help"),
        }
        msg
    }

    /// Answers one JSON-RPC message (`None` for notifications).
    fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let id = id?; // notifications (initialized, cancelled, ...) need no answer
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                // Connect now: it logs both versions (and any mismatch) to the app's MCP log.
                if let Err(e) = self.backend() {
                    eprintln!("rapidfind-mcp: {e}");
                }
                self.exit = None; // a server that was just started is the newest there is
                let asked = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOLS[0]);
                let version = if PROTOCOLS.contains(&asked) {
                    asked
                } else {
                    PROTOCOLS[0]
                };
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
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Ok(match self.call_tool(name, &args) {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap() }],
                        "structuredContent": v,
                        "isError": false,
                    }),
                    Err(e) => {
                        json!({ "content": [{ "type": "text", "text": e }], "isError": true })
                    }
                })
            }
            "resources/list" => Ok(json!({ "resources": [] })),
            "prompts/list" => Ok(guide::prompts_list()),
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                guide::prompt_get(name, params.get("arguments").unwrap_or(&Value::Null))
                    .map_err(|e| (-32602, e))
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        })
    }
}

fn main() {
    let mut server = Server::new();
    eprintln!(
        "rapidfind-mcp {} (protocol v{}-v{}) from {}",
        ipc::BUILD,
        ipc::MIN_PROTOCOL,
        ipc::PROTOCOL,
        server
            .exe
            .as_ref()
            .map_or_else(String::new, |x| x.path.display().to_string())
    );
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
            Err(e) => Some(
                json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } }),
            ),
        };
        if let Some(r) = reply
            && writeln!(out, "{r}").and_then(|_| out.flush()).is_err()
        {
            break;
        }
        if let Some(code) = server.exit {
            std::process::exit(code);
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
        let mut s = Scan {
            has_sizes: true,
            ..Default::default()
        };
        s.meta = vec![Meta::default(); 36];
        s.meta[5] = Meta {
            in_use: true,
            is_dir: true,
            ..Default::default()
        };
        s.names.push(ScanName {
            record: 5,
            parent: 5,
            off: 0,
            len: 1,
        });
        s.strings.push(b'.');
        for &(r, p, n, d, size) in items {
            s.meta[r as usize] = Meta {
                in_use: true,
                is_dir: d,
                size,
                modified: 134_037_072_000_000_000,
                ..Default::default()
            };
            s.names.push(ScanName {
                record: r,
                parent: p,
                off: s.strings.len() as u32,
                len: n.len() as u32,
            });
            s.strings.extend_from_slice(n.as_bytes());
        }
        Index::from_scan('C', &s)
    }

    fn server() -> Server {
        Server {
            be: Some(Box::new(ipc::Local::new(Arc::new(Engine::from_indexes(
                vec![sample()],
            ))))),
            ..remote("unused")
        }
    }

    /// A server for the service on `pipe`, started from an unknown file.
    fn remote(pipe: &str) -> Server {
        Server {
            be: None,
            pipe: pipe.into(),
            timeout: Duration::from_millis(500),
            fault: Arc::default(),
            exe: None,
            exit: None,
        }
    }

    fn pipe(tag: &str) -> String {
        format!(r"\\.\pipe\rapidfind-mcp-test-{tag}-{}", std::process::id())
    }

    fn tool_error(s: &mut Server) -> String {
        let r = s.handle(&json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": { "name": "disk_overview", "arguments": {} } })).unwrap();
        assert_eq!(r["result"]["isError"], true, "{r}");
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The first error once the test service's pipe is up.
    fn first_error(s: &mut Server) -> String {
        for _ in 0..100 {
            let e = tool_error(s);
            if !e.contains("can't reach") {
                return e;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        tool_error(s)
    }

    /// A service of another version: exit (to be restarted) only if this
    /// program was replaced on disk; a missing service is never a reason.
    #[test]
    fn version_mismatch_exits_only_after_an_update() {
        let name = pipe("v9");
        ipc::serve_frames(&name, |_| {
            Response::Error("malformed message: protocol version".into()).encode_for(9)
        });
        let exe =
            std::env::temp_dir().join(format!("rapidfind-mcp-test-{}.exe", std::process::id()));
        std::fs::write(&exe, b"old").unwrap();
        let mut s = Server {
            exe: Some(ExeStamp::of(exe.clone())),
            ..remote(&name)
        };
        let e = first_error(&mut s);
        assert!(
            e.contains("are different versions") && e.contains("reinstall rapidfind"),
            "{e}"
        );
        assert_eq!(s.exit, None, "same program file: restarting wouldn't help");
        std::fs::write(&exe, b"new build").unwrap();
        let e = tool_error(&mut s);
        assert!(
            e.contains("was updated") && e.contains("retry the call"),
            "{e}"
        );
        assert_eq!(s.exit, Some(EXIT_UPDATED));
        std::fs::remove_file(&exe).ok();

        let mut s = Server {
            exe: Some(ExeStamp::of(std::env::current_exe().unwrap())),
            ..remote(&pipe("nobody"))
        };
        assert!(tool_error(&mut s).contains("can't reach the rapidfind service"));
        assert_eq!(s.exit, None);
    }

    /// A service that hangs fails the call after the timeout, saying what it can.
    #[test]
    fn stuck_service_times_out() {
        let name = pipe("slow");
        let engine = Arc::new(std::sync::OnceLock::new());
        let _ = engine.set(Arc::new(Engine::from_indexes(vec![sample()])));
        let inner = ipc::engine_handler(engine);
        let factory: ipc::HandlerFactory = Arc::new(move |t| {
            let mut h = inner(t);
            Box::new(move |req: &Request| {
                if !matches!(req, Request::Status) {
                    std::thread::sleep(Duration::from_secs(2));
                }
                h(req)
            })
        });
        let n2 = name.clone();
        std::thread::spawn(move || ipc::serve_on(&n2, factory));
        let mut s = remote(&name);
        let e = first_error(&mut s);
        assert!(
            e.contains("didn't answer within 0.5 s") && e.contains("busy"),
            "{e}"
        );
        assert_eq!(s.exit, None);
        assert!(s.be.is_none(), "reconnects next time");
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
        assert!(
            s.handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .is_none()
        );
        let list = s
            .handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "search_files",
                "folder_sizes",
                "folder_growth",
                "disk_overview",
                "cleanup_candidates",
                "delete_paths",
                "empty_recycle_bin"
            ]
        );
        assert!(init["result"]["capabilities"]["prompts"].is_object());
        assert!(
            init["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("search_files")
        );
        let prompts = s
            .handle(&json!({ "jsonrpc": "2.0", "id": 5, "method": "prompts/list" }))
            .unwrap();
        let names: Vec<&str> = prompts["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "free_disk_space",
                "whats_using_space",
                "whats_growing",
                "find_files",
                "find_large_files"
            ]
        );
        let got = s.handle(&json!({ "jsonrpc": "2.0", "id": 6, "method": "prompts/get", "params": { "name": "free_disk_space", "arguments": { "drive": "D:" } } })).unwrap();
        let text = got["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(
            text.contains(r"D:\") && text.contains("cleanup_candidates"),
            "{text}"
        );
        let missing = s.handle(&json!({ "jsonrpc": "2.0", "id": 7, "method": "prompts/get", "params": { "name": "find_files" } })).unwrap();
        assert_eq!(missing["error"]["code"], -32602);
        let unknown = s
            .handle(&json!({ "jsonrpc": "2.0", "id": 3, "method": "nope" }))
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        let bad = s.handle(&json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "search_files", "arguments": { "query": "size:banana" } } })).unwrap();
        assert_eq!(bad["result"]["isError"], true);
    }

    #[test]
    fn search_and_sizes() {
        let mut s = server();
        let r = call(
            &mut s,
            "search_files",
            json!({ "extensions": ["pdf", "txt"], "sort": "size" }),
        );
        assert_eq!(r["total"], 2);
        assert_eq!(r["results"][0]["path"], r"C:\Users\Me\Report.PDF");
        assert_eq!(r["results"][0]["size"], 1000);
        let r = call(&mut s, "search_files", json!({ "query": r"users\*\*.txt" }));
        assert_eq!(r["results"][0]["path"], r"C:\Users\Me\notes.txt");
        let r = call(
            &mut s,
            "search_files",
            json!({ "path": r"C:\Users", "type": "file", "limit": 1, "offset": 1, "sort": "name" }),
        );
        assert_eq!(
            (
                r["total"].as_u64(),
                r["results"][0]["path"].as_str(),
                r["has_more"].as_bool()
            ),
            (Some(2), Some(r"C:\Users\Me\Report.PDF"), Some(false))
        );

        let r = call(&mut s, "folder_sizes", json!({ "depth": 3 }));
        let c = &r["children"][0];
        assert_eq!(c["name"], "C:");
        assert_eq!(c["children"][0]["name"], "Users");
        assert_eq!(c["children"][0]["size"], 1024);
        assert_eq!(c["children"][0]["children"][0]["name"], "Me");
        let r = call(
            &mut s,
            "folder_sizes",
            json!({ "path": r"C:\Users\Me", "limit": 1 }),
        );
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
        let r = call(
            &mut s,
            "delete_paths",
            json!({ "paths": [r"C:\", r"C:\Windows", r"Z:\definitely\not\here.txt"], "dry_run": true }),
        );
        assert_eq!(r["failed"], 3);
        // A real temp file: dry run keeps it, a delete recycles it.
        let dir = std::env::temp_dir().join(format!("rapidfind-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("junk.bin");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        let p = f.to_string_lossy().into_owned();
        let r = call(
            &mut s,
            "delete_paths",
            json!({ "paths": [p], "dry_run": true }),
        );
        assert_eq!(
            (r["failed"].as_u64(), r["results"][0]["size"].as_u64()),
            (Some(0), Some(4096))
        );
        assert!(f.exists());
        let r = call(
            &mut s,
            "delete_paths",
            json!({ "paths": [p], "permanent": true }),
        );
        assert_eq!(r["freed_now"], 4096, "{r}");
        assert!(!f.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}

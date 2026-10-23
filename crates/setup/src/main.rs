//! quickseek-setup: installs Quickseek (service, GUI, CLI, MCP server) into
//! Program Files, or updates/uninstalls it. Runs elevated (the service needs it).
//!
//! ```text
//! quickseek-setup.exe                       window with options
//! quickseek-setup.exe --silent [--no-mcp]  install with defaults (winget uses this)
//!     --dir <folder> --no-service --no-shortcut --no-path --no-startup --no-launch
//!     --mcp-clients claude-code,cursor      connect only these AI clients
//!     --log <file>                          (default %TEMP%\quickseek-setup.log)
//! quickseek-setup.exe --uninstall [--silent] [--purge]
//! ```
//! Everything is logged to %TEMP%\quickseek-setup.log.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod ui;

use quickseek_pkg::{install, log_line, parse};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = parse(&args);
    log_line(&format!(
        "--- quickseek-setup {} {}",
        install::VERSION,
        args.join(" ")
    ));
    let a = match parsed {
        Ok(a) => a,
        Err(e) => {
            log_line(&e);
            if !args
                .iter()
                .any(|x| x.to_lowercase().contains("silent") || x == "/S")
            {
                ui::message(&e, true);
            }
            std::process::exit(2);
        }
    };
    let code = if a.silent {
        let mut log = |s: &str| log_line(s);
        let r = if a.uninstall {
            install::uninstall(
                &install::Removal {
                    dir: a.opts.dir.clone(),
                    service: a.opts.service,
                    system: a.opts.arp,
                    purge: a.purge,
                },
                &mut log,
            )
        } else {
            install::install(&a.opts, &mut log)
        };
        match r {
            Ok(()) => 0,
            Err(e) => {
                log_line(&format!("error: {e}"));
                1
            }
        }
    } else if a.uninstall {
        ui::uninstall(&a)
    } else {
        ui::run(a)
    };
    std::process::exit(code);
}

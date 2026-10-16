//! `kitae-server` — the headless HTTP API (Track B, tier B3).
//!
//! ```text
//! kitae-server [--bind 127.0.0.1:8787] [--allow <verb>[,<verb>…]|*]
//!              [--viewport WxH] [--scale N] [--evidence-dir DIR]
//! ```
//!
//! With no `--allow`, sessions are read-only. `--allow '*'` permits every
//! verb — the posture a container running a trusted agent wants, and the one
//! the Dockerfile documents.
//!
//! There is no TLS here on purpose: a process talking to `127.0.0.1` (or
//! behind a reverse proxy that terminates TLS) does not need a second TLS
//! stack in the same binary that already links rustls for page fetches.

use std::net::TcpListener;
use std::process::ExitCode;

use kitae_server::{Server, ServerConfig};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut bind = "127.0.0.1:8787".to_string();
    let mut config = ServerConfig::default();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" | "--port" => {
                let Some(value) = args.get(i + 1) else {
                    eprintln!("kitae-server: --bind needs an address");
                    return ExitCode::from(2);
                };
                // `--port 8787` is a convenience for `--bind 0.0.0.0:8787`.
                bind = if args[i] == "--port" {
                    format!("0.0.0.0:{value}")
                } else {
                    value.clone()
                };
                i += 2;
            }
            "--allow" | "--allow-list" => {
                let Some(list) = args.get(i + 1) else {
                    eprintln!("kitae-server: --allow needs a value");
                    return ExitCode::from(2);
                };
                config.allow.extend(
                    list.split([',', ' '])
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                );
                i += 2;
            }
            "--viewport" => {
                let Some(size) = args.get(i + 1) else {
                    eprintln!("kitae-server: --viewport needs WxH");
                    return ExitCode::from(2);
                };
                match size.split_once(['x', 'X']) {
                    Some((w, h)) if w.parse::<f32>().is_ok() && h.parse::<f32>().is_ok() => {
                        config.viewport = (w.parse().unwrap(), h.parse().unwrap());
                    }
                    _ => {
                        eprintln!("kitae-server: --viewport takes WxH, e.g. 1280x800");
                        return ExitCode::from(2);
                    }
                }
                i += 2;
            }
            "--scale" => {
                let Some(value) = args.get(i + 1).and_then(|v| v.parse::<f32>().ok()) else {
                    eprintln!("kitae-server: --scale needs a number");
                    return ExitCode::from(2);
                };
                config.scale = value;
                i += 2;
            }
            "--evidence-dir" => {
                if let Some(dir) = args.get(i + 1) {
                    std::env::set_var("KITAE_EVIDENCE_DIR", dir);
                }
                i += 2;
            }
            "--help" | "-h" => {
                println!(
                    "kitae-server — headless Kitae over HTTP\n\n\
                     USAGE:\n  kitae-server [--bind ADDR | --port N] [--allow <verbs|*>] \
                     [--viewport WxH] [--scale N] [--evidence-dir DIR]\n\n\
                     ROUTES:\n  GET  /health\n  GET  /verbs\n  GET  /sessions\n  \
                     POST /sessions\n  DELETE /sessions/NAME\n  POST /act?session=NAME\n  \
                     POST /batch?session=NAME"
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("kitae-server: unknown argument `{other}` (try --help)");
                return ExitCode::from(2);
            }
        }
    }

    let listener = match TcpListener::bind(&bind) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("kitae-server: cannot bind {bind}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let allow = if config.allow.is_empty() {
        "read-only".to_string()
    } else {
        config.allow.join(",")
    };
    eprintln!("kitae-server listening on {bind} (allow: {allow})");
    let server = Server::new(config);
    if let Err(e) = server.serve(listener) {
        eprintln!("kitae-server: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

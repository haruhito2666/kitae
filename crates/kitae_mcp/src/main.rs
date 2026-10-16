//! `kitae-mcp` — the MCP stdio server (Track B, tier B2).
//!
//! ```text
//! kitae-mcp [--allow <verb>[,<verb>…]] [--viewport 1024x768] [--scale 1.0] [--evidence-dir DIR]
//! ```
//!
//! `--allow` names the verbs a client may use. With no `--allow`, the
//! session is read-only (navigate-less: `read`, `links`, `find`, `extract`,
//! `screenshot`, `pdf`, `verify`). `--allow '*'` permits everything, which
//! is what a container running a trusted agent wants.
//!
//! stdout carries JSON-RPC frames only; progress and errors go to stderr.

use std::process::ExitCode;

use kitae_core::agent::AgentSession;
use kitae_headless::HeadlessRaster;
use kitae_mcp::{policy_from_args, McpServer};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut allow: Vec<String> = Vec::new();
    let mut viewport = (1024.0_f32, 768.0_f32);
    let mut scale = 1.0_f32;
    let mut evidence_dir: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--allow" | "--allow-list" => {
                let Some(list) = args.get(i + 1) else {
                    eprintln!("kitae-mcp: --allow needs a value");
                    return ExitCode::from(2);
                };
                allow.extend(
                    list.split([',', ' '])
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                );
                i += 2;
            }
            "--viewport" => {
                let Some(size) = args.get(i + 1) else {
                    eprintln!("kitae-mcp: --viewport needs WxH");
                    return ExitCode::from(2);
                };
                match size.split_once(['x', 'X']) {
                    Some((w, h)) if w.parse::<f32>().is_ok() && h.parse::<f32>().is_ok() => {
                        viewport = (w.parse().unwrap(), h.parse().unwrap());
                    }
                    _ => {
                        eprintln!("kitae-mcp: --viewport takes WxH, e.g. 1280x800");
                        return ExitCode::from(2);
                    }
                }
                i += 2;
            }
            "--scale" => {
                let Some(value) = args.get(i + 1).and_then(|v| v.parse::<f32>().ok()) else {
                    eprintln!("kitae-mcp: --scale needs a number");
                    return ExitCode::from(2);
                };
                scale = value;
                i += 2;
            }
            "--evidence-dir" => {
                evidence_dir = args.get(i + 1).cloned();
                if let Some(dir) = &evidence_dir {
                    std::env::set_var("KITAE_EVIDENCE_DIR", dir);
                }
                i += 2;
            }
            "--help" | "-h" => {
                println!(
                    "kitae-mcp — MCP server driving a headless Kitae browser\n\n\
                     USAGE:\n  kitae-mcp [--allow <verbs|*>] [--viewport WxH] [--scale N] \
                     [--evidence-dir DIR]\n\n\
                     Verbs: navigate click fill submit back read links find extract watch \
                     dispatch screenshot pdf archive verify"
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("kitae-mcp: unknown argument `{other}` (try --help)");
                return ExitCode::from(2);
            }
        }
    }

    // Layout and rasterization share one font stack, or the text an agent
    // reads about would not be the text on the pixels.
    let session = match HeadlessRaster::load() {
        Ok(raster) => AgentSession::new(kitae_core::Browser::new(
            Box::new(kitae_core::agent::HeadlessHost::default()),
            Box::new(raster.measurer()),
            viewport,
        ))
        .with_raster(std::sync::Arc::new(raster.with_scale(scale)))
        .with_policy(Box::new(policy_from_args(&allow))),
        Err(message) => {
            // No rasterizer: text verbs still work, so the server runs and
            // says what is missing rather than refusing to start.
            eprintln!("kitae-mcp: {message}");
            eprintln!("kitae-mcp: running without screenshots (text verbs are unaffected)");
            AgentSession::headless(viewport).with_policy(Box::new(policy_from_args(&allow)))
        }
    };

    let mut server = McpServer::new(session);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    if let Err(e) = server.serve(stdin.lock(), stdout.lock()) {
        eprintln!("kitae-mcp: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

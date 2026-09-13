//! jay binary entry point.
//!
//! Usage:
//!   jay          — CLI help (default)
//!   jay mcp      — run the MCP stdio server (for agents)
//!   jay <cmd>    — quick CLI commands (init/current/config/ls/new/status/...)
//!
//! Exit codes: 0 = success; 1 = `jay doctor` found integrity errors;
//! 2 = invocation/runtime failure.

use anyhow::Result;

fn main() {
    match real_main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(2);
        }
    }
}

fn real_main() -> Result<i32> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("mcp") {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(jay::mcp::run())?;
        return Ok(0);
    }
    jay::cli::run()
}

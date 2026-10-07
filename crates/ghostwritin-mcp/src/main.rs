//! `ghostwritin-mcp`: the MCP server on stdio.
//!
//! ```text
//! claude mcp add ghostwritin -- ghostwritin-mcp   # with ANTHROPIC_API_KEY or OPENAI_API_KEY set
//! ```

#![forbid(unsafe_code)]

use std::io::{BufRead as _, Write as _};

use ghostwritin_cli::{NO_MODEL, model_from_env};
use ghostwritin_mcp::Server;

fn main() {
    let server = Server::new(model_from_env(), NO_MODEL);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(answer) = server.handle_line(&line)
            && (writeln!(stdout, "{answer}").is_err() || stdout.flush().is_err())
        {
            break;
        }
    }
}

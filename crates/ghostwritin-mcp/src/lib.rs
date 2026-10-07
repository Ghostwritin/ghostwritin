//! A minimal Model Context Protocol server over stdio (newline-delimited
//! JSON-RPC 2.0) exposing one tool, `rewrite`, on the shared engine.
//!
//! The harness's `cratefield-mcp` is a server over the `fz` CLI's own
//! envelopes, not a general MCP library, so this is a small hand-written
//! server: `initialize`, `ping`, `tools/list` and `tools/call`.
//!
//! Text goes only to the model provider whose key is set, and nothing is
//! written anywhere but the JSON-RPC answer on stdout.

#![forbid(unsafe_code)]

use std::sync::Arc;

use cratefield_core::TextModel;
use ghostwritin_core::{GhostwritinError, RewriteRequest, Strength, Voice};
use ghostwritin_rewrite::Engine;
use serde_json::{Value, json};

/// The protocol version answered when the client names none.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The server: the model is optional so `initialize` and `tools/list`
/// work before a key is set, and the tool says what is missing.
pub struct Server {
    engine: Option<Engine>,
    missing_model: &'static str,
}

impl Server {
    pub fn new(model: Option<Arc<dyn TextModel>>, missing_model: &'static str) -> Self {
        Self {
            engine: model.map(Engine::new),
            missing_model,
        }
    }

    /// Answers one line of input; `None` for a notification.
    pub fn handle_line(&self, line: &str) -> Option<String> {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return Some(error(&Value::Null, -32700, "parse error").to_string());
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        // A notification (no id) gets no answer, whatever it is.
        let id = id?;
        let result = match method {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": [tool()] })),
            "tools/call" => self.call(&params),
            _ => Err((-32601, "method not found")),
        };
        Some(
            match result {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err((code, message)) => error(&id, code, message),
            }
            .to_string(),
        )
    }

    fn call(&self, params: &Value) -> Result<Value, (i64, &'static str)> {
        if params.get("name").and_then(Value::as_str) != Some("rewrite") {
            return Err((-32602, "unknown tool"));
        }
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            return Ok(tool_error("`text` is required"));
        };
        let voice = match args.get("voice").and_then(Value::as_str) {
            None => Voice::Professional,
            Some(name) => match Voice::parse(name) {
                // My voice needs a stored style summary: hosted only.
                Some(Voice::MyVoice) | None => {
                    return Ok(tool_error("voice must be casual, professional or academic"));
                }
                Some(voice) => voice,
            },
        };
        let strength = match args.get("strength").and_then(Value::as_str) {
            None => Strength::Edit,
            Some(name) => match Strength::parse(name) {
                Some(strength) => strength,
                None => return Ok(tool_error("strength must be polish, edit or rewrite")),
            },
        };
        let Some(engine) = &self.engine else {
            return Ok(tool_error(self.missing_model));
        };
        let request = RewriteRequest {
            text: text.to_owned(),
            voice,
            strength,
        };
        match pollster::block_on(engine.rewrite(&request, None)) {
            Ok(response) => Ok(json!({
                "content": [{ "type": "text", "text": response.rewrite }],
                "structuredContent": response,
                "isError": false,
            })),
            Err(error) => Ok(tool_error(&explain(&error))),
        }
    }
}

fn explain(error: &GhostwritinError) -> String {
    let mut message = format!("{error} ({})", error.code());
    if let GhostwritinError::MeaningChanged { locks } = error {
        let facts: Vec<&str> = locks.iter().map(|l| l.text.as_str()).collect();
        message.push_str(": ");
        message.push_str(&facts.join(", "));
    }
    message
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "ghostwritin-mcp", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn tool() -> Value {
    json!({
        "name": "rewrite",
        "description": "Rewrite a draft (plain text or Markdown, up to 10,000 words) in a natural voice. \
                        Names, numbers, quotes and code are locked: a rewrite that changes one is \
                        rejected. Returns the rewrite, the locked facts and a word diff.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "The draft." },
                "voice": { "type": "string", "enum": ["casual", "professional", "academic"], "default": "professional" },
                "strength": { "type": "string", "enum": ["polish", "edit", "rewrite"], "default": "edit" }
            },
            "required": ["text"]
        }
    })
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use cratefield_core::{Completion, Prompt, TextModelError};

    use super::*;

    struct Echo;

    #[async_trait]
    impl TextModel for Echo {
        async fn complete(&self, prompt: &Prompt) -> Result<Completion, TextModelError> {
            let user = &prompt.messages[0].content;
            let input: Value =
                serde_json::from_str(&user[user.find("Input:\n").unwrap() + 7..]).unwrap();
            let out: Vec<String> = input["paragraphs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    p.as_str()
                        .unwrap()
                        .replace("It is important to note that w", "W")
                })
                .collect();
            Ok(Completion::new(
                json!({ "paragraphs": out }).to_string(),
                "echo",
            ))
        }
    }

    fn answer(server: &Server, message: &Value) -> Value {
        serde_json::from_str(&server.handle_line(&message.to_string()).unwrap()).unwrap()
    }

    #[test]
    fn handshake_and_tool_list() {
        let server = Server::new(None, "no key");
        let init = answer(
            &server,
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "ghostwritin-mcp");
        assert!(
            server
                .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        let list = answer(
            &server,
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        assert_eq!(list["result"]["tools"][0]["name"], "rewrite");
        let unknown = answer(
            &server,
            &json!({"jsonrpc":"2.0","id":3,"method":"resources/list"}),
        );
        assert_eq!(unknown["error"]["code"], -32601);
        let bad: Value = serde_json::from_str(&server.handle_line("not json").unwrap()).unwrap();
        assert_eq!(bad["error"]["code"], -32700);
    }

    #[test]
    fn rewrite_tool() {
        let server = Server::new(Some(Arc::new(Echo)), "no key");
        let call = |args: Value| {
            answer(
                &server,
                &json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"rewrite","arguments":args}}),
            )
        };
        let ok =
            call(json!({"text": "It is important to note that we grew 18%.", "voice": "casual"}));
        assert_eq!(ok["result"]["isError"], false);
        assert_eq!(ok["result"]["content"][0]["text"], "We grew 18%.");
        assert_eq!(ok["result"]["structuredContent"]["locks"][0]["text"], "18%");

        let my_voice = call(json!({"text": "Hi.", "voice": "my_voice"}));
        assert_eq!(my_voice["result"]["isError"], true);
        let missing = call(json!({"voice": "casual"}));
        assert_eq!(missing["result"]["isError"], true);
    }

    #[test]
    fn without_a_model_the_tool_says_so() {
        let server = Server::new(None, "no model key");
        let out = answer(
            &server,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"rewrite","arguments":{"text":"Hi there."}}}),
        );
        assert_eq!(out["result"]["isError"], true);
        assert_eq!(out["result"]["content"][0]["text"], "no model key");
    }
}

//! The local surfaces' shared wiring: a native [`HttpClient`] over `ureq`
//! and the model chosen from the environment. The `ghostwritin` binary
//! (this crate) and `ghostwritin-mcp` both use it.
//!
//! Locally, text goes only to the provider whose key is set: nothing
//! passes through Ghostwritin's servers.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use cratefield_adapter_anthropic::Anthropic;
use cratefield_adapter_openai_compatible::OpenAiCompatible;
use cratefield_core::{
    DEFAULT_RESPONSE_TIMEOUT, HttpClient, HttpError, HttpPolicy, MAX_RESPONSE_BYTES, SystemClock,
    TextModel,
};

/// The `HttpClient` port over `ureq`, blocking inside the future: the CLI
/// and the MCP server drive one request at a time with `pollster`.
///
/// Honours the request's [`HttpPolicy`] (deadline and body cap). A local
/// process may wait longer than a Worker, so the deadline here is the
/// policy's, with no 30-second clamp (see `LOCAL_TIMEOUT`).
#[derive(Debug, Clone, Copy, Default)]
pub struct UreqClient;

/// The longest a local model call may take. The harness adapters ask for
/// the port's 30-second ceiling; a long chunk can take longer, and a local
/// process has no Worker deadline to respect.
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(180);

#[async_trait]
impl HttpClient for UreqClient {
    async fn send(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        let policy = request.extensions().get::<HttpPolicy>().copied();
        let limit = policy.map_or(MAX_RESPONSE_BYTES, |p| p.max_response_bytes);
        let timeout = policy
            .map_or(DEFAULT_RESPONSE_TIMEOUT, |p| p.timeout)
            .max(LOCAL_TIMEOUT);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build()
            .into();
        let (parts, body) = request.into_parts();
        let request = http::Request::from_parts(parts, body.to_vec());
        let response = agent.run(request).map_err(|error| match error {
            ureq::Error::Timeout(_) => HttpError::DeadlineExceeded { after: timeout },
            // The error text names the host or the I/O failure; it never
            // carries the request body.
            other => HttpError::Transport(other.to_string()),
        })?;
        let (parts, mut body) = response.into_parts();
        let bytes = body
            .with_config()
            .limit(u64::try_from(limit).unwrap_or(u64::MAX))
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => HttpError::ResponseTooLarge { limit },
                other => HttpError::Transport(other.to_string()),
            })?;
        Ok(http::Response::from_parts(parts, Bytes::from(bytes)))
    }
}

/// Which provider answers, from the environment:
///
/// - `ANTHROPIC_API_KEY` (and optional `ANTHROPIC_MODEL`): Anthropic.
/// - else `OPENAI_API_KEY` (optional `OPENAI_MODEL`, `OPENAI_BASE_URL`):
///   `OpenAI` or any compatible endpoint (`OpenRouter`, a local server).
/// - `GHOSTWRITIN_PROVIDER=openai` picks the second when both are set.
///
/// `None` when no key is set. The key goes to the adapter and is never
/// printed.
pub fn model_from_env() -> Option<Arc<dyn TextModel>> {
    let set = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let http: Arc<dyn HttpClient> = Arc::new(UreqClient);
    let clock = Arc::new(SystemClock);
    let prefer_openai =
        set("GHOSTWRITIN_PROVIDER").is_some_and(|p| p.eq_ignore_ascii_case("openai"));
    if !prefer_openai && let Some(key) = set("ANTHROPIC_API_KEY") {
        let model = set("ANTHROPIC_MODEL")
            .unwrap_or_else(|| cratefield_adapter_anthropic::DEFAULT_MODEL.to_owned());
        return Some(Arc::new(Anthropic::new(http, clock, Some(key), model)));
    }
    let key = set("OPENAI_API_KEY")?;
    let model = set("OPENAI_MODEL")
        .unwrap_or_else(|| cratefield_adapter_openai_compatible::DEFAULT_MODEL.to_owned());
    let mut adapter = OpenAiCompatible::new(http, clock, Some(key), model);
    if let Some(base) = set("OPENAI_BASE_URL") {
        adapter = adapter.with_base_url(base);
    }
    Some(Arc::new(adapter))
}

/// The message for a missing model key.
pub const NO_MODEL: &str = "no model key: set ANTHROPIC_API_KEY, or OPENAI_API_KEY (with OPENAI_BASE_URL for a compatible endpoint)";

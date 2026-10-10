//! The open-source Ghostwritin Worker: [`ghostwritin_api`]'s modules on the
//! Cratefield harness, served by the Cloudflare runtime, with the open
//! implementation of every port ([`Services::open`]).
//!
//! - `POST /v1/rewrite`, `GET /v1/health` (see `ghostwritin-api`).
//! - The model: Anthropic when `ANTHROPIC_API_KEY` is set, else an
//!   OpenAI-compatible endpoint when `OPENAI_API_KEY` is (`OPENAI_BASE_URL`
//!   for `OpenRouter`, Workers AI's compatible endpoint, or a self-hosted
//!   server). Neither: `/v1/rewrite` answers `503 model-not-configured`.
//! - API keys: `GHOSTWRITIN_API_KEYS`, `account=sha256hex` pairs. None:
//!   every rewrite is refused.
//!
//! The hosted service does not use this crate: it composes the same
//! modules with its own `Services` in its own (private) Worker.
//!
//! Not deployed. It builds for `wasm32-unknown-unknown` and runs under
//! `wrangler dev --local` (against a scripted model, like the tests); the
//! deploy itself has not been run (see the repository's issues).

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use sha2::{Digest, Sha256};

use cratefield_adapter_anthropic::Anthropic;
use cratefield_adapter_openai_compatible::OpenAiCompatible;
use cratefield_core::{Harness, HarnessBuilder, TextModel, Venture};
use cratefield_runtime_cloudflare::{Cloudflare, FetchClient, WorkersClock, serve};
use ghostwritin_api::{HealthApi, PROBLEM_BASE, RewriteApi, Services};
use ghostwritin_core::ports::{RequestLog, RequestLogger, StaticApiKeys};
use worker::{Context, Env, Request, Response, event};

/// A secret or a var from the Worker's `Env` (`std::env` is empty on
/// Workers), `None` when unset or empty.
fn setting(env: &Env, name: &str) -> Option<String> {
    env.secret(name)
        .map(|s| s.to_string())
        .or_else(|_| env.var(name).map(|v| v.to_string()))
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// The model from the deployment's settings. The key is passed to the
/// adapter and never logged.
fn text_model(env: &Env) -> Option<Arc<dyn TextModel>> {
    let http = Arc::new(FetchClient);
    let clock = Arc::new(WorkersClock);
    if let Some(key) = setting(env, "ANTHROPIC_API_KEY") {
        let model = setting(env, "ANTHROPIC_MODEL")
            .unwrap_or_else(|| cratefield_adapter_anthropic::DEFAULT_MODEL.to_owned());
        return Some(Arc::new(Anthropic::new(http, clock, Some(key), model)));
    }
    let key = setting(env, "OPENAI_API_KEY")?;
    let model = setting(env, "OPENAI_MODEL")
        .unwrap_or_else(|| cratefield_adapter_openai_compatible::DEFAULT_MODEL.to_owned());
    let mut adapter = OpenAiCompatible::new(http, clock, Some(key), model);
    if let Some(base) = setting(env, "OPENAI_BASE_URL") {
        adapter = adapter.with_base_url(base);
    }
    Some(Arc::new(adapter))
}

/// [`RequestLog`]s as one JSON line each on the console, which Workers Logs
/// collects. `tracing` has no subscriber on wasm, so the API's default
/// logger would print nothing here. Metadata only: the record has no text.
struct ConsoleLogger;

impl RequestLogger for ConsoleLogger {
    fn log(&self, entry: &RequestLog) {
        let line = serde_json::json!({
            "event": "rewrite",
            "account": entry.account.as_ref().map(|a| a.0.as_str()),
            "words": entry.words,
            "voice": entry.voice.map(ghostwritin_core::Voice::name),
            "strength": entry.strength.map(ghostwritin_core::Strength::name),
            "status": entry.status,
            "outcome": entry.outcome,
        });
        worker::console_log!("{line}");
    }
}

/// The browser origins that may call the API: the site, where the web
/// app will run.
pub const SITE_ORIGINS: &[&str] = &["https://ghostwrit.in", "https://www.ghostwrit.in"];

/// The venture: api.ghostwrit.in (planned).
pub fn venture() -> Venture {
    Venture::new("ghostwritin", "ghostwrit.in")
        .public_url("https://api.ghostwrit.in")
        .cors_origins(SITE_ORIGINS.iter().copied())
}

/// The modules on the venture, before a runtime is attached.
pub fn compose(services: Services) -> HarnessBuilder {
    Harness::builder()
        .venture(venture())
        .module(RewriteApi::new(services))
        .module(HealthApi)
}

static INSTANCE: OnceLock<(Harness, Cloudflare)> = OnceLock::new();

/// The isolate, built once from the first request's `Env`.
fn instance(env: &Env) -> &'static (Harness, Cloudflare) {
    INSTANCE.get_or_init(|| {
        let mut runtime = Cloudflare::new();
        if let Some(model) = text_model(env) {
            runtime = runtime.text_model_arc(model);
        }
        let keys = StaticApiKeys::parse(&setting(env, "GHOSTWRITIN_API_KEYS").unwrap_or_default());
        let mut services = Services::open(Arc::new(keys));
        services.logger = Arc::new(ConsoleLogger);
        let harness = compose(services)
            .runtime(runtime.clone())
            .build()
            .unwrap_or_else(|error| {
                worker::console_error!("the Ghostwritin harness composition is invalid: {error}");
                panic!("the Ghostwritin harness composition is invalid")
            });
        (harness, runtime)
    })
}

/// The route the Worker throttles in front of the harness, and the
/// `[[ratelimits]]` binding it throttles it with (see `wrangler.toml`).
const REWRITE_ROUTE: &str = "/v1/rewrite";
const LIMITER: &str = "REWRITE_LIMITER";

/// Whether this request is one `POST /v1/rewrite`, the only route the
/// limiter guards (`/v1/health` stays unlimited, and so does everything
/// else the Worker might one day serve).
fn is_limited(method: &str, path: &str) -> bool {
    method.eq_ignore_ascii_case("POST") && path == REWRITE_ROUTE
}

/// The SHA-256 of `input` as lowercase hex. A bearer token is hashed before
/// it becomes a limiter key: keys are readable in the Cloudflare dashboard
/// and in the binding's metrics, and the key must not be the credential.
fn sha256_hex(input: &str) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(input.as_bytes()) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The limiter key for a request: the hash of its bearer token, or the
/// connecting IP when it carries none. `None` when neither is present —
/// an unattributable request (curl on a laptop) is admitted rather than
/// throttled as one anonymous mass.
fn limit_key(authorization: Option<&str>, client_ip: Option<&str>) -> Option<String> {
    let token = authorization
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, key)| key.trim())
        .filter(|key| !key.is_empty());
    match token {
        Some(token) => Some(sha256_hex(token)),
        None => client_ip
            .map(str::trim)
            .filter(|ip| !ip.is_empty())
            .map(str::to_owned),
    }
}

/// The limiter's verdict for a request: `Ok(None)` when it may proceed,
/// `Ok(Some(429))` when its key is over the limit. The check is skipped —
/// also `Ok(None)` — when the binding is not configured (a self-hosted
/// install may remove it) or when the binding errors: a limiter that
/// cannot answer must not take the API down with it.
///
/// # Errors
///
/// Propagates `worker::Error` from reading the request's headers and from
/// building the 429.
async fn rate_limited(env: &Env, req: &Request) -> worker::Result<Option<Response>> {
    if !is_limited(req.method().as_ref(), &req.path()) {
        return Ok(None);
    }
    let authorization = req.headers().get("authorization")?;
    let client_ip = req.headers().get("cf-connecting-ip")?;
    let Some(key) = limit_key(authorization.as_deref(), client_ip.as_deref()) else {
        return Ok(None);
    };
    let admitted = match env.rate_limiter(LIMITER) {
        Ok(limiter) => limiter.limit(key).await.map(|outcome| outcome.success),
        // No such binding: the deployment chose to serve unlimited.
        Err(_) => return Ok(None),
    };
    // An admitted key passes; a limiter that cannot answer must not take
    // the API down with it, so only a definite refusal throttles.
    if admitted.unwrap_or(true) {
        return Ok(None);
    }
    too_many_requests().map(Some)
}

/// The 429 problem, in the API's RFC 9457 shape (a stable `code` on
/// `ghostwritin_api::PROBLEM_BASE`), with `Retry-After` naming the
/// binding's period.
fn too_many_requests() -> worker::Result<Response> {
    let mut response = Response::from_json(&serde_json::json!({
        "type": format!("{PROBLEM_BASE}rate-limited"),
        "title": "too many rewrites from this key; try again shortly",
        "status": 429,
        "code": "rate-limited",
    }))?
    .with_status(429);
    response
        .headers_mut()
        .set("Content-Type", "application/problem+json")?;
    response.headers_mut().set("Retry-After", "60")?;
    Ok(response)
}

/// Worker fetch entry point.
///
/// # Errors
///
/// Propagates `worker::Error` from the limiter and from the harness router.
#[event(fetch)]
pub async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    if let Some(response) = rate_limited(&env, &req).await? {
        return Ok(response);
    }
    let (harness, runtime) = instance(&env);
    serve(harness, runtime, req, env, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostwritin_core::ports::NoApiKeys;

    /// The composition the Worker builds on its first request, checked on
    /// the host so a bad venture or module setting fails here, not as a
    /// panic in the isolate.
    #[test]
    fn the_composition_builds() {
        let built = compose(Services::open(Arc::new(NoApiKeys))).build();
        assert!(built.is_ok(), "{:?}", built.err().map(|e| e.to_string()));
    }

    /// The limiter guards exactly the rewrite route, and its key is the
    /// hashed token — or the connecting IP, never the token itself.
    #[test]
    fn the_limiter_keys_requests_by_token_or_ip() {
        assert!(is_limited("POST", "/v1/rewrite"));
        assert!(is_limited("post", "/v1/rewrite"));
        assert!(!is_limited("GET", "/v1/health"));
        assert!(!is_limited("POST", "/v1/health"));

        let key = limit_key(Some("Bearer gw_secret"), None).expect("a token makes a key");
        assert_eq!(key, sha256_hex("gw_secret"));
        assert!(!key.contains("gw_secret"));
        assert_eq!(
            limit_key(Some("bearer gw_secret"), None).as_deref(),
            Some(key.as_str())
        );
        assert_eq!(limit_key(Some("Basic gw_secret"), None), None);
        assert_eq!(limit_key(Some("Bearer "), None), None);

        assert_eq!(
            limit_key(None, Some("203.0.113.7")).as_deref(),
            Some("203.0.113.7")
        );
        assert_eq!(limit_key(None, Some(" ")), None);
        assert_eq!(limit_key(None, None), None);
    }
}

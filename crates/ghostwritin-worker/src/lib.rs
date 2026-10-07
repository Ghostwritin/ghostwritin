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
//! Not deployed. It builds for `wasm32-unknown-unknown`; it has not been
//! run under `wrangler dev` or deployed (see the repository's issues).

#![forbid(unsafe_code)]

use std::sync::{Arc, OnceLock};

use cratefield_adapter_anthropic::Anthropic;
use cratefield_adapter_openai_compatible::OpenAiCompatible;
use cratefield_core::{Harness, HarnessBuilder, TextModel, Venture};
use cratefield_runtime_cloudflare::{Cloudflare, FetchClient, WorkersClock, serve};
use ghostwritin_api::{HealthApi, RewriteApi, Services};
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

/// Worker fetch entry point.
///
/// # Errors
///
/// Propagates `worker::Error` from the harness router.
#[event(fetch)]
pub async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
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
}

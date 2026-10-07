//! Ghostwritin's HTTP API, as two Cratefield modules a Worker composes:
//!
//! - [`RewriteApi`]: `POST /v1/rewrite` with `{text, voice, strength}`
//!   and an `Authorization: Bearer <key>` header, answering
//!   `{rewrite, score_before, score_after, diff, locks}`.
//! - [`HealthApi`]: `GET /v1/health`, answering whether a model is wired.
//!
//! # Open core
//!
//! Everything beyond the engine comes in through [`Services`], one field
//! per port of `ghostwritin_core::ports`. [`Services::open`] is this
//! repository's: hashed static API keys, no quota, no human score, no
//! watermark detection, no My voice. The hosted service builds the same
//! modules with its own `Services` in its own Worker crate; nothing here
//! needs to change for that.
//!
//! # Data policy
//!
//! The body is read once, parsed, and dropped with the request. The only
//! log record is a [`RequestLog`]: the account, the word count, the voice,
//! the strength, the status and the outcome code. The type has no field for
//! text, so the rewrite, the diff, the locks and parser or provider
//! messages cannot reach a log.
//! Errors answer with fixed messages ([`GhostwritinError`]'s `Display`
//! carries no user text).

#![forbid(unsafe_code)]

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, TextModel};
use ghostwritin_core::ports::{
    ApiKeys, HumanScore, NoHumanScore, NoVoiceStore, NoWatermarkDetector, Quota, RequestLog,
    RequestLogger, Unlimited, VoiceStore, WatermarkDetector,
};
use ghostwritin_core::{GhostwritinError, MAX_WORDS, RewriteRequest, Voice, word_count};
use ghostwritin_rewrite::Engine;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde_json::json;

/// The largest body `POST /v1/rewrite` accepts: 10,000 words of text and
/// the JSON around them, with room for long words and escapes.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// Where problem types are documented.
pub const PROBLEM_BASE: &str = "https://ghostwrit.in/problems/";

/// The ports the API depends on beyond the engine and the model.
#[derive(Clone)]
pub struct Services {
    pub api_keys: Arc<dyn ApiKeys>,
    pub quota: Arc<dyn Quota>,
    pub human_score: Arc<dyn HumanScore>,
    pub watermarks: Arc<dyn WatermarkDetector>,
    pub voices: Arc<dyn VoiceStore>,
    pub logger: Arc<dyn RequestLogger>,
}

impl Services {
    /// The open build: `api_keys` for authentication, and the open
    /// implementation of every other port (no quota, no human score, no
    /// watermark detection, no My voice).
    pub fn open(api_keys: Arc<dyn ApiKeys>) -> Self {
        Self {
            api_keys,
            quota: Arc::new(Unlimited),
            human_score: Arc::new(NoHumanScore),
            watermarks: Arc::new(NoWatermarkDetector),
            voices: Arc::new(NoVoiceStore),
            logger: Arc::new(TracingLogger),
        }
    }
}

/// `POST /v1/rewrite`.
pub struct RewriteApi {
    services: Services,
}

impl RewriteApi {
    pub fn new(services: Services) -> Self {
        Self { services }
    }
}

struct RewriteState {
    ctx: ModuleContext,
    services: Services,
}

impl Module for RewriteApi {
    fn name(&self) -> &'static str {
        "rewrite"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[]
    }

    // Optional so a deployment without a model key still boots and says
    // so (`503 model-not-configured`, and `/v1/health`), rather than failing
    // to build.
    fn optional(&self) -> &'static [Port] {
        &[Port::TextModel]
    }

    fn migrations(&self) -> Migrations {
        Migrations {
            sqlite: &[],
            postgres: &[],
        }
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn max_body_bytes(&self, _cfg: &dyn Config) -> usize {
        MAX_BODY_BYTES
    }

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        let state = Arc::new(RewriteState {
            ctx,
            services: self.services.clone(),
        });
        axum::Router::new()
            .route("/", post(rewrite))
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .with_state(state)
    }
}

/// `GET /v1/health`.
pub struct HealthApi;

impl Module for HealthApi {
    fn name(&self) -> &'static str {
        "health"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[]
    }

    fn optional(&self) -> &'static [Port] {
        &[Port::TextModel]
    }

    fn migrations(&self) -> Migrations {
        Migrations {
            sqlite: &[],
            postgres: &[],
        }
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        let model = ctx.ports.text_model.is_some();
        axum::Router::new().route(
            "/",
            get(move || async move {
                axum::Json(json!({
                    "ok": true,
                    "service": "ghostwritin",
                    "version": env!("CARGO_PKG_VERSION"),
                    "model_configured": model,
                    "max_words": MAX_WORDS,
                }))
            }),
        )
    }
}

async fn rewrite(
    State(state): State<Arc<RewriteState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut log = RequestLog::default();
    let model = state.ctx.ports.text_model.clone();
    let result = handle(&state.services, model, &headers, &body, &mut log).await;
    let response = match result {
        Ok(answer) => (StatusCode::OK, axum::Json(answer)).into_response(),
        Err(error) => problem(&error),
    };
    log.status = response.status().as_u16();
    log.outcome = result_code(&response);
    state.services.logger.log(&log);
    response
}

/// The outcome code of a response this module built.
fn result_code(response: &Response) -> &'static str {
    response
        .extensions()
        .get::<Outcome>()
        .map_or("ok", |outcome| outcome.0)
}

#[derive(Clone, Copy)]
struct Outcome(&'static str);

async fn handle(
    services: &Services,
    model: Option<Arc<dyn TextModel>>,
    headers: &HeaderMap,
    body: &[u8],
    log: &mut RequestLog,
) -> Result<ghostwritin_core::RewriteResponse, GhostwritinError> {
    let key = bearer(headers).ok_or(GhostwritinError::Unauthorized)?;
    let account = services
        .api_keys
        .authenticate(key)
        .await
        .ok_or(GhostwritinError::Unauthorized)?;
    log.account = Some(account.clone());

    let request: RewriteRequest =
        serde_json::from_slice(body).map_err(|_| GhostwritinError::InvalidRequest)?;
    log.words = word_count(&request.text);
    log.voice = Some(request.voice);
    log.strength = Some(request.strength);
    let words = request.validate()?;

    let model = model.ok_or(GhostwritinError::ModelNotConfigured)?;
    let style = match request.voice {
        Voice::MyVoice => Some(
            services
                .voices
                .get(&account)
                .await?
                .ok_or(GhostwritinError::VoiceUnavailable)?,
        ),
        _ => None,
    };
    services.quota.reserve(&account, words).await?;

    Engine::new(model)
        .human_score(services.human_score.clone())
        .watermarks(services.watermarks.clone())
        .rewrite(&request, style.as_ref())
        .await
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, key) = value.split_once(' ')?;
    let key = key.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !key.is_empty()).then_some(key)
}

/// An RFC 9457 problem. `locks` rides along for `meaning-changed`, because
/// the caller needs to know which facts broke; it goes to the caller only.
fn problem(error: &GhostwritinError) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = json!({
        "type": format!("{PROBLEM_BASE}{}", error.code()),
        "title": error.to_string(),
        "status": status.as_u16(),
        "code": error.code(),
    });
    if let GhostwritinError::MeaningChanged { locks } = error {
        body["locks"] = serde_json::to_value(locks).unwrap_or_default();
    }
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    if matches!(error, GhostwritinError::Unauthorized) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response.extensions_mut().insert(Outcome(error.code()));
    response
}

/// [`RequestLog`]s as `tracing` events (target `ghostwritin::rewrite`), for
/// native runtimes and tests. On Workers `tracing` has no subscriber (the
/// harness installs none on wasm), so the Worker uses a console logger.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingLogger;

impl RequestLogger for TracingLogger {
    fn log(&self, entry: &RequestLog) {
        tracing::info!(
            target: "ghostwritin::rewrite",
            account = entry.account.as_ref().map_or("-", |a| a.0.as_str()),
            words = entry.words,
            voice = entry.voice.map_or("-", Voice::name),
            strength = entry.strength.map_or("-", ghostwritin_core::Strength::name),
            status = entry.status,
            outcome = entry.outcome,
            "rewrite"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer  abc "),
        );
        assert_eq!(bearer(&headers), Some("abc"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("bearer "));
        assert_eq!(bearer(&headers), None);
    }
}

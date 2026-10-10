//! `/v1/rewrite` and `/v1/health` through the harness router, with a
//! scripted model.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use cratefield_core::{Completion, Prompt, TextModel, TextModelError};
use cratefield_testing::{TestHarness, conformance};
use ghostwritin_api::{HealthApi, RewriteApi, Services};
use ghostwritin_core::ports::{
    DailyWordLimit, InMemoryVoiceStore, Quota, RequestLog, RequestLogger, Reservation,
    StaticApiKeys, VoiceStore,
};
use ghostwritin_core::{AccountId, GhostwritinError};
use ghostwritin_core::{Strength, Voice};
use http::{HeaderMap, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

/// sha256("test-key")
const KEY_DIGEST: &str = "62af8704764faf8ea82fc61ce9c4c3908b6cb97d463a634e9e587d7c885db0ef";

/// Answers every prompt with `rewrite(paragraph)` for each input paragraph.
struct Model {
    rewrite: fn(&str) -> String,
    calls: Mutex<usize>,
}

#[async_trait]
impl TextModel for Model {
    async fn complete(&self, prompt: &Prompt) -> Result<Completion, TextModelError> {
        *self.calls.lock().unwrap() += 1;
        let user = &prompt.messages[0].content;
        let input: Value =
            serde_json::from_str(&user[user.find("Input:\n").unwrap() + 7..]).unwrap();
        let out: Vec<String> = input["paragraphs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (self.rewrite)(p.as_str().unwrap()))
            .collect();
        Ok(Completion::new(
            json!({ "paragraphs": out }).to_string(),
            "test",
        ))
    }
}

fn polish(text: &str) -> String {
    text.replace("Furthermore, ", "")
        .replace("it is important to note that ", "")
}

fn break_numbers(text: &str) -> String {
    text.replace("18%", "nearly a fifth")
}

fn services() -> Services {
    Services::open(Arc::new(StaticApiKeys::parse(&format!(
        "acct_1={KEY_DIGEST}"
    ))))
}

fn kit_with(services: Services, rewrite: Option<fn(&str) -> String>) -> TestHarness {
    TestHarness::with_ports(
        vec![Box::new(RewriteApi::new(services)), Box::new(HealthApi)],
        move |ports| {
            ports.text_model = rewrite.map(|rewrite| {
                Arc::new(Model {
                    rewrite,
                    calls: Mutex::new(0),
                }) as Arc<dyn TextModel>
            });
        },
    )
}

async fn call(
    kit: &TestHarness,
    method: Method,
    path: &str,
    key: Option<&str>,
    body: &str,
) -> (StatusCode, Value, String) {
    let (status, json, content_type, _) = call_with_headers(kit, method, path, key, body).await;
    (status, json, content_type)
}

/// [`call`], with the response headers (`Retry-After` on a used-up quota).
async fn call_with_headers(
    kit: &TestHarness,
    method: Method,
    path: &str,
    key: Option<&str>,
    body: &str,
) -> (StatusCode, Value, String, HeaderMap) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = kit
        .router
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let response_headers = response.headers().clone();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        content_type,
        response_headers,
    )
}

const DRAFT: &str = "In short, it is important to note that our Q3 revenue grew by 18% to $4.2M. Furthermore, Dana Okafor said “we doubled down on retention.”";

fn body(voice: &str) -> String {
    json!({ "text": DRAFT, "voice": voice, "strength": "polish" }).to_string()
}

#[pollster::test]
async fn health_says_whether_a_model_is_wired() {
    let (status, json, _) = call(
        &kit_with(services(), Some(polish)),
        Method::GET,
        "/v1/health",
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["ok"], true);
    assert_eq!(json["model_configured"], true);
    let (_, json, _) = call(
        &kit_with(services(), None),
        Method::GET,
        "/v1/health",
        None,
        "",
    )
    .await;
    assert_eq!(json["model_configured"], false);
}

#[pollster::test]
async fn a_rewrite() {
    let kit = kit_with(services(), Some(polish));
    let (status, json, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("professional"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["rewrite"],
        "In short, our Q3 revenue grew by 18% to $4.2M. Dana Okafor said “we doubled down on retention.”"
    );
    assert_eq!(json["score_before"], Value::Null);
    assert_eq!(json["score_after"], Value::Null);
    assert_eq!(json["locks"].as_array().unwrap().len(), 5);
    let ops: Vec<&str> = json["diff"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["op"].as_str().unwrap())
        .collect();
    assert!(ops.contains(&"removed") && ops.contains(&"locked") && ops.contains(&"same"));
}

#[pollster::test]
async fn no_key_or_a_wrong_key_is_401() {
    let kit = kit_with(services(), Some(polish));
    for key in [None, Some("wrong-key")] {
        let (status, json, content_type) =
            call(&kit, Method::POST, "/v1/rewrite", key, &body("casual")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["code"], "unauthorized");
        assert_eq!(content_type, "application/problem+json");
    }
}

#[pollster::test]
async fn a_bad_body_is_400_and_never_echoed() {
    let kit = kit_with(services(), Some(polish));
    let (status, json, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        r#"{"text": "Dana Okafor's secret", "voice": "pirate", "strength": "polish"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["code"], "invalid-request");
    assert!(!json.to_string().contains("Okafor"));
    assert!(!json.to_string().contains("pirate"));

    let (status, json, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &json!({"text": "  ", "voice": "casual", "strength": "edit"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["code"], "empty-text");

    let long = "word ".repeat(10_001);
    let (status, json, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &json!({"text": long, "voice": "casual", "strength": "edit"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["code"], "too-many-words");
}

#[pollster::test]
async fn no_model_is_503() {
    let kit = kit_with(services(), None);
    let (status, json, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("casual"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json["code"], "model-not-configured");
}

#[pollster::test]
async fn a_broken_lock_is_422_with_the_locks() {
    let kit = kit_with(services(), Some(break_numbers));
    let (status, json, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("casual"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json["code"], "meaning-changed");
    assert_eq!(json["locks"], json!([{ "kind": "number", "text": "18%" }]));
}

#[pollster::test]
async fn my_voice_uses_the_stored_summary_only() {
    // The open build has no My voice.
    let kit = kit_with(services(), Some(polish));
    let (status, json, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("my_voice"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["code"], "voice-unavailable");

    // A deployment with a voice store and a summary for the account.
    let voices = Arc::new(InMemoryVoiceStore::default());
    voices
        .put(
            &AccountId("acct_1".to_owned()),
            ghostwritin_core::StyleSummary::new("Short sentences.").unwrap(),
        )
        .await
        .unwrap();
    let mut with_voices = services();
    with_voices.voices = voices;
    let kit = kit_with(with_voices, Some(polish));
    let (status, _, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("my_voice"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[pollster::test]
async fn a_quota_is_429() {
    let mut limited = services();
    limited.quota = Arc::new(DailyWordLimit::new(30, || 86_400));
    let kit = kit_with(limited, Some(polish));
    let (status, _, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("casual"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, json, content_type, headers) = call_with_headers(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("casual"),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json["code"], "quota-exceeded");
    assert_eq!(json["resets_at"], "1970-01-03T00:00:00Z");
    assert_eq!(headers.get(header::RETRY_AFTER).unwrap(), "86400");
    assert_eq!(content_type, "application/problem+json");
}

#[derive(Default)]
struct Captured(Mutex<Vec<RequestLog>>);

impl RequestLogger for Captured {
    fn log(&self, entry: &RequestLog) {
        self.0.lock().unwrap().push(entry.clone());
    }
}

#[pollster::test]
async fn one_metadata_record_per_request() {
    let captured = Arc::new(Captured::default());
    let mut logged = services();
    logged.logger = captured.clone();
    let kit = kit_with(logged, Some(polish));
    call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("casual"),
    )
    .await;
    call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        "{not json",
    )
    .await;
    call(&kit, Method::POST, "/v1/rewrite", None, &body("casual")).await;

    let entries = captured.0.lock().unwrap().clone();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].account, Some(AccountId("acct_1".to_owned())));
    assert_eq!(entries[0].words, DRAFT.split_whitespace().count());
    assert_eq!(entries[0].voice, Some(Voice::Casual));
    assert_eq!(entries[0].strength, Some(Strength::Polish));
    assert_eq!((entries[0].status, entries[0].outcome), (200, "ok"));
    assert_eq!(
        (entries[1].status, entries[1].outcome),
        (400, "invalid-request")
    );
    assert_eq!(entries[1].voice, None);
    assert_eq!(
        (entries[2].account.clone(), entries[2].outcome),
        (None, "unauthorized")
    );
}

#[test]
fn modules_conform() {
    conformance(Box::new(RewriteApi::new(services())));
    conformance(Box::new(HealthApi));
}

/// A quota that records what the API did with each reservation, over an
/// in-memory daily limit.
struct RecordingQuota {
    inner: DailyWordLimit,
    calls: Mutex<Vec<&'static str>>,
}

#[async_trait]
impl Quota for RecordingQuota {
    async fn reserve(
        &self,
        account: &AccountId,
        words: usize,
    ) -> Result<Reservation, GhostwritinError> {
        self.calls.lock().unwrap().push("reserve");
        self.inner.reserve(account, words).await
    }

    async fn refund(&self, reservation: &Reservation) -> Result<(), GhostwritinError> {
        self.calls.lock().unwrap().push("refund");
        self.inner.refund(reservation).await
    }

    async fn settle(&self, _reservation: &Reservation) {
        self.calls.lock().unwrap().push("settle");
    }
}

#[pollster::test]
async fn a_failed_rewrite_refunds_and_a_good_one_settles() {
    let quota = Arc::new(RecordingQuota {
        // Room for one draft a day, not two.
        inner: DailyWordLimit::new(30, || 86_400),
        calls: Mutex::new(Vec::new()),
    });
    let mut limited = services();
    limited.quota = quota.clone();

    // The model breaks a locked number: 422, and the words go back.
    let kit = kit_with(limited.clone(), Some(break_numbers));
    let (status, json, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("casual"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert_eq!(*quota.calls.lock().unwrap(), ["reserve", "refund"]);

    // So the same draft still fits, and a good rewrite settles its words.
    let kit = kit_with(limited, Some(polish));
    let (status, json, _) = call(
        &kit,
        Method::POST,
        "/v1/rewrite",
        Some("test-key"),
        &body("casual"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        *quota.calls.lock().unwrap(),
        ["reserve", "refund", "reserve", "settle"]
    );
}

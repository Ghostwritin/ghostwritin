use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use cratefield_core::{Completion, Role};
use ghostwritin_core::ports::HumanScore;
use ghostwritin_core::{DiffOp, LockKind, Score, Strength};

use super::*;

type Reply = Box<dyn Fn(&[String]) -> Result<Vec<String>, TextModelError> + Send + Sync>;

/// A `TextModel` that answers from a script: each reply gets the input
/// paragraphs of the prompt and returns the "rewritten" ones, which are
/// sent back as JSON text the way a provider without native structured
/// output would.
struct Scripted {
    replies: Mutex<VecDeque<Reply>>,
    prompts: Mutex<Vec<Prompt>>,
}

impl Scripted {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into()),
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }

    fn prompt(&self, index: usize) -> Prompt {
        self.prompts.lock().unwrap()[index].clone()
    }
}

fn last_user_text(prompt: &Prompt) -> String {
    let turn = prompt
        .messages
        .iter()
        .find(|turn| turn.role == Role::User)
        .expect("a user turn");
    turn.content.clone()
}

/// The paragraphs of the first user turn's `Input:` JSON.
fn input_paragraphs(prompt: &Prompt) -> Vec<String> {
    let text = last_user_text(prompt);
    let json = &text[text.find("Input:\n").expect("input marker") + 7..];
    let value: serde_json::Value = serde_json::from_str(json).expect("input JSON");
    serde_json::from_value(value["paragraphs"].clone()).expect("paragraphs")
}

#[async_trait]
impl TextModel for Scripted {
    async fn complete(&self, prompt: &Prompt) -> Result<Completion, TextModelError> {
        self.prompts.lock().unwrap().push(prompt.clone());
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("an unexpected model call");
        let paragraphs = reply(&input_paragraphs(prompt))?;
        let json = serde_json::json!({ "paragraphs": paragraphs }).to_string();
        Ok(Completion::new(json, "scripted"))
    }
}

fn reply(f: impl Fn(&[String]) -> Vec<String> + Send + Sync + 'static) -> Reply {
    Box::new(move |input| Ok(f(input)))
}

fn request(text: &str, voice: Voice, strength: Strength) -> RewriteRequest {
    RewriteRequest {
        text: text.to_owned(),
        voice,
        strength,
    }
}

const SITE: &str = "In today's fast-paced digital landscape, it is important to note that our Q3 \
revenue grew by 18% to $4.2M. Furthermore, Dana Okafor stated that put it simply: \
“we doubled down on retention,” which ultimately serves as a testament to the team’s dedication.";

const SITE_REWRITE: &str = "Q3 revenue grew 18% to $4.2M. Dana Okafor put it simply: \
“we doubled down on retention.” The team earned it.";

#[pollster::test]
async fn the_site_example_end_to_end() {
    let model = Scripted::new(vec![reply(|_| vec![SITE_REWRITE.to_owned()])]);
    let engine = Engine::new(model.clone());
    let response = engine
        .rewrite(&request(SITE, Voice::Casual, Strength::Rewrite), None)
        .await
        .unwrap();

    assert_eq!(response.rewrite, SITE_REWRITE);
    assert_eq!(response.locks.len(), 5);
    assert!(response.locks.iter().any(|l| l.kind == LockKind::Quote));
    // Four locked spans came through byte for byte. The quote's closing
    // comma became a full stop, which the lock allows (closing punctuation
    // may move) but the diff shows, so the quote is removed and added.
    let locked = response
        .diff
        .iter()
        .filter(|s| s.op == DiffOp::Locked)
        .count();
    assert_eq!(locked, 4);
    assert!(response.diff.iter().any(|s| s.op == DiffOp::Removed));
    // The open build has no detector: both scores are absent, not invented.
    assert_eq!(response.score_before, None);
    assert_eq!(response.score_after, None);
    assert_eq!(model.calls(), 1);

    let prompt = model.prompt(0);
    assert_eq!(prompt.tier, ModelTier::Strong);
    let system = prompt.system.unwrap();
    assert!(system.contains("Casual"));
    assert!(system.contains("Rewrite: write each paragraph again"));
    // The locked facts are named to the model.
    let task = last_user_text(&model.prompt(0));
    assert!(task.contains("\"Dana Okafor\""));
    assert!(task.contains("\"$4.2M\""));
}

#[pollster::test]
async fn a_broken_lock_is_retried_with_the_fact_named() {
    let model = Scripted::new(vec![
        reply(|_| vec![SITE_REWRITE.replace("18%", "about a fifth")]),
        reply(|_| vec![SITE_REWRITE.to_owned()]),
    ]);
    let response = Engine::new(model.clone())
        .rewrite(&request(SITE, Voice::Professional, Strength::Edit), None)
        .await
        .unwrap();
    assert_eq!(response.rewrite, SITE_REWRITE);
    assert_eq!(model.calls(), 2);

    let retry = model.prompt(1);
    let repair = &retry.messages.last().unwrap().content;
    assert!(
        repair.contains("\"18%\" must appear exactly as written"),
        "{repair}"
    );
    assert_eq!(retry.messages[1].role, Role::Assistant);
}

#[pollster::test]
async fn a_second_failure_is_meaning_changed_with_the_locks() {
    let broken = || {
        reply(|_| {
            vec![
                SITE_REWRITE
                    .replace("18%", "19%")
                    .replace("Dana Okafor", "Dana"),
            ]
        })
    };
    let model = Scripted::new(vec![broken(), broken()]);
    let error = Engine::new(model.clone())
        .rewrite(&request(SITE, Voice::Casual, Strength::Rewrite), None)
        .await
        .unwrap_err();
    let GhostwritinError::MeaningChanged { locks } = error else {
        panic!("expected MeaningChanged, got {error:?}");
    };
    let texts: Vec<_> = locks.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["18%", "Dana Okafor", "19%"]);
    assert_eq!(model.calls(), 2);
}

#[pollster::test]
async fn the_wrong_paragraph_count_is_retried_then_refused() {
    let model = Scripted::new(vec![
        reply(|input| input.iter().chain(input).cloned().collect()),
        reply(|_| Vec::new()),
    ]);
    let error = Engine::new(model.clone())
        .rewrite(
            &request("One.\n\nTwo.", Voice::Casual, Strength::Polish),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error, GhostwritinError::ModelOutput);
    let repair = model.prompt(1).messages.last().unwrap().content.clone();
    assert!(repair.contains("exactly 2 paragraphs"));
}

#[pollster::test]
async fn markdown_structure_never_reaches_the_model() {
    let draft = "# Leveraging Synergy\n\nIt is important to note that we ship.\n\n```sh\necho \"leverage\"\n```\n\n- Furthermore, tests pass.\n";
    let model = Scripted::new(vec![reply(|input| {
        assert_eq!(
            input,
            [
                "It is important to note that we ship.",
                "Furthermore, tests pass."
            ]
        );
        vec!["We ship.".to_owned(), "Tests pass.".to_owned()]
    })]);
    let response = Engine::new(model)
        .rewrite(&request(draft, Voice::Casual, Strength::Polish), None)
        .await
        .unwrap();
    assert_eq!(
        response.rewrite,
        "# Leveraging Synergy\n\nWe ship.\n\n```sh\necho \"leverage\"\n```\n\n- Tests pass.\n"
    );
    // The fenced block is one locked span.
    assert!(response.locks.iter().any(|l| l.kind == LockKind::Code));
}

#[pollster::test]
async fn long_drafts_are_chunked() {
    let paragraph = "This sentence has exactly eight words in it.";
    let draft = [paragraph; 5].join("\n\n");
    let identity = || reply(<[String]>::to_vec);
    let model = Scripted::new(vec![identity(), identity(), identity()]);
    let response = Engine::new(model.clone())
        .chunk_words(16)
        .rewrite(&request(&draft, Voice::Academic, Strength::Edit), None)
        .await
        .unwrap();
    assert_eq!(response.rewrite, draft);
    assert_eq!(model.calls(), 3); // 2 + 2 + 1 paragraphs
}

#[pollster::test]
async fn my_voice_needs_a_style_and_puts_it_in_the_prompt() {
    let model = Scripted::new(vec![reply(<[String]>::to_vec)]);
    let engine = Engine::new(model.clone());
    let req = request("Hello there, friend.", Voice::MyVoice, Strength::Polish);
    assert_eq!(
        engine.rewrite(&req, None).await.unwrap_err(),
        GhostwritinError::VoiceUnavailable
    );
    assert_eq!(model.calls(), 0);

    let style = StyleSummary::new("Short sentences. Dry humour. British spelling.").unwrap();
    engine.rewrite(&req, Some(&style)).await.unwrap();
    assert!(model.prompt(0).system.unwrap().contains("Dry humour"));
}

#[pollster::test]
async fn invalid_requests_never_call_the_model() {
    let model = Scripted::new(Vec::new());
    let engine = Engine::new(model.clone());
    let empty = engine
        .rewrite(&request(" ", Voice::Casual, Strength::Polish), None)
        .await;
    assert_eq!(empty.unwrap_err(), GhostwritinError::EmptyText);
    let long = "word ".repeat(10_001);
    let too_long = engine
        .rewrite(&request(&long, Voice::Casual, Strength::Polish), None)
        .await;
    assert!(matches!(
        too_long.unwrap_err(),
        GhostwritinError::TooManyWords { words: 10_001, .. }
    ));
    assert_eq!(model.calls(), 0);
}

#[pollster::test]
async fn model_errors_map_to_codes_without_provider_text() {
    let model = Scripted::new(vec![Box::new(|_: &[String]| {
        Err(TextModelError::Rejected(
            "the provider quoted: Dana Okafor".to_owned(),
        ))
    })]);
    let error = Engine::new(model)
        .rewrite(&request(SITE, Voice::Casual, Strength::Polish), None)
        .await
        .unwrap_err();
    assert_eq!(error, GhostwritinError::ModelRejected);
    assert!(!error.to_string().contains("Okafor"));
    assert_eq!(
        model_error(&TextModelError::NotConfigured).code(),
        "model-not-configured"
    );
    assert_eq!(
        model_error(&TextModelError::Transient { retry_after: None }).code(),
        "model-unavailable"
    );
}

struct FixedScore;

#[async_trait]
impl HumanScore for FixedScore {
    async fn score(&self, text: &str) -> Option<Score> {
        Some(Score {
            ai_likely: if text.contains("Furthermore") {
                0.92
            } else {
                0.08
            },
            detector: "fixed-test".to_owned(),
            flags: Vec::new(),
        })
    }
}

#[pollster::test]
async fn a_wired_human_score_is_reported_before_and_after() {
    let model = Scripted::new(vec![reply(|_| vec![SITE_REWRITE.to_owned()])]);
    let response = Engine::new(model)
        .human_score(Arc::new(FixedScore))
        .rewrite(&request(SITE, Voice::Casual, Strength::Rewrite), None)
        .await
        .unwrap();
    assert!((response.score_before.unwrap().ai_likely - 0.92).abs() < f32::EPSILON);
    assert!((response.score_after.unwrap().ai_likely - 0.08).abs() < f32::EPSILON);
}

#[test]
fn chunking_and_token_budget() {
    let units = ["a b c", "d e", "f g h i", "j"];
    let groups = chunks(&units, 5);
    assert_eq!(groups, [&units[0..2], &units[2..4]]);
    assert_eq!(chunks(&units, 1).len(), 4);
    assert!(chunks(&[], 5).is_empty());
    assert_eq!(max_tokens(10), 1024);
    assert_eq!(max_tokens(1_000), 3_512);
    assert_eq!(max_tokens(100_000), 8192);
}

//! The request, the response and the values they carry.

use std::collections::BTreeSet;

use cratefield_text_diff::{Op, diff_words_locked};
use cratefield_text_guard::{Guard, Span, SpanKind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::GhostwritinError;

/// The longest draft, in words, one rewrite takes.
pub const MAX_WORDS: usize = 10_000;

/// The longest style summary, in characters: a page of guidance, not a copy
/// of the samples.
pub const MAX_STYLE_SUMMARY_CHARS: usize = 2_000;

/// How many writing samples My voice learns from: three to five.
pub const MIN_SAMPLES: usize = 3;
/// See [`MIN_SAMPLES`].
pub const MAX_SAMPLES: usize = 5;
/// The shortest sample, in words, worth learning a voice from.
pub const MIN_SAMPLE_WORDS: usize = 50;
/// The longest sample, in words. Longer ones teach nothing more.
pub const MAX_SAMPLE_WORDS: usize = 2_000;

/// Words in `text`: runs of non-whitespace.
pub fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Whose voice the rewrite is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Voice {
    /// Plain, warm, contractions, short sentences.
    Casual,
    /// Clear and direct, as for a client or a colleague.
    Professional,
    /// Precise and formal, hedged where the evidence is.
    Academic,
    /// The writer's own, from a [`StyleSummary`] learned from their samples.
    MyVoice,
}

impl Voice {
    /// The name used in JSON, logs and the CLI.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Casual => "casual",
            Self::Professional => "professional",
            Self::Academic => "academic",
            Self::MyVoice => "my_voice",
        }
    }

    /// Parses [`Voice::name`] (and `my-voice`).
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "casual" => Some(Self::Casual),
            "professional" => Some(Self::Professional),
            "academic" => Some(Self::Academic),
            "my_voice" | "my-voice" | "myvoice" => Some(Self::MyVoice),
            _ => None,
        }
    }
}

impl std::fmt::Display for Voice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// How far the rewrite may move from the draft, from a light polish to a
/// full ghostwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    /// Fix what reads as machine-written; keep the sentences.
    Polish,
    /// Rework sentences; keep the paragraph structure and order.
    Edit,
    /// Write it again in the voice, from the same facts and argument.
    Rewrite,
}

impl Strength {
    /// The name used in JSON, logs and the CLI.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Polish => "polish",
            Self::Edit => "edit",
            Self::Rewrite => "rewrite",
        }
    }

    /// Parses [`Strength::name`].
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "polish" => Some(Self::Polish),
            "edit" => Some(Self::Edit),
            "rewrite" => Some(Self::Rewrite),
            _ => None,
        }
    }
}

impl std::fmt::Display for Strength {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// `POST /v1/rewrite`'s body.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RewriteRequest {
    /// The draft, plain text or Markdown, at most [`MAX_WORDS`] words.
    pub text: String,
    pub voice: Voice,
    pub strength: Strength,
}

/// Prints the shape of the request, never its text (data policy).
impl std::fmt::Debug for RewriteRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RewriteRequest")
            .field("words", &word_count(&self.text))
            .field("voice", &self.voice)
            .field("strength", &self.strength)
            .finish()
    }
}

impl RewriteRequest {
    /// Checks the request and returns its word count.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::EmptyText`] for a draft with no words, and
    /// [`GhostwritinError::TooManyWords`] over [`MAX_WORDS`].
    pub fn validate(&self) -> Result<usize, GhostwritinError> {
        let words = word_count(&self.text);
        if words == 0 {
            return Err(GhostwritinError::EmptyText);
        }
        if words > MAX_WORDS {
            return Err(GhostwritinError::TooManyWords {
                words,
                max: MAX_WORDS,
            });
        }
        Ok(words)
    }
}

/// How a writer writes, learned from their samples: what My voice keeps.
/// The samples themselves are never kept (data policy).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct StyleSummary(String);

impl StyleSummary {
    /// # Errors
    ///
    /// [`GhostwritinError::InvalidStyleSummary`] when it is empty or longer
    /// than [`MAX_STYLE_SUMMARY_CHARS`].
    pub fn new(summary: impl Into<String>) -> Result<Self, GhostwritinError> {
        let summary = summary.into().trim().to_owned();
        let chars = summary.chars().count();
        if chars == 0 || chars > MAX_STYLE_SUMMARY_CHARS {
            return Err(GhostwritinError::InvalidStyleSummary { chars });
        }
        Ok(Self(summary))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A summary is about a person's writing: printed as its length only.
impl std::fmt::Debug for StyleSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StyleSummary({} chars)", self.0.chars().count())
    }
}

/// An account, as the hosted service or a self-hosted key list names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub String);

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a [`Lock`] protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockKind {
    Name,
    Number,
    Quote,
    Code,
}

impl From<SpanKind> for LockKind {
    fn from(kind: SpanKind) -> Self {
        match kind {
            SpanKind::Name => Self::Name,
            SpanKind::Number => Self::Number,
            SpanKind::Quote => Self::Quote,
            SpanKind::Code => Self::Code,
        }
    }
}

/// A fact the rewrite had to keep exactly as written.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Lock {
    pub kind: LockKind,
    pub text: String,
}

impl From<&Span> for Lock {
    fn from(span: &Span) -> Self {
        Self {
            kind: span.kind.into(),
            text: span.text.clone(),
        }
    }
}

/// The distinct locks of `text`, in order of first appearance.
pub fn locks_of(text: &str) -> Vec<Lock> {
    let mut seen = BTreeSet::new();
    Guard::new()
        .extract(text)
        .iter()
        .map(Lock::from)
        .filter(|lock| seen.insert(lock.clone()))
        .collect()
}

/// What happened to a run of text between the draft and the rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    /// Cut from the draft.
    Removed,
    /// Written in the voice.
    Added,
    /// A locked fact, unchanged.
    Locked,
    /// Kept as it was.
    Same,
}

/// One run of the word diff.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DiffSegment {
    pub op: DiffOp,
    pub text: String,
}

/// The word diff of a draft and its rewrite, locked facts kept whole.
pub fn diff(original: &str, rewrite: &str) -> Vec<DiffSegment> {
    let guard = Guard::new();
    diff_words_locked(
        original,
        rewrite,
        &guard.ranges(original),
        &guard.ranges(rewrite),
    )
    .into_iter()
    .map(|segment| DiffSegment {
        op: match segment.op {
            Op::Removed => DiffOp::Removed,
            Op::Added => DiffOp::Added,
            Op::Locked => DiffOp::Locked,
            Op::Same => DiffOp::Same,
        },
        text: segment.text,
    })
    .collect()
}

/// A human score: how likely a detector is to read the text as written by
/// a model, from 0.0 (reads as human) to 1.0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub ai_likely: f32,
    /// Which detector said so.
    pub detector: String,
}

/// A watermark a detector found (hosted only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Watermark {
    /// The model family the watermark belongs to.
    pub model: String,
    pub confidence: f32,
}

/// `POST /v1/rewrite`'s answer.
///
/// `score_before` and `score_after` are `null` when no detector is wired:
/// the open-source build has none (see `ports::NoHumanScore`).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RewriteResponse {
    pub rewrite: String,
    pub score_before: Option<Score>,
    pub score_after: Option<Score>,
    pub diff: Vec<DiffSegment>,
    pub locks: Vec<Lock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watermarks: Vec<Watermark>,
}

/// Prints the shape of the response, never its text (data policy).
impl std::fmt::Debug for RewriteResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RewriteResponse")
            .field("words", &word_count(&self.rewrite))
            .field("diff_segments", &self.diff.len())
            .field("locks", &self.locks.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        let request = |text: &str| RewriteRequest {
            text: text.to_owned(),
            voice: Voice::Casual,
            strength: Strength::Polish,
        };
        assert_eq!(
            request("  \n ").validate(),
            Err(GhostwritinError::EmptyText)
        );
        assert_eq!(request("two words").validate(), Ok(2));
        let long = "word ".repeat(MAX_WORDS + 1);
        assert_eq!(
            request(&long).validate(),
            Err(GhostwritinError::TooManyWords {
                words: MAX_WORDS + 1,
                max: MAX_WORDS
            })
        );
        assert_eq!(
            request(&"word ".repeat(MAX_WORDS)).validate(),
            Ok(MAX_WORDS)
        );
    }

    #[test]
    fn debug_never_prints_text() {
        let request = RewriteRequest {
            text: "Dana Okafor's secret plan".to_owned(),
            voice: Voice::MyVoice,
            strength: Strength::Rewrite,
        };
        let printed = format!("{request:?}");
        assert!(!printed.contains("Okafor"), "{printed}");
        assert!(printed.contains("words: 4"));
        let summary = StyleSummary::new("Short sentences. Dry humour.").unwrap();
        assert_eq!(format!("{summary:?}"), "StyleSummary(28 chars)");
    }

    #[test]
    fn wire_names() {
        let request: RewriteRequest =
            serde_json::from_str(r#"{"text":"Hi there","voice":"my_voice","strength":"polish"}"#)
                .unwrap();
        assert_eq!(request.voice, Voice::MyVoice);
        assert_eq!(Voice::parse("my-voice"), Some(Voice::MyVoice));
        assert_eq!(Strength::parse("Rewrite"), Some(Strength::Rewrite));
        assert_eq!(Voice::parse("pirate"), None);
    }

    #[test]
    fn style_summary_bounds() {
        assert!(StyleSummary::new("   ").is_err());
        assert!(StyleSummary::new("x".repeat(MAX_STYLE_SUMMARY_CHARS + 1)).is_err());
        assert!(StyleSummary::new("x".repeat(MAX_STYLE_SUMMARY_CHARS)).is_ok());
    }

    #[test]
    fn locks_and_diff_of_the_site_example() {
        let draft =
            "Our Q3 revenue grew by 18% to $4.2M. Dana Okafor said “we doubled down on retention.”";
        let rewrite = "Q3 revenue grew 18% to $4.2M. Dana Okafor: “we doubled down on retention.” The team earned it.";
        let locks = locks_of(draft);
        assert_eq!(locks.len(), 5);
        assert_eq!(
            locks[3],
            Lock {
                kind: LockKind::Name,
                text: "Dana Okafor".to_owned()
            }
        );
        let segments = diff(draft, rewrite);
        let locked = segments.iter().filter(|s| s.op == DiffOp::Locked).count();
        assert_eq!(locked, 5);
        let rebuilt: String = segments
            .iter()
            .filter(|s| s.op != DiffOp::Removed)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(rebuilt, rewrite);
    }

    #[test]
    fn response_json_shape() {
        let response = RewriteResponse {
            rewrite: "x".to_owned(),
            score_before: None,
            score_after: None,
            diff: vec![DiffSegment {
                op: DiffOp::Same,
                text: "x".to_owned(),
            }],
            locks: vec![],
            watermarks: vec![],
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "rewrite": "x",
                "score_before": null,
                "score_after": null,
                "diff": [{"op": "same", "text": "x"}],
                "locks": []
            })
        );
    }
}

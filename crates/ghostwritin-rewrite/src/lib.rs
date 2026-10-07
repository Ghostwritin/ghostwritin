//! The Ghostwritin engine, shared by the API, the CLI and the MCP server.
//!
//! [`Engine::rewrite`] takes a [`RewriteRequest`] (and, for My voice, a
//! [`StyleSummary`]) and returns a [`RewriteResponse`]:
//!
//! 1. Validates the request (non-empty, at most 10,000 words).
//! 2. Splits the draft into prose units and Markdown structure
//!    ([`markdown`]): code fences, headings, tables and the like never reach
//!    the model and come back byte for byte.
//! 3. Rewrites the prose in chunks of about [`DEFAULT_CHUNK_WORDS`] words,
//!    one [`TextModel`] call per chunk through the harness's structured
//!    output (`TextModelExt::complete_as`), so each call stays inside the
//!    harness's 30-second HTTP ceiling.
//! 4. Checks the meaning lock per unit (`cratefield-text-guard`): every
//!    name, number, quote and code span of the draft must survive verbatim,
//!    and no number may be invented. On a failure it retries the chunk once,
//!    naming the broken facts; a second failure is
//!    [`GhostwritinError::MeaningChanged`] and nothing is returned.
//! 5. Builds the word diff with locked spans (`cratefield-text-diff`), the
//!    locks, and the before/after [`HumanScore`]s.
//!
//! The engine keeps nothing and logs nothing: text lives in this call's
//! memory only. Callers log [`GhostwritinError::code`] and counts.

#![forbid(unsafe_code)]

pub mod markdown;
mod prompt;

use std::sync::Arc;

use cratefield_core::{ModelTier, Prompt, TextModel, TextModelError, TextModelExt, Turn};
use cratefield_text_guard::{Guard, Violation};
use ghostwritin_core::ports::{HumanScore, NoHumanScore, NoWatermarkDetector, WatermarkDetector};
use ghostwritin_core::{
    GhostwritinError, Lock, RewriteRequest, RewriteResponse, StyleSummary, Voice, diff, locks_of,
    word_count,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::markdown::Piece;

/// Words of prose per model call.
pub const DEFAULT_CHUNK_WORDS: usize = 600;

/// The answer the model is held to.
#[derive(Debug, Deserialize, JsonSchema)]
struct Rewritten {
    paragraphs: Vec<String>,
}

/// The rewrite engine. Cheap to clone; share one per process.
#[derive(Clone)]
pub struct Engine {
    model: Arc<dyn TextModel>,
    human_score: Arc<dyn HumanScore>,
    watermarks: Arc<dyn WatermarkDetector>,
    tier: ModelTier,
    chunk_words: usize,
}

impl Engine {
    /// An engine over `model`, with no human score and no watermark
    /// detection (the open build), asking for the strong tier.
    pub fn new(model: Arc<dyn TextModel>) -> Self {
        Self {
            model,
            human_score: Arc::new(NoHumanScore),
            watermarks: Arc::new(NoWatermarkDetector),
            tier: ModelTier::Strong,
            chunk_words: DEFAULT_CHUNK_WORDS,
        }
    }

    /// The human-score port (hosted: a real detector).
    #[must_use]
    pub fn human_score(mut self, human_score: Arc<dyn HumanScore>) -> Self {
        self.human_score = human_score;
        self
    }

    /// The watermark port (hosted only).
    #[must_use]
    pub fn watermarks(mut self, watermarks: Arc<dyn WatermarkDetector>) -> Self {
        self.watermarks = watermarks;
        self
    }

    /// Which model tier to ask for (default: strong).
    #[must_use]
    pub fn tier(mut self, tier: ModelTier) -> Self {
        self.tier = tier;
        self
    }

    /// Words of prose per model call (default [`DEFAULT_CHUNK_WORDS`]).
    #[must_use]
    pub fn chunk_words(mut self, words: usize) -> Self {
        self.chunk_words = words.max(1);
        self
    }

    /// Rewrites `request.text` in its voice and strength.
    ///
    /// `style` is required for [`Voice::MyVoice`] and ignored otherwise.
    ///
    /// # Errors
    ///
    /// Validation errors before any model call; [`GhostwritinError::VoiceUnavailable`]
    /// for My voice without a style; the model's failures as
    /// `Model*` codes; [`GhostwritinError::MeaningChanged`] when a chunk
    /// broke the meaning lock twice.
    pub async fn rewrite(
        &self,
        request: &RewriteRequest,
        style: Option<&StyleSummary>,
    ) -> Result<RewriteResponse, GhostwritinError> {
        request.validate()?;
        let style = match request.voice {
            Voice::MyVoice => Some(style.ok_or(GhostwritinError::VoiceUnavailable)?),
            _ => None,
        };

        let pieces = markdown::split(&request.text);
        let units: Vec<&str> = pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Prose { text, .. } => Some(text.as_str()),
                Piece::Keep(_) => None,
            })
            .collect();

        let system = prompt::system(request.voice, request.strength, style);
        let mut rewritten = Vec::with_capacity(units.len());
        for chunk in chunks(&units, self.chunk_words) {
            rewritten.extend(self.rewrite_chunk(&system, chunk).await?);
        }
        let rewrite = markdown::join(&pieces, &rewritten);

        Ok(RewriteResponse {
            score_before: self.human_score.score(&request.text).await,
            score_after: self.human_score.score(&rewrite).await,
            watermarks: self.watermarks.detect(&request.text).await,
            diff: diff(&request.text, &rewrite),
            locks: locks_of(&request.text),
            rewrite,
        })
    }

    /// One model call (and at most one retry) for a chunk of units.
    async fn rewrite_chunk(
        &self,
        system: &str,
        units: &[&str],
    ) -> Result<Vec<String>, GhostwritinError> {
        let words: usize = units.iter().map(|u| word_count(u)).sum();
        let mut prompt = Prompt::new(self.tier)
            .system(system)
            .user(prompt::task(units))
            .max_tokens(max_tokens(words));

        let first = self.call(prompt.clone()).await?;
        let (failures, mismatch) = check(units, &first);
        if failures.is_empty() && mismatch.is_none() {
            return Ok(first);
        }

        // One retry, naming what broke. The previous answer is quoted back
        // so the model edits rather than starts over.
        let previous = serde_json::json!({ "paragraphs": first }).to_string();
        prompt = prompt
            .turn(Turn::assistant(previous))
            .turn(Turn::user(prompt::repair(&failures, mismatch)));
        let second = self.call(prompt).await?;
        let (failures, mismatch) = check(units, &second);
        if mismatch.is_some() {
            return Err(GhostwritinError::ModelOutput);
        }
        if !failures.is_empty() {
            let mut locks: Vec<Lock> = Vec::new();
            for violation in failures.iter().flat_map(|(_, v)| v) {
                let lock = Lock::from(&violation.span);
                if !locks.contains(&lock) {
                    locks.push(lock);
                }
            }
            return Err(GhostwritinError::MeaningChanged { locks });
        }
        Ok(second)
    }

    async fn call(&self, prompt: Prompt) -> Result<Vec<String>, GhostwritinError> {
        self.model
            .complete_as::<Rewritten>(prompt)
            .await
            .map(|answer| answer.paragraphs)
            .map_err(|error| model_error(&error))
    }
}

/// Units grouped so each group has at most `budget` words (a unit longer
/// than the budget is a group of its own).
fn chunks<'a, 'b>(units: &'b [&'a str], budget: usize) -> Vec<&'b [&'a str]> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut words = 0;
    for (index, unit) in units.iter().enumerate() {
        let count = word_count(unit);
        if index > start && words + count > budget {
            groups.push(&units[start..index]);
            start = index;
            words = 0;
        }
        words += count;
    }
    if start < units.len() {
        groups.push(&units[start..]);
    }
    groups
}

/// Room for the rewrite plus the JSON around it.
fn max_tokens(words: usize) -> u32 {
    let wanted = words.saturating_mul(3).saturating_add(512);
    u32::try_from(wanted.clamp(1024, 8192)).unwrap_or(8192)
}

type Failures = Vec<(usize, Vec<Violation>)>;

/// The units whose rewrite broke the lock, and the expected count if the
/// model returned the wrong number of paragraphs.
fn check(units: &[&str], rewritten: &[String]) -> (Failures, Option<usize>) {
    if rewritten.len() != units.len() {
        return (Vec::new(), Some(units.len()));
    }
    let guard = Guard::new();
    let failures = units
        .iter()
        .zip(rewritten)
        .enumerate()
        .filter_map(|(index, (unit, new))| guard.verify(unit, new).err().map(|v| (index, v)))
        .collect();
    (failures, None)
}

/// The port's errors as Ghostwritin's. The provider's text (which can
/// quote the prompt) is dropped here, never carried on.
fn model_error(error: &TextModelError) -> GhostwritinError {
    match error {
        TextModelError::NotConfigured => GhostwritinError::ModelNotConfigured,
        TextModelError::Rejected(_)
        | TextModelError::Unsupported(_)
        | TextModelError::ImageLimit(_) => GhostwritinError::ModelRejected,
        TextModelError::SchemaViolation(_) | TextModelError::InvalidSchema(_) => {
            GhostwritinError::ModelOutput
        }
        _ => GhostwritinError::ModelUnavailable,
    }
}

#[cfg(test)]
mod tests;

//! My voice: learn how a writer writes from three to five samples of their
//! own work, as a short **style summary** the rewrite prompt then follows.
//!
//! Only the summary is kept. [`VoiceBuilder::learn`] takes the samples by
//! value, sends them to the model once, and drops them; what reaches the
//! [`VoiceStore`] is the [`StyleSummary`] alone. A summary that quotes a
//! sample (eight or more words in a row) is refused, so the summary cannot
//! become a copy of the writing it describes.
//!
//! This is the open, basic builder: one model call per voice. The hosted
//! service runs My voice at scale behind the same [`VoiceStore`] port.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::sync::Arc;

use cratefield_core::{ModelTier, Prompt, TextModel, TextModelError, TextModelExt};
use ghostwritin_core::ports::VoiceStore;
use ghostwritin_core::{
    AccountId, GhostwritinError, MAX_SAMPLE_WORDS, MAX_SAMPLES, MIN_SAMPLE_WORDS, MIN_SAMPLES,
    StyleSummary, word_count,
};
use schemars::JsonSchema;
use serde::Deserialize;

/// How many words in a row, copied from a sample, make a summary a quote.
pub const QUOTE_WORDS: usize = 8;

const SYSTEM: &str = "You describe how a person writes, so another writer can imitate the \
voice without seeing the samples. Describe: typical sentence length and rhythm; vocabulary \
(plain or technical, British or American spelling); punctuation habits (dashes, semicolons, \
exclamation marks, Oxford comma); tone and formality; how paragraphs open and close; humour; \
recurring quirks. Write instructions in the second person (\"Use short sentences.\"). Do not \
quote the samples, and do not mention any name, number, place, topic or fact from them: the \
summary describes style, never content. At most 1,500 characters. Answer with JSON only: \
{\"summary\": \"...\"}.";

#[derive(Debug, Deserialize, JsonSchema)]
struct Learned {
    summary: String,
}

/// Builds style summaries through a [`TextModel`].
#[derive(Clone)]
pub struct VoiceBuilder {
    model: Arc<dyn TextModel>,
    tier: ModelTier,
}

impl VoiceBuilder {
    /// A builder over `model`, asking for the strong tier.
    pub fn new(model: Arc<dyn TextModel>) -> Self {
        Self {
            model,
            tier: ModelTier::Strong,
        }
    }

    /// Which model tier to ask for (default: strong).
    #[must_use]
    pub fn tier(mut self, tier: ModelTier) -> Self {
        self.tier = tier;
        self
    }

    /// The style summary of `samples`.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::InvalidSamples`] for the wrong number of samples
    /// or a sample that is too short or too long (before any model call);
    /// [`GhostwritinError::ModelOutput`] for a summary that is empty, too
    /// long, or quotes a sample; the model's own failures otherwise.
    pub async fn summarise(&self, samples: &[String]) -> Result<StyleSummary, GhostwritinError> {
        check_samples(samples)?;
        let mut prompt = Prompt::new(self.tier).system(SYSTEM).max_tokens(1024);
        for (index, sample) in samples.iter().enumerate() {
            prompt = prompt.user(format!("Sample {}:\n{sample}", index + 1));
        }
        let learned: Learned = self
            .model
            .complete_as(prompt)
            .await
            .map_err(|error| model_error(&error))?;
        if quotes_a_sample(&learned.summary, samples) {
            return Err(GhostwritinError::ModelOutput);
        }
        StyleSummary::new(learned.summary).map_err(|_| GhostwritinError::ModelOutput)
    }

    /// Builds the summary of `samples`, stores it for `account`, and drops
    /// the samples. Returns the stored summary.
    ///
    /// # Errors
    ///
    /// Those of [`VoiceBuilder::summarise`] and of the store's `put`.
    pub async fn learn(
        &self,
        store: &dyn VoiceStore,
        account: &AccountId,
        samples: Vec<String>,
    ) -> Result<StyleSummary, GhostwritinError> {
        let summary = self.summarise(&samples).await?;
        drop(samples);
        store.put(account, summary.clone()).await?;
        Ok(summary)
    }
}

fn check_samples(samples: &[String]) -> Result<(), GhostwritinError> {
    if !(MIN_SAMPLES..=MAX_SAMPLES).contains(&samples.len()) {
        return Err(GhostwritinError::InvalidSamples(
            "My voice learns from 3 to 5 samples",
        ));
    }
    for sample in samples {
        let words = word_count(sample);
        if words < MIN_SAMPLE_WORDS {
            return Err(GhostwritinError::InvalidSamples(
                "each sample needs at least 50 words",
            ));
        }
        if words > MAX_SAMPLE_WORDS {
            return Err(GhostwritinError::InvalidSamples(
                "each sample may have at most 2,000 words",
            ));
        }
    }
    Ok(())
}

fn normalised_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

/// Whether `summary` repeats [`QUOTE_WORDS`] words in a row from a sample.
fn quotes_a_sample(summary: &str, samples: &[String]) -> bool {
    let mut shingles = HashSet::new();
    for sample in samples {
        for window in normalised_words(sample).windows(QUOTE_WORDS) {
            shingles.insert(window.join(" "));
        }
    }
    normalised_words(summary)
        .windows(QUOTE_WORDS)
        .any(|window| shingles.contains(&window.join(" ")))
}

fn model_error(error: &TextModelError) -> GhostwritinError {
    match error {
        TextModelError::NotConfigured => GhostwritinError::ModelNotConfigured,
        TextModelError::Rejected(_) | TextModelError::Unsupported(_) => {
            GhostwritinError::ModelRejected
        }
        TextModelError::SchemaViolation(_) | TextModelError::InvalidSchema(_) => {
            GhostwritinError::ModelOutput
        }
        _ => GhostwritinError::ModelUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use cratefield_core::Completion;
    use ghostwritin_core::ports::InMemoryVoiceStore;

    use super::*;

    struct Fixed {
        summary: String,
        prompts: Mutex<Vec<Prompt>>,
    }

    impl Fixed {
        fn new(summary: &str) -> Arc<Self> {
            Arc::new(Self {
                summary: summary.to_owned(),
                prompts: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl TextModel for Fixed {
        async fn complete(&self, prompt: &Prompt) -> Result<Completion, TextModelError> {
            self.prompts.lock().unwrap().push(prompt.clone());
            let json = serde_json::json!({ "summary": self.summary }).to_string();
            Ok(Completion::new(json, "fixed"))
        }
    }

    fn sample(seed: &str) -> String {
        format!("{seed} ") + &"walked down to the harbour again and counted the boats ".repeat(6)
    }

    fn samples(n: usize) -> Vec<String> {
        (0..n).map(|i| sample(&format!("Sample{i}"))).collect()
    }

    const SUMMARY: &str = "Use short sentences. Prefer plain words. British spelling. \
                           Dashes, not semicolons. Dry humour at paragraph ends.";

    #[pollster::test]
    async fn learn_stores_the_summary_only() {
        let model = Fixed::new(SUMMARY);
        let store = InMemoryVoiceStore::default();
        let me = AccountId("me".to_owned());
        let summary = VoiceBuilder::new(model.clone())
            .learn(&store, &me, samples(3))
            .await
            .unwrap();
        assert_eq!(summary.as_str(), SUMMARY);
        assert_eq!(store.get(&me).await.unwrap(), Some(summary));
        let prompt = model.prompts.lock().unwrap()[0].clone();
        assert_eq!(prompt.messages.len(), 3);
        assert!(prompt.system.unwrap().contains("never content"));
    }

    #[pollster::test]
    async fn sample_rules_are_checked_before_the_model() {
        let model = Fixed::new(SUMMARY);
        let builder = VoiceBuilder::new(model.clone());
        for bad in [
            samples(2),
            samples(6),
            vec![sample("a"), sample("b"), "too short".to_owned()],
        ] {
            assert!(matches!(
                builder.summarise(&bad).await,
                Err(GhostwritinError::InvalidSamples(_))
            ));
        }
        let long = "word ".repeat(MAX_SAMPLE_WORDS + 1);
        assert!(
            builder
                .summarise(&[long, sample("b"), sample("c")])
                .await
                .is_err()
        );
        assert!(model.prompts.lock().unwrap().is_empty());
    }

    #[pollster::test]
    async fn a_summary_that_quotes_a_sample_is_refused() {
        let quoting = "Write like this: walked down to the harbour again and counted the boats.";
        let result = VoiceBuilder::new(Fixed::new(quoting))
            .summarise(&samples(3))
            .await;
        assert_eq!(result.unwrap_err(), GhostwritinError::ModelOutput);
    }

    #[pollster::test]
    async fn an_empty_or_oversized_summary_is_refused() {
        for summary in [String::new(), "x".repeat(2_001)] {
            let result = VoiceBuilder::new(Fixed::new(&summary))
                .summarise(&samples(4))
                .await;
            assert_eq!(result.unwrap_err(), GhostwritinError::ModelOutput);
        }
    }
}

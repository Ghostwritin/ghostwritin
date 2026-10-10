//! What can go wrong, with a stable code per case for metadata-only logs.

use crate::Lock;

/// Every failure a surface (API, CLI, MCP) can report.
///
/// `Display` never contains user text: no draft, no rewrite, no sample, no
/// provider message (a provider may quote the prompt back). Logs carry
/// [`GhostwritinError::code`] only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GhostwritinError {
    /// The request body was not the expected JSON. Carries no detail: a
    /// parser's message can quote the input.
    #[error("the request body must be JSON with text, voice and strength")]
    InvalidRequest,
    #[error("the text is empty")]
    EmptyText,
    #[error("the text has {words} words; the most one rewrite takes is {max}")]
    TooManyWords { words: usize, max: usize },
    #[error("my_voice needs a style summary; build one from 3 to 5 writing samples first")]
    VoiceUnavailable,
    #[error("a style summary must be 1 to 2000 characters (this one is {chars})")]
    InvalidStyleSummary { chars: usize },
    #[error("{0}")]
    InvalidSamples(&'static str),
    /// The rewrite changed or dropped locked facts, after a retry. The locks
    /// are the original's spans that did not survive (or numbers the model
    /// made up); they go back to the caller, never to a log.
    #[error("the rewrite changed {} locked fact(s) and was rejected", locks.len())]
    MeaningChanged { locks: Vec<Lock> },
    #[error("no language model is configured")]
    ModelNotConfigured,
    #[error("the language model is unavailable; try again shortly")]
    ModelUnavailable,
    #[error("the language model refused the request")]
    ModelRejected,
    #[error("the language model's answer did not have the expected shape")]
    ModelOutput,
    #[error("the word quota for this account is used up")]
    QuotaExceeded {
        /// Unix seconds when the account's period rolls over and the words
        /// come back.
        resets_at: u64,
        /// Seconds from now until `resets_at`: the `Retry-After` value.
        retry_after: u64,
    },
    #[error("a valid API key is required")]
    Unauthorized,
    #[error("billing is not configured")]
    BillingNotConfigured,
    #[error("billing is unavailable; try again shortly")]
    BillingUnavailable,
    #[error("storage failed")]
    Storage,
}

impl GhostwritinError {
    /// The stable slug for logs, problem types and exit messages.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid-request",
            Self::EmptyText => "empty-text",
            Self::TooManyWords { .. } => "too-many-words",
            Self::VoiceUnavailable => "voice-unavailable",
            Self::InvalidStyleSummary { .. } => "invalid-style-summary",
            Self::InvalidSamples(_) => "invalid-samples",
            Self::MeaningChanged { .. } => "meaning-changed",
            Self::ModelNotConfigured => "model-not-configured",
            Self::ModelUnavailable => "model-unavailable",
            Self::ModelRejected => "model-rejected",
            Self::ModelOutput => "model-output",
            Self::QuotaExceeded { .. } => "quota-exceeded",
            Self::Unauthorized => "unauthorized",
            Self::BillingNotConfigured => "billing-not-configured",
            Self::BillingUnavailable => "billing-unavailable",
            Self::Storage => "storage",
        }
    }

    /// The HTTP status a surface answers with.
    pub const fn status(&self) -> u16 {
        match self {
            Self::InvalidRequest
            | Self::EmptyText
            | Self::TooManyWords { .. }
            | Self::InvalidStyleSummary { .. }
            | Self::InvalidSamples(_) => 400,
            Self::Unauthorized => 401,
            Self::VoiceUnavailable => 403,
            Self::MeaningChanged { .. } => 422,
            Self::QuotaExceeded { .. } => 429,
            Self::ModelRejected | Self::ModelOutput => 502,
            Self::ModelNotConfigured
            | Self::ModelUnavailable
            | Self::BillingNotConfigured
            | Self::BillingUnavailable
            | Self::Storage => 503,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LockKind;

    #[test]
    fn display_never_names_a_lock() {
        let error = GhostwritinError::MeaningChanged {
            locks: vec![Lock {
                kind: LockKind::Name,
                text: "Dana Okafor".to_owned(),
            }],
        };
        let shown = error.to_string();
        assert!(!shown.contains("Okafor"), "{shown}");
        assert_eq!(error.code(), "meaning-changed");
        assert_eq!(error.status(), 422);
    }
}

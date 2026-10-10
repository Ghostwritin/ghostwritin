//! Ghostwritin's domain, with no I/O: the voices and strengths, the rewrite
//! request and its limits, the response (rewrite, scores, diff, locks), the
//! errors, the **open-core ports** ([`ports`]) through which the hosted
//! service plugs in what is not in this repository, and the issued API keys
//! ([`accounts`]) that authenticate against one of those ports.
//!
//! # Data policy, as types
//!
//! Drafts and rewrites are never persisted or logged. Two things here make
//! that hard to get wrong:
//!
//! - [`GhostwritinError`]'s `Display` never contains user text, only counts
//!   and kinds, and [`GhostwritinError::code`] is the stable slug that logs
//!   carry. A [`MeaningChanged`](GhostwritinError::MeaningChanged) error
//!   holds the locks (they go back to the caller, who wrote them), but
//!   printing the error prints how many, not which.
//! - [`RewriteRequest`]'s `Debug` prints the word count, the voice and the
//!   strength, never the text, so a stray `{:?}` in a log line is safe.
//!
//! The ports that hold data ([`ports::VoiceStore`]) take a
//! [`StyleSummary`], never the samples it was built from.

#![forbid(unsafe_code)]

pub mod accounts;
mod error;
pub mod ports;
mod types;

pub use error::GhostwritinError;
pub use types::{
    AccountId, DiffOp, DiffSegment, Lock, LockKind, MAX_SAMPLE_WORDS, MAX_SAMPLES,
    MAX_STYLE_SUMMARY_CHARS, MAX_WORDS, MIN_SAMPLE_WORDS, MIN_SAMPLES, RewriteRequest,
    RewriteResponse, Score, Strength, StyleSummary, Voice, Watermark, diff, locks_of, word_count,
};

//! The open-core seams.
//!
//! Everything a surface needs beyond the engine and a `TextModel` comes in
//! through one of these traits. This repository (MIT) ships an open
//! implementation of each: a no-op where the real thing is a hosted feature,
//! a basic one where a self-hosted install can use it as it is. The hosted
//! service supplies its own implementations from a separate private crate,
//! composed in its own Worker, without forking anything here.
//!
//! | Port | Open implementation here | Hosted implementation (private) |
//! |---|---|---|
//! | [`HumanScore`] | [`NoHumanScore`]: no score, `null` in the API | a detector ensemble |
//! | [`WatermarkDetector`] | [`NoWatermarkDetector`]: finds nothing | detection for major model families |
//! | [`Quota`] | [`Unlimited`], [`DailyWordLimit`] (in memory) | plans and word quotas, billed through Polar |
//! | [`VoiceStore`] | [`NoVoiceStore`], [`InMemoryVoiceStore`] | style summaries per account, encrypted at rest |
//! | [`ApiKeys`] | [`NoApiKeys`], [`StaticApiKeys`] (hashed keys from config) | accounts, passkeys and API keys |
//! | [`RequestLogger`] | a `tracing` line (API crate), a console line (Worker) | the hosted log pipeline and usage metering |
//!
//! None of these ever receives text to keep: [`HumanScore`] and
//! [`WatermarkDetector`] see the text to judge it and must not store it,
//! and [`VoiceStore`] holds a [`StyleSummary`], never samples.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{AccountId, GhostwritinError, Score, Strength, StyleSummary, Voice, Watermark};

/// How likely a detector is to read a text as model-written.
///
/// An implementation sees the text and must not keep it: it stores and
/// logs nothing. Its [`Score`] may carry [`crate::Flag`]s — byte offsets
/// into that text, never the words themselves.
#[async_trait]
pub trait HumanScore: Send + Sync {
    /// The score, or `None` when there is no detector (or it failed: a
    /// missing score must never fail a rewrite).
    async fn score(&self, text: &str) -> Option<Score>;
}

/// **Not a detector.** The open build's [`HumanScore`]: always `None`, so
/// the API answers `"score_before": null`. Real scoring is a hosted
/// feature; nothing in this repository estimates it, and nothing here
/// pretends to.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHumanScore;

#[async_trait]
impl HumanScore for NoHumanScore {
    async fn score(&self, _text: &str) -> Option<Score> {
        None
    }
}

/// Finds the watermarks a model left in a text.
///
/// An implementation sees the text and must not keep it.
#[async_trait]
pub trait WatermarkDetector: Send + Sync {
    async fn detect(&self, text: &str) -> Vec<Watermark>;
}

/// The open build's [`WatermarkDetector`]: finds nothing. Watermark
/// detection is a hosted feature.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWatermarkDetector;

#[async_trait]
impl WatermarkDetector for NoWatermarkDetector {
    async fn detect(&self, _text: &str) -> Vec<Watermark> {
        Vec::new()
    }
}

/// Word quotas: reserve the words a rewrite will use before the model is
/// called.
#[async_trait]
pub trait Quota: Send + Sync {
    /// # Errors
    ///
    /// [`GhostwritinError::QuotaExceeded`] when `words` more would go over
    /// the account's allowance; [`GhostwritinError::Storage`] when the
    /// count cannot be read.
    async fn reserve(&self, account: &AccountId, words: usize) -> Result<(), GhostwritinError>;
}

/// No quota: every reservation succeeds. The CLI's and the MCP server's,
/// where the user pays their own model provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unlimited;

#[async_trait]
impl Quota for Unlimited {
    async fn reserve(&self, _account: &AccountId, _words: usize) -> Result<(), GhostwritinError> {
        Ok(())
    }
}

/// A fixed number of words per account per day, counted in memory.
///
/// Basic on purpose: the count lives in this process, so it resets on a
/// restart and is per isolate on Workers. Good enough for one self-hosted
/// server; the hosted service keeps its counts in a database. The day is
/// whatever `today` returns (days since an epoch), so tests and runtimes
/// without a system clock can supply it.
pub struct DailyWordLimit {
    limit: usize,
    today: Arc<dyn Fn() -> u64 + Send + Sync>,
    used: Mutex<HashMap<AccountId, (u64, usize)>>,
}

impl DailyWordLimit {
    pub fn new(limit: usize, today: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            limit,
            today: Arc::new(today),
            used: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Quota for DailyWordLimit {
    async fn reserve(&self, account: &AccountId, words: usize) -> Result<(), GhostwritinError> {
        let today = (self.today)();
        let mut used = self.used.lock().map_err(|_| GhostwritinError::Storage)?;
        let entry = used.entry(account.clone()).or_insert((today, 0));
        if entry.0 != today {
            *entry = (today, 0);
        }
        if entry.1.saturating_add(words) > self.limit {
            return Err(GhostwritinError::QuotaExceeded);
        }
        entry.1 += words;
        Ok(())
    }
}

/// Where My voice's style summaries live. Holds the summary only; the
/// samples it was built from are never passed here.
#[async_trait]
pub trait VoiceStore: Send + Sync {
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when the store cannot be read.
    async fn get(&self, account: &AccountId) -> Result<Option<StyleSummary>, GhostwritinError>;
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when it cannot be written, and
    /// [`GhostwritinError::VoiceUnavailable`] where My voice is not offered.
    async fn put(&self, account: &AccountId, summary: StyleSummary)
    -> Result<(), GhostwritinError>;
    /// Deletes the account's summary; deleting none is not an error.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when it cannot be written.
    async fn delete(&self, account: &AccountId) -> Result<(), GhostwritinError>;
}

/// No My voice: nothing is stored and nothing is found. The open Worker's
/// default; the hosted service offers My voice.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoVoiceStore;

#[async_trait]
impl VoiceStore for NoVoiceStore {
    async fn get(&self, _account: &AccountId) -> Result<Option<StyleSummary>, GhostwritinError> {
        Ok(None)
    }
    async fn put(
        &self,
        _account: &AccountId,
        _summary: StyleSummary,
    ) -> Result<(), GhostwritinError> {
        Err(GhostwritinError::VoiceUnavailable)
    }
    async fn delete(&self, _account: &AccountId) -> Result<(), GhostwritinError> {
        Ok(())
    }
}

/// Style summaries in memory, for one process (the CLI, the MCP server,
/// tests). Gone when the process ends.
#[derive(Default)]
pub struct InMemoryVoiceStore {
    summaries: Mutex<HashMap<AccountId, StyleSummary>>,
}

#[async_trait]
impl VoiceStore for InMemoryVoiceStore {
    async fn get(&self, account: &AccountId) -> Result<Option<StyleSummary>, GhostwritinError> {
        let summaries = self
            .summaries
            .lock()
            .map_err(|_| GhostwritinError::Storage)?;
        Ok(summaries.get(account).cloned())
    }
    async fn put(
        &self,
        account: &AccountId,
        summary: StyleSummary,
    ) -> Result<(), GhostwritinError> {
        let mut summaries = self
            .summaries
            .lock()
            .map_err(|_| GhostwritinError::Storage)?;
        summaries.insert(account.clone(), summary);
        Ok(())
    }
    async fn delete(&self, account: &AccountId) -> Result<(), GhostwritinError> {
        let mut summaries = self
            .summaries
            .lock()
            .map_err(|_| GhostwritinError::Storage)?;
        summaries.remove(account);
        Ok(())
    }
}

/// The one log record per rewrite request: metadata only. There is no
/// field that could hold text, so a logger cannot log any (data policy);
/// the time is the log record's own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestLog {
    pub account: Option<AccountId>,
    pub words: usize,
    pub voice: Option<Voice>,
    pub strength: Option<Strength>,
    pub status: u16,
    /// `ok`, or the error's [`GhostwritinError::code`].
    pub outcome: &'static str,
}

/// Where [`RequestLog`]s go.
pub trait RequestLogger: Send + Sync {
    fn log(&self, entry: &RequestLog);
}

/// Logs nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRequestLogger;

impl RequestLogger for NoRequestLogger {
    fn log(&self, _entry: &RequestLog) {}
}

/// Turns an API key into an account.
#[async_trait]
pub trait ApiKeys: Send + Sync {
    /// The account `key` belongs to, or `None` for an unknown key.
    async fn authenticate(&self, key: &str) -> Option<AccountId>;
}

/// No keys: every request is refused. The safe default for a deployment
/// that has not configured any.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoApiKeys;

#[async_trait]
impl ApiKeys for NoApiKeys {
    async fn authenticate(&self, _key: &str) -> Option<AccountId> {
        None
    }
}

/// A fixed list of keys for a self-hosted install, stored as SHA-256
/// digests so the configuration never holds a usable key.
///
/// Configuration format (`GHOSTWRITIN_API_KEYS`): comma-separated
/// `account=sha256hex` pairs, e.g. `me=9f86d0…`. Make a digest with
/// `printf %s "$KEY" | shasum -a 256`. A malformed pair is skipped.
#[derive(Debug, Clone, Default)]
pub struct StaticApiKeys {
    keys: Vec<(AccountId, [u8; 32])>,
}

impl StaticApiKeys {
    /// Parses the `account=sha256hex,…` format.
    pub fn parse(config: &str) -> Self {
        let keys = config
            .split(',')
            .filter_map(|pair| {
                let (account, hex) = pair.trim().split_once('=')?;
                let digest = decode_hex32(hex.trim())?;
                let account = account.trim();
                (!account.is_empty()).then(|| (AccountId(account.to_owned()), digest))
            })
            .collect();
        Self { keys }
    }

    /// How many keys parsed.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no key parsed.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

#[async_trait]
impl ApiKeys for StaticApiKeys {
    async fn authenticate(&self, key: &str) -> Option<AccountId> {
        let digest: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        // Compare against every key, in constant time each, so timing says
        // nothing about which (or whether one) matched.
        let mut found = None;
        for (account, expected) in &self.keys {
            if bool::from(digest.ct_eq(expected)) {
                found = Some(account.clone());
            }
        }
        found
    }
}

fn decode_hex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str) -> AccountId {
        AccountId(name.to_owned())
    }

    #[pollster::test]
    async fn the_open_score_is_no_score() {
        assert_eq!(NoHumanScore.score("anything").await, None);
        assert!(NoWatermarkDetector.detect("anything").await.is_empty());
    }

    #[pollster::test]
    async fn a_daily_limit_resets_each_day() {
        let day = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let clock = day.clone();
        let quota =
            DailyWordLimit::new(100, move || clock.load(std::sync::atomic::Ordering::SeqCst));
        let me = account("me");
        assert_eq!(quota.reserve(&me, 60).await, Ok(()));
        assert_eq!(
            quota.reserve(&me, 41).await,
            Err(GhostwritinError::QuotaExceeded)
        );
        assert_eq!(quota.reserve(&account("other"), 100).await, Ok(()));
        assert_eq!(quota.reserve(&me, 40).await, Ok(()));
        day.store(2, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(quota.reserve(&me, 100).await, Ok(()));
    }

    #[pollster::test]
    async fn voice_stores() {
        let me = account("me");
        let summary = StyleSummary::new("Short sentences.").unwrap();
        assert_eq!(
            NoVoiceStore.put(&me, summary.clone()).await,
            Err(GhostwritinError::VoiceUnavailable)
        );
        let store = InMemoryVoiceStore::default();
        store.put(&me, summary.clone()).await.unwrap();
        assert_eq!(store.get(&me).await.unwrap(), Some(summary));
        store.delete(&me).await.unwrap();
        assert_eq!(store.get(&me).await.unwrap(), None);
    }

    #[pollster::test]
    async fn static_keys_match_by_digest() {
        // sha256("test-key")
        let digest = "62af8704764faf8ea82fc61ce9c4c3908b6cb97d463a634e9e587d7c885db0ef";
        let keys = StaticApiKeys::parse(&format!(" me = {digest} , broken=xyz, =abc"));
        assert_eq!(keys.len(), 1);
        assert_eq!(keys.authenticate("test-key").await, Some(account("me")));
        assert_eq!(keys.authenticate("test-kez").await, None);
        assert_eq!(keys.authenticate(digest).await, None);
        assert_eq!(NoApiKeys.authenticate("test-key").await, None);
    }
}

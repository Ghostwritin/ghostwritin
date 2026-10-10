//! Issued API keys: the hosted service's [`ApiKeys`].
//!
//! A live key is [`LIVE_KEY_PREFIX`] plus 48 lowercase hex characters, 24
//! bytes of entropy the caller draws — core has no randomness. The
//! plaintext is shown once, at issue time, and never stored: the
//! [`AccountStore`] keeps the SHA-256 digest of the whole key, a [`KeyId`]
//! derived from that digest, the key's last four characters as a display
//! hint, and the metadata the dashboard shows. A plaintext that is lost is
//! revoked and replaced, never recovered.
//!
//! This module is the key format, the store port and an in-memory
//! reference store. The hosted service implements [`AccountStore`] over
//! its own database in its private crate (#10), passes an
//! [`IssuedApiKeys`] as its `ApiKeys` to `Services::open`, and keeps
//! sign-in (passkeys, email link) and the dashboard to itself. Nothing
//! here reads a clock: `issue` takes the time from its caller.

use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::ports::{ApiKeys, VoiceStore};
use crate::{AccountId, GhostwritinError};

/// Every live key starts with this.
pub const LIVE_KEY_PREFIX: &str = "gw_live_";

/// The entropy after the prefix, as lowercase hex: 24 bytes, 48 characters.
const KEY_HEX_CHARS: usize = 48;

/// The longest label an issued key carries, in characters.
pub const MAX_KEY_LABEL_CHARS: usize = 64;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn hex_encode(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        hex.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    hex
}

/// A key's public identifier: the first 12 hex characters of its SHA-256
/// digest. Deterministic, so a key lists and revokes under the same id for
/// its whole life, and useless for guessing the key — 48 bits of a digest
/// of 24 random bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyId(String);

impl KeyId {
    fn of(digest: &[u8; 32]) -> Self {
        Self(hex_encode(&digest[..6]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stored key: everything about it except the key itself. The digest is
/// of the whole presented key, prefix included; `hint` is the plaintext's
/// last four characters, so a dashboard can show `…9f2c` the way a card
/// shows its last digits.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    /// The public identifier, safe to show, sort by and log.
    pub id: KeyId,
    pub account: AccountId,
    /// The SHA-256 digest of the whole key. Not printable: see `Debug`.
    pub digest: [u8; 32],
    /// What the key is for, as the account owner named it.
    pub label: String,
    /// The plaintext's last four characters, for display only.
    pub hint: String,
    /// When the key was issued, in seconds since the Unix epoch.
    pub created_at: u64,
}

/// A record prints its shape, never its digest — the data policy of this
/// crate, applied to a secret in waiting.
impl std::fmt::Debug for ApiKeyRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyRecord")
            .field("id", &self.id)
            .field("account", &self.account)
            .field("digest", &"[sha-256; not printed]")
            .field("label", &self.label)
            .field("hint", &self.hint)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// What [`IssuedApiKeys::issue`] returns: the plaintext, shown once, and
/// the record that was stored.
#[derive(Clone)]
pub struct IssuedKey {
    /// The key: [`LIVE_KEY_PREFIX`] plus 48 hex characters. Shown once;
    /// never stored, never logged, and [`Debug`] prints nothing of it.
    pub secret: String,
    /// The stored record (digest, id, hint — never the secret).
    pub record: ApiKeyRecord,
}

/// The secret is the one thing this type exists to hand over, so `Debug`
/// prints the record only.
impl std::fmt::Debug for IssuedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedKey")
            .field("record", &self.record)
            .finish_non_exhaustive()
    }
}

/// Where a deployment keeps its issued keys. The hosted service implements
/// this over its own database in its private crate (#10);
/// [`InMemoryAccountStore`] is the open reference.
#[async_trait]
pub trait AccountStore: Send + Sync {
    /// Files a newly issued key.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when it cannot be written.
    async fn insert_key(&self, record: ApiKeyRecord) -> Result<(), GhostwritinError>;

    /// Looks a key up by the digest of the presented plaintext.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when the store cannot be read.
    async fn key_by_digest(
        &self,
        digest: &[u8; 32],
    ) -> Result<Option<ApiKeyRecord>, GhostwritinError>;

    /// The account's keys, in any order; [`IssuedApiKeys::list`] sorts.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when the store cannot be read.
    async fn keys(&self, account: &AccountId) -> Result<Vec<ApiKeyRecord>, GhostwritinError>;

    /// Revokes one of `account`'s keys: `Ok(true)` when a key of that
    /// account was removed, `Ok(false)` when there was no such key.
    /// Another account's key is never touched.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when it cannot be written.
    async fn revoke_key(&self, account: &AccountId, id: &KeyId) -> Result<bool, GhostwritinError>;

    /// Removes the account and all its keys; deleting an account that is
    /// not there is not an error.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when it cannot be written.
    async fn delete_account(&self, account: &AccountId) -> Result<(), GhostwritinError>;
}

/// Issues, lists and revokes live keys, and authenticates API requests
/// with them. Behind an [`AccountStore`]; passed wherever the deployment's
/// [`ApiKeys`] is expected (`Services::open` in the
/// API crate).
pub struct IssuedApiKeys {
    store: Arc<dyn AccountStore>,
}

impl IssuedApiKeys {
    /// Serves keys from `store`.
    pub fn new(store: Arc<dyn AccountStore>) -> Self {
        Self { store }
    }

    /// Issues a new key to `account`, from 24 bytes of entropy the caller
    /// draws from its runtime's secure randomness. The plaintext is in the
    /// answer and nowhere else.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::InvalidSamples`] — the crate's one validation
    /// error that carries a message — when `label` is empty or longer than
    /// [`MAX_KEY_LABEL_CHARS`], and [`GhostwritinError::Storage`] when the
    /// key cannot be filed.
    pub async fn issue(
        &self,
        account: AccountId,
        label: &str,
        entropy: [u8; 24],
        created_at: u64,
    ) -> Result<IssuedKey, GhostwritinError> {
        let label = label.trim();
        let chars = label.chars().count();
        if chars == 0 || chars > MAX_KEY_LABEL_CHARS {
            return Err(GhostwritinError::InvalidSamples(
                "an API key label must be 1 to 64 characters",
            ));
        }
        let secret = format!("{LIVE_KEY_PREFIX}{}", hex_encode(&entropy));
        let digest: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        let record = ApiKeyRecord {
            id: KeyId::of(&digest),
            account,
            digest,
            label: label.to_owned(),
            hint: secret[secret.len() - 4..].to_owned(),
            created_at,
        };
        self.store.insert_key(record.clone()).await?;
        Ok(IssuedKey { secret, record })
    }

    /// The account's keys, oldest first, ties broken by id.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when the store cannot be read.
    pub async fn list(&self, account: &AccountId) -> Result<Vec<ApiKeyRecord>, GhostwritinError> {
        let mut keys = self.store.keys(account).await?;
        keys.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(keys)
    }

    /// Revokes one of the account's keys: `Ok(true)` when it was removed.
    ///
    /// # Errors
    ///
    /// [`GhostwritinError::Storage`] when the store cannot be written.
    pub async fn revoke(&self, account: &AccountId, id: &KeyId) -> Result<bool, GhostwritinError> {
        self.store.revoke_key(account, id).await
    }
}

#[async_trait]
impl ApiKeys for IssuedApiKeys {
    /// The account the key belongs to, or `None` for a malformed or
    /// unknown key — and for a store outage: an unreachable store reads as
    /// unauthorized, never as a fallback that lets anyone through.
    async fn authenticate(&self, key: &str) -> Option<AccountId> {
        let hex = key.strip_prefix(LIVE_KEY_PREFIX)?;
        if hex.len() != KEY_HEX_CHARS || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let digest: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        // Defence in depth: the lookup is already by digest, and the match
        // is still made in constant time, so nothing about the timing says
        // which bytes differed.
        let record = self.store.key_by_digest(&digest).await.ok()??;
        bool::from(digest.ct_eq(&record.digest)).then_some(record.account)
    }
}

/// Issued keys in memory, for tests and local use. Gone when the process
/// ends.
#[derive(Default)]
pub struct InMemoryAccountStore {
    keys: Mutex<Vec<ApiKeyRecord>>,
}

impl InMemoryAccountStore {
    fn lock(&self) -> Result<MutexGuard<'_, Vec<ApiKeyRecord>>, GhostwritinError> {
        self.keys.lock().map_err(|_| GhostwritinError::Storage)
    }
}

#[async_trait]
impl AccountStore for InMemoryAccountStore {
    async fn insert_key(&self, record: ApiKeyRecord) -> Result<(), GhostwritinError> {
        self.lock()?.push(record);
        Ok(())
    }

    async fn key_by_digest(
        &self,
        digest: &[u8; 32],
    ) -> Result<Option<ApiKeyRecord>, GhostwritinError> {
        let keys = self.lock()?;
        Ok(keys.iter().find(|key| key.digest == *digest).cloned())
    }

    async fn keys(&self, account: &AccountId) -> Result<Vec<ApiKeyRecord>, GhostwritinError> {
        let keys = self.lock()?;
        Ok(keys
            .iter()
            .filter(|key| key.account == *account)
            .cloned()
            .collect())
    }

    async fn revoke_key(&self, account: &AccountId, id: &KeyId) -> Result<bool, GhostwritinError> {
        let mut keys = self.lock()?;
        let before = keys.len();
        keys.retain(|key| key.account != *account || key.id != *id);
        Ok(keys.len() != before)
    }

    async fn delete_account(&self, account: &AccountId) -> Result<(), GhostwritinError> {
        let mut keys = self.lock()?;
        keys.retain(|key| key.account != *account);
        Ok(())
    }
}

/// Deletes an account everywhere this repository reaches: the My voice
/// style summary, then the account and its keys. Idempotent — a run that
/// half-failed is simply run again, and every step treats "already gone"
/// as done ([`NoVoiceStore`](crate::ports::NoVoiceStore), which holds
/// nothing, deletes with `Ok(())`).
///
/// # Errors
///
/// [`GhostwritinError::Storage`] when either store cannot be written; the
/// summary goes first, so a second run picks up wherever the first
/// stopped.
pub async fn delete_account(
    accounts: &dyn AccountStore,
    voices: &dyn VoiceStore,
    account: &AccountId,
) -> Result<(), GhostwritinError> {
    voices.delete(account).await?;
    accounts.delete_account(account).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StyleSummary;
    use crate::ports::{InMemoryVoiceStore, NoVoiceStore};

    fn store() -> IssuedApiKeys {
        IssuedApiKeys::new(Arc::new(InMemoryAccountStore::default()))
    }

    fn entropy(byte: u8) -> [u8; 24] {
        [byte; 24]
    }

    fn account(name: &str) -> AccountId {
        AccountId(name.to_owned())
    }

    #[pollster::test]
    async fn an_issued_key_authenticates_and_hides_itself() {
        let keys = store();
        let me = account("me");
        let issued = keys
            .issue(me.clone(), "CI", entropy(1), 1_700_000_000)
            .await
            .unwrap();
        assert!(issued.secret.starts_with(LIVE_KEY_PREFIX));
        assert_eq!(issued.secret.len(), LIVE_KEY_PREFIX.len() + KEY_HEX_CHARS);
        assert_eq!(
            issued.record.hint,
            &issued.secret[issued.secret.len() - 4..]
        );
        // The same entropy makes the same id: ids come from the digest.
        let twin = keys.issue(me.clone(), "twin", entropy(1), 2).await.unwrap();
        assert_eq!(issued.record.id, twin.record.id);
        assert_eq!(keys.authenticate(&issued.secret).await, Some(me.clone()));

        let debugged = format!("{issued:?}");
        assert!(!debugged.contains(&issued.secret), "{debugged}");
        let stored = keys.list(&me).await.unwrap();
        assert_eq!(stored.len(), 2);
        let debugged = format!("{:?}", stored[0]);
        assert!(!debugged.contains(&issued.secret), "{debugged}");
    }

    #[pollster::test]
    async fn malformed_and_wrong_keys_are_refused() {
        let keys = store();
        let issued = keys
            .issue(account("me"), "CI", entropy(1), 1)
            .await
            .unwrap();
        let body = issued.secret.strip_prefix(LIVE_KEY_PREFIX).unwrap();
        // Right body, wrong prefix.
        assert_eq!(keys.authenticate(&format!("gw_test_{body}")).await, None);
        // Right prefix, too short.
        assert_eq!(
            keys.authenticate(&format!("{LIVE_KEY_PREFIX}{}", &body[..47]))
                .await,
            None
        );
        // Right shape, one character not hex.
        assert_eq!(
            keys.authenticate(&format!("{LIVE_KEY_PREFIX}z{}", &body[1..]))
                .await,
            None
        );
        // Truncated, uppercase prefix, empty, and a well-formed key that
        // was never issued.
        assert_eq!(keys.authenticate(&issued.secret[..55]).await, None);
        assert_eq!(keys.authenticate(&issued.secret.to_uppercase()).await, None);
        assert_eq!(keys.authenticate("").await, None);
        let forged = format!("{LIVE_KEY_PREFIX}{}", hex_encode(&entropy(2)));
        assert_eq!(keys.authenticate(&forged).await, None);
    }

    #[pollster::test]
    async fn listing_and_revocation_are_per_account() {
        let keys = store();
        let me = account("me");
        let other = account("other");
        let first = keys.issue(me.clone(), "one", entropy(1), 1).await.unwrap();
        let second = keys.issue(me.clone(), "two", entropy(2), 1).await.unwrap();
        let older = keys
            .issue(me.clone(), "older", entropy(3), 0)
            .await
            .unwrap();
        let theirs = keys
            .issue(other.clone(), "theirs", entropy(4), 9)
            .await
            .unwrap();

        let listed = keys.list(&me).await.unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].id, older.record.id);
        assert!(listed[1].id < listed[2].id, "ties break by id");
        assert_eq!(keys.list(&other).await.unwrap()[0].id, theirs.record.id);

        // Another account's id is not mine to revoke, and it still works.
        assert_eq!(keys.revoke(&me, &theirs.record.id).await, Ok(false));
        assert_eq!(keys.authenticate(&theirs.secret).await, Some(other.clone()));

        assert_eq!(keys.revoke(&me, &second.record.id).await, Ok(true));
        assert_eq!(keys.authenticate(&second.secret).await, None);
        assert_eq!(keys.authenticate(&first.secret).await, Some(me.clone()));
        assert_eq!(keys.list(&me).await.unwrap().len(), 2);
    }

    #[pollster::test]
    async fn labels_are_trimmed_and_bounded() {
        let keys = store();
        let me = account("me");
        assert!(keys.issue(me.clone(), "   ", entropy(1), 1).await.is_err());
        assert!(
            keys.issue(
                me.clone(),
                &"x".repeat(MAX_KEY_LABEL_CHARS + 1),
                entropy(2),
                1
            )
            .await
            .is_err()
        );
        let issued = keys
            .issue(me.clone(), "  smoke test  ", entropy(3), 1)
            .await
            .unwrap();
        assert_eq!(issued.record.label, "smoke test");
    }

    #[pollster::test]
    async fn delete_account_clears_keys_and_voice_and_repeats_safely() {
        let accounts = Arc::new(InMemoryAccountStore::default());
        let keys = IssuedApiKeys::new(accounts.clone());
        let voices = InMemoryVoiceStore::default();
        let me = account("me");
        let issued = keys.issue(me.clone(), "cli", entropy(1), 1).await.unwrap();
        let summary = StyleSummary::new("Short sentences.").unwrap();
        voices.put(&me, summary).await.unwrap();

        delete_account(accounts.as_ref(), &voices, &me)
            .await
            .unwrap();
        assert_eq!(keys.authenticate(&issued.secret).await, None);
        assert_eq!(keys.list(&me).await.unwrap(), Vec::new());
        assert_eq!(voices.get(&me).await.unwrap(), None);

        // Already gone is done: again fails nowhere, even against a voice
        // store that never held anything for the account.
        delete_account(accounts.as_ref(), &voices, &me)
            .await
            .unwrap();
        delete_account(accounts.as_ref(), &NoVoiceStore, &me)
            .await
            .unwrap();
    }

    #[pollster::test]
    async fn issued_keys_fill_the_api_keys_port() {
        let keys = Arc::new(store());
        let issued = keys
            .issue(account("me"), "cli", entropy(1), 1)
            .await
            .unwrap();
        let keys: Arc<dyn ApiKeys> = keys;
        assert_eq!(keys.authenticate(&issued.secret).await, Some(account("me")));
    }
}

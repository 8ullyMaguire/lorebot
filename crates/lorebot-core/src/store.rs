//! The encrypted token store.
//!
//! Holds one bearer token per (platform, platform-user) pair, encrypted at rest
//! with AES-256-GCM. **It is a JSON file, not a database** — §23.2's "the bot
//! never touches the Lorehaven database" is about Lorehaven's data, but a bot
//! that reached for a database to hold a dozen tokens would need one
//! configured, operated and backed up, and would then have a second thing to
//! secure. A file holding only ciphertext, on the same host, is one fewer
//! system to get wrong.
//!
//! ## The threat model, stated so it can be argued with
//!
//! An attacker who can read the file gets ciphertext and nothing else: the key
//! lives in the environment, in the process's memory, or in a file the operator
//! chose. An attacker who can read the file *and* the environment gets
//! everything, and no amount of encryption on disk helps — which is why
//! [`TokenStore::ephemeral`] exists, and why the key is never written beside
//! the data.
//!
//! What this does **not** defend against, and should not be claimed to: an
//! attacker with a memory dump of the running process, a root-level compromise
//! of the host, or a key in an environment readable by other processes under
//! the same uid. A companion bot holding a user's bearer token is a
//! bearer-token holder by definition; the file encryption keeps the token out
//! of backups, out of a `git add .`, and out of a support log.
//!
//! ## Atomicity
//!
//! Writes go to a temporary file in the same directory and are renamed over the
//! target. `rename(2)` within a filesystem is atomic, so a crash mid-write
//! leaves the previous file intact rather than a truncated one. That matters
//! more than usual here: a half-written store parses as "no tokens", and every
//! user silently becomes unlinked.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use zeroize::Zeroizing;

use crate::error::{BotError, Result};

/// The key length, in bytes. AES-256.
pub const KEY_LEN: usize = 32;

/// The nonce length, in bytes. GCM's standard 96-bit nonce.
const NONCE_LEN: usize = 12;

/// A stored link.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredLink {
    /// The bearer token.
    ///
    /// `Zeroizing` so a heap copy of the secret is zeroed when the last
    /// reference drops. It is not a guarantee the secret never sits in memory —
    /// nothing can be — but it does mean a freed buffer does not keep a live
    /// credential in it until the next allocation.
    #[serde(rename = "token")]
    token: Zeroizing<String>,
    /// The scopes Lorehaven granted, space-separated.
    #[serde(default)]
    pub scopes: String,
    /// The pseud the token acts as, if any.
    #[serde(default, rename = "acting_pseud_id")]
    pub acting_pseud_id: Option<String>,
    /// Lorehaven's id for the token, for revocation from the bot side.
    #[serde(default)]
    pub token_id: Option<String>,
    /// When the link was made, as a unix timestamp.
    #[serde(default)]
    pub linked_at: i64,
}

impl StoredLink {
    /// A link from a token the exchange just returned.
    pub fn new(
        token: impl Into<String>,
        scopes: impl Into<String>,
        acting_pseud_id: Option<String>,
        token_id: Option<String>,
        now: i64,
    ) -> Self {
        Self {
            token: Zeroizing::new(token.into()),
            scopes: scopes.into(),
            acting_pseud_id,
            token_id,
            linked_at: now,
        }
    }

    /// The token, for building a credential.
    ///
    /// Named `expose` because it is the one way out, and because a `pub token`
    /// field plus a `Debug` derive would print the secret into any log that
    /// formats the struct.
    pub fn expose(&self) -> &str {
        &self.token
    }

    /// The scopes as a set.
    pub fn scope_set(&self) -> lorebot_api::ScopeSet {
        lorebot_api::ScopeSet::parse(&self.scopes)
    }
}

/// Redacted so a stray `{:?}` cannot leak a token.
impl std::fmt::Debug for StoredLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredLink")
            .field("token", &"[REDACTED]")
            .field("scopes", &self.scopes)
            .field("acting_pseud_id", &self.acting_pseud_id)
            .field("token_id", &self.token_id)
            .field("linked_at", &self.linked_at)
            .finish()
    }
}

/// One account's links, keyed by platform.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Account {
    /// The links.
    #[serde(default)]
    pub links: BTreeMap<String, StoredLink>,
}

/// The on-disk shape.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    /// Format version, so a future change can migrate rather than guess.
    #[serde(default)]
    version: u32,
    /// The accounts.
    #[serde(default)]
    accounts: BTreeMap<String, Account>,
}

/// The current format version.
const FORMAT_VERSION: u32 = 1;

/// Where links are kept.
#[derive(Clone)]
pub enum TokenStore {
    /// An encrypted file.
    File(Arc<FileStore>),
    /// Memory only. Nothing is written, so nothing to leak from disk and
    /// nothing to clean up — and every user is unlinked on restart.
    Ephemeral(Arc<EphemeralStore>),
}

impl std::fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written rather than derived: `File` holds the key, and a derived
        // `Debug` would print the inner store.
        match self {
            Self::File(store) => f
                .debug_struct("TokenStore::File")
                .field("path", &store.path)
                .finish_non_exhaustive(),
            Self::Ephemeral(_) => f.write_str("TokenStore::Ephemeral"),
        }
    }
}

/// An encrypted file store.
pub struct FileStore {
    path: PathBuf,
    /// `Arc`-shared behind the enum, so there is one key rather than one per
    /// call site.
    cipher: Aes256Gcm,
    /// Serialises read-modify-write, so two concurrent `/link` calls cannot
    /// each read the file, each add a user, and each write — losing one.
    lock: tokio::sync::Mutex<()>,
    cached: tokio::sync::RwLock<BTreeMap<String, Account>>,
}

impl std::fmt::Debug for FileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl FileStore {
    /// Open or create a store.
    ///
    /// # Errors
    ///
    /// Returns [`BotError::Store`] if the file exists but cannot be decrypted,
    /// which almost always means the key is wrong. That is reported rather than
    /// treated as "empty", because starting empty would look like every user had
    /// been silently unlinked.
    pub async fn open(path: impl Into<PathBuf>, key: &[u8; KEY_LEN]) -> Result<Self> {
        let path = path.into();
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
        let store = Self {
            path,
            cipher,
            lock: tokio::sync::Mutex::new(()),
            cached: tokio::sync::RwLock::new(BTreeMap::new()),
        };
        let accounts = store.read_file()?;
        *store.cached.write().await = accounts;
        Ok(store)
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read and decrypt, or an empty map if there is no file.
    fn read_file(&self) -> Result<BTreeMap<String, Account>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => {
                return Err(BotError::Store(format!(
                    "could not read the token store at {}: {e}",
                    self.path.display()
                )));
            }
        };
        if bytes.is_empty() {
            // A zero-length file is what a crash between create and write
            // leaves behind, and what `touch` leaves. Treating it as empty is
            // right; treating it as corruption would make a recoverable state
            // fatal.
            return Ok(BTreeMap::new());
        }

        let plaintext = self.decrypt(&bytes).map_err(|e| {
            BotError::Store(format!(
                "could not decrypt the token store at {}. If LOREBOT_STORE_KEY changed, the \
                 stored links cannot be read and the users must re-link. Original error: {e}",
                self.path.display()
            ))
        })?;
        let file: StoreFile = serde_json::from_slice(&plaintext).map_err(|e| {
            BotError::Store(format!(
                "the token store at {} is not valid JSON: {e}",
                self.path.display()
            ))
        })?;
        if file.version > FORMAT_VERSION {
            return Err(BotError::Store(format!(
                "the token store at {} is version {}, but this build only understands up to \
                 {FORMAT_VERSION}. Upgrade lorebot rather than running it against a newer store.",
                self.path.display(),
                file.version
            )));
        }
        Ok(file.accounts)
    }

    /// Decrypt a whole file: nonce, then ciphertext-and-tag.
    fn decrypt(&self, bytes: &[u8]) -> std::result::Result<Vec<u8>, aes_gcm::Error> {
        if bytes.len() < NONCE_LEN {
            return Err(aes_gcm::Error);
        }
        let (nonce, body) = bytes.split_at(NONCE_LEN);
        self.cipher.decrypt(Nonce::from_slice(nonce), body)
    }

    /// Encrypt the whole file: nonce, then ciphertext-and-tag.
    fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        // A fresh nonce per write, from the OS RNG. Reusing a nonce under one
        // key is the one mistake that breaks GCM outright: it leaks the XOR of
        // two plaintexts and the authentication key.
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .expect("AES-GCM cannot fail for a 32-byte key and a valid nonce length");

        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        out
    }

    /// Write the map to disk, atomically.
    fn write_file(&self, accounts: &BTreeMap<String, Account>) -> Result<()> {
        let file = StoreFile {
            version: FORMAT_VERSION,
            accounts: accounts.clone(),
        };
        let json = serde_json::to_vec(&file)
            .map_err(|e| BotError::Store(format!("could not serialise the token store: {e}")))?;
        let encrypted = self.encrypt(&json);

        // A temporary file *in the same directory*, so `rename` stays within one
        // filesystem. A temp file in /tmp would make the rename a copy, and a
        // copy is not atomic — which defeats the whole reason for this dance.
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| BotError::Store(format!("could not create {}: {e}", dir.display())))?;
        }

        let temp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&temp, &encrypted)
            .map_err(|e| BotError::Store(format!("could not write {}: {e}", temp.display())))?;
        // 0600 *before* the rename, so the file is never briefly readable by
        // another user in the window between the write and the rename.
        restrict_permissions(&temp);
        std::fs::rename(&temp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            BotError::Store(format!("could not replace {}: {e}", self.path.display()))
        })?;
        Ok(())
    }
}

/// An in-memory store.
#[derive(Debug, Default)]
pub struct EphemeralStore {
    accounts: tokio::sync::RwLock<BTreeMap<String, Account>>,
}

impl TokenStore {
    /// A store that never writes anything.
    pub fn ephemeral() -> Self {
        Self::Ephemeral(Arc::new(EphemeralStore::default()))
    }

    /// An encrypted file store.
    ///
    /// # Errors
    ///
    /// See [`FileStore::open`].
    pub async fn file(path: impl Into<PathBuf>, key: &[u8; KEY_LEN]) -> Result<Self> {
        Ok(Self::File(Arc::new(FileStore::open(path, key).await?)))
    }

    /// Whether this store survives a restart.
    pub fn is_durable(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// The store's path, if it has one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::File(store) => Some(store.path()),
            Self::Ephemeral(_) => None,
        }
    }

    /// Store a link, replacing any existing one for that platform.
    ///
    /// # Errors
    ///
    /// [`BotError::Store`] if the file cannot be written.
    pub async fn put(&self, account: &str, platform: &str, link: StoredLink) -> Result<()> {
        match self {
            Self::File(store) => {
                // The lock spans the read-modify-write, not just the write. Two
                // `/link` calls at once would otherwise each read the old map,
                // each insert their user, and each write — and the second write
                // would drop the first user's token with no error anywhere.
                let _guard = store.lock.lock().await;
                let mut accounts = store.cached.write().await;
                accounts
                    .entry(account.to_owned())
                    .or_default()
                    .links
                    .insert(platform.to_owned(), link);
                store.write_file(&accounts)
            }
            Self::Ephemeral(store) => {
                store
                    .accounts
                    .write()
                    .await
                    .entry(account.to_owned())
                    .or_default()
                    .links
                    .insert(platform.to_owned(), link);
                Ok(())
            }
        }
    }

    /// Fetch a link.
    pub async fn get(&self, account: &str, platform: &str) -> Option<StoredLink> {
        match self {
            Self::File(store) => store
                .cached
                .read()
                .await
                .get(account)
                .and_then(|a| a.links.get(platform))
                .cloned(),
            Self::Ephemeral(store) => store
                .accounts
                .read()
                .await
                .get(account)
                .and_then(|a| a.links.get(platform))
                .cloned(),
        }
    }

    /// Every account and platform that has a link.
    ///
    /// The adapters need this to poll: an adapter does not know who linked until
    /// it asks, and polling is the only way to find out.
    pub async fn accounts(&self) -> Vec<(String, String)> {
        let read = match self {
            Self::File(store) => store.cached.read().await,
            Self::Ephemeral(store) => store.accounts.read().await,
        };
        read.iter()
            .flat_map(|(account, data)| {
                data.links
                    .keys()
                    .map(move |platform| (account.clone(), platform.clone()))
            })
            .collect()
    }

    /// Forget one link.
    ///
    /// # Errors
    ///
    /// [`BotError::Store`] if the file cannot be written.
    pub async fn remove(&self, account: &str, platform: &str) -> Result<bool> {
        match self {
            Self::File(store) => {
                let _guard = store.lock.lock().await;
                let mut accounts = store.cached.write().await;
                let removed = accounts
                    .get_mut(account)
                    .and_then(|a| a.links.remove(platform))
                    .is_some();
                // Written even when nothing was removed, so the file always ends
                // up matching the cache.
                store.write_file(&accounts)?;
                Ok(removed)
            }
            Self::Ephemeral(store) => {
                let mut accounts = store.accounts.write().await;
                Ok(accounts
                    .get_mut(account)
                    .and_then(|a| a.links.remove(platform))
                    .is_some())
            }
        }
    }

    /// How many links are stored, for the startup log.
    pub async fn len(&self) -> usize {
        match self {
            Self::File(store) => store
                .cached
                .read()
                .await
                .values()
                .map(|a| a.links.len())
                .sum(),
            Self::Ephemeral(store) => store
                .accounts
                .read()
                .await
                .values()
                .map(|a| a.links.len())
                .sum(),
        }
    }

    /// Whether the store holds nothing.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// Make a file readable only by its owner.
///
/// A no-op on platforms without Unix permissions rather than an error: the bot
/// should still run on a filesystem that does not support them, and the
/// encryption is the real protection. Where it *is* supported, 0600 matters,
/// because the default umask would otherwise leave the store group- or
/// world-readable.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // Best-effort, and logged rather than fatal: refusing to store a link
        // because of a permissions quirk would lose the user's work.
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not restrict the token store's permissions"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn key(byte: u8) -> [u8; KEY_LEN] {
        [byte; KEY_LEN]
    }

    fn link(token: &str) -> StoredLink {
        StoredLink::new(
            token,
            "content.read library.read",
            Some("p1".into()),
            Some("tok-1".into()),
            now(),
        )
    }

    /// A path inside a fresh directory named for this test, so tests cannot
    /// collide with each other or with a real store. Never a fixed path: a
    /// fixed one makes a test pass exactly once and fail on the second run.
    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lorebot-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("store.bin")
    }

    /// A current-thread runtime, for the tests that do not need `#[tokio::test]`
    /// and touch the filesystem directly.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
    }

    #[tokio::test]
    async fn a_link_survives_a_reopen() {
        let path = temp_path("reopen");
        {
            let store = TokenStore::file(&path, &key(1)).await.expect("opens");
            store
                .put("alice", "discord", link("lh-alice"))
                .await
                .expect("stores");
        }
        let store = TokenStore::file(&path, &key(1)).await.expect("reopens");
        let got = store.get("alice", "discord").await.expect("still there");
        assert_eq!(got.expose(), "lh-alice");
        assert_eq!(got.acting_pseud_id.as_deref(), Some("p1"));
        assert!(got.scope_set().allows(lorebot_api::Scope::LibraryRead));
    }

    #[tokio::test]
    async fn the_file_on_disk_contains_no_plaintext_token() {
        // The assertion worth writing: not "the file parses" but "the secret is
        // not in it". A store that encrypted nothing would pass every other
        // test in this file.
        let path = temp_path("ciphertext");
        let store = TokenStore::file(&path, &key(1)).await.expect("opens");
        store
            .put("alice", "discord", link("lh-supersecret"))
            .await
            .expect("stores");

        let bytes = std::fs::read(&path).expect("readable");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("lh-supersecret"),
            "the token is on disk in the clear:\n{text}"
        );
        // The metadata is encrypted too, so a reader of the file learns neither
        // who is linked nor to which platform. An index of pseudonyms in a file
        // that gets copied around is its own disclosure.
        assert!(
            !text.contains("alice"),
            "the account name is on disk:\n{text}"
        );
        assert!(
            !text.contains("discord"),
            "the platform is on disk:\n{text}"
        );
    }

    #[tokio::test]
    async fn a_different_key_cannot_read_the_store() {
        // The failure mode that matters most: an operator changes
        // LOREBOT_STORE_KEY and every user is silently unlinked. Reported as an
        // error naming the cause, not treated as an empty store.
        let path = temp_path("wrongkey");
        {
            let store = TokenStore::file(&path, &key(1)).await.expect("opens");
            store
                .put("alice", "discord", link("lh-alice"))
                .await
                .expect("stores");
        }
        let error = TokenStore::file(&path, &key(2))
            .await
            .expect_err("a wrong key must not open it");
        let text = error.to_string();
        assert!(
            text.contains("LOREBOT_STORE_KEY"),
            "names the cause: {text}"
        );
        assert!(text.contains("re-link"), "and the remedy: {text}");
    }

    #[tokio::test]
    async fn a_tampered_file_is_refused_rather_than_parsed() {
        // GCM authenticates. A flipped bit must fail loudly: the alternative is
        // a store that decrypts to garbage and a bot that sends garbage as a
        // bearer token.
        let path = temp_path("tamper");
        {
            let store = TokenStore::file(&path, &key(1)).await.expect("opens");
            store
                .put("alice", "discord", link("lh-alice"))
                .await
                .expect("stores");
        }
        let mut bytes = std::fs::read(&path).expect("readable");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).expect("writes");

        let error = TokenStore::file(&path, &key(1))
            .await
            .expect_err("tampering must be caught");
        assert!(error.to_string().contains("decrypt"), "{error}");
    }

    #[tokio::test]
    async fn the_same_plaintext_encrypts_differently_each_time() {
        // A fresh nonce per write. Reusing one would leak the XOR of two
        // plaintexts, and — with the same token stored twice — would make the
        // two ciphertexts identical, which is the visible symptom of it.
        let path = temp_path("nonce");
        let store = TokenStore::file(&path, &key(1)).await.expect("opens");
        store
            .put("alice", "discord", link("same-token"))
            .await
            .expect("stores");
        let first = std::fs::read(&path).expect("readable");
        store
            .put("alice", "discord", link("same-token"))
            .await
            .expect("stores");
        let second = std::fs::read(&path).expect("readable");
        assert_ne!(
            first, second,
            "two writes of the same data are not identical"
        );
    }

    #[tokio::test]
    async fn a_zero_length_file_is_empty_not_corrupt() {
        // What a crash between create and write leaves behind, and what `touch`
        // leaves. Fatal here would mean a recoverable state took down startup.
        let path = temp_path("empty");
        std::fs::write(&path, b"").expect("writes");
        let store = TokenStore::file(&path, &key(1))
            .await
            .expect("an empty file is not corruption");
        assert!(store.is_empty().await);
    }

    #[tokio::test]
    async fn a_missing_file_is_empty_and_nothing_is_created() {
        let path = temp_path("missing");
        let store = TokenStore::file(&path, &key(1)).await.expect("opens");
        assert!(store.is_empty().await);
        assert!(!path.exists(), "opening must not touch the disk");
    }

    #[tokio::test]
    async fn a_newer_format_version_is_refused() {
        // Running an old build against a store a newer one wrote would silently
        // drop whatever it does not understand — losing links with no error.
        let path = temp_path("version");
        {
            let store = TokenStore::file(&path, &key(1)).await.expect("opens");
            store
                .put("alice", "discord", link("lh-alice"))
                .await
                .expect("stores");
        }

        // Rewrite as version 99, re-encrypted with the same key.
        let file = FileStore::open(&path, &key(1)).await.expect("reopens");
        let accounts = file.cached.read().await.clone();
        let json = serde_json::to_vec(&StoreFile {
            version: 99,
            accounts,
        })
        .expect("serialises");
        std::fs::write(&path, file.encrypt(&json)).expect("writes");

        let error = TokenStore::file(&path, &key(1))
            .await
            .expect_err("a newer version must be refused");
        assert!(error.to_string().contains("Upgrade lorebot"), "{error}");
    }

    #[tokio::test]
    async fn an_ephemeral_store_keeps_nothing_on_disk() {
        let store = TokenStore::ephemeral();
        store
            .put("alice", "discord", link("lh-alice"))
            .await
            .expect("stores");
        assert!(store.get("alice", "discord").await.is_some());
        assert!(!store.is_durable(), "and says so");
        assert!(store.path().is_none());
    }

    #[tokio::test]
    async fn removing_a_link_leaves_the_file_consistent() {
        let path = temp_path("remove");
        let store = TokenStore::file(&path, &key(1)).await.expect("opens");
        store
            .put("alice", "discord", link("lh-alice"))
            .await
            .expect("stores");
        store
            .put("bob", "discord", link("lh-bob"))
            .await
            .expect("stores");

        assert!(store.remove("alice", "discord").await.expect("removes"));
        assert!(!store
            .remove("alice", "discord")
            .await
            .expect("removing twice is fine"));

        // Reopened from disk, so the assertion is about the file and not the
        // cache — a remove that updated only the cache would pass otherwise.
        let reopened = TokenStore::file(&path, &key(1)).await.expect("reopens");
        assert!(reopened.get("alice", "discord").await.is_none());
        assert!(
            reopened.get("bob", "discord").await.is_some(),
            "and the other account survives"
        );
    }

    #[tokio::test]
    async fn accounts_lists_every_link_for_polling() {
        let store = TokenStore::ephemeral();
        store
            .put("alice", "discord", link("a"))
            .await
            .expect("stores");
        store
            .put("alice", "matrix", link("b"))
            .await
            .expect("stores");
        store
            .put("bob", "discord", link("c"))
            .await
            .expect("stores");

        let mut pairs = store.accounts().await;
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("alice".to_owned(), "discord".to_owned()),
                ("alice".to_owned(), "matrix".to_owned()),
                ("bob".to_owned(), "discord".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn two_platforms_for_one_account_are_separate_links() {
        // A user who links on Discord and Matrix has two *different* Lorehaven
        // tokens, because each was issued for one platform. Collapsing them
        // would mean revoking one unlinks both.
        let store = TokenStore::ephemeral();
        store
            .put("alice", "discord", link("lh-discord"))
            .await
            .expect("stores");
        store
            .put("alice", "matrix", link("lh-matrix"))
            .await
            .expect("stores");
        assert_eq!(
            store
                .get("alice", "discord")
                .await
                .expect("discord")
                .expose(),
            "lh-discord"
        );
        assert_eq!(
            store.get("alice", "matrix").await.expect("matrix").expose(),
            "lh-matrix"
        );
    }

    #[test]
    fn a_debug_print_never_shows_the_token() {
        // A stray `{:?}` on a link is the most likely way a token reaches a log.
        let printed = format!("{:?}", link("lh-supersecret"));
        assert!(!printed.contains("lh-supersecret"), "{printed}");
        assert!(printed.contains("[REDACTED]"), "{printed}");
    }

    #[test]
    fn a_debug_print_of_the_store_never_shows_the_key() {
        // `FileStore` holds the cipher, and `Aes256Gcm`'s own `Debug` is not
        // something to put in a log. The store's `Debug` is hand-written for
        // exactly this.
        let store = runtime()
            .block_on(TokenStore::file(temp_path("debug"), &key(0xab)))
            .expect("opens");
        let printed = format!("{store:?}");
        assert!(printed.contains("TokenStore::File"), "{printed}");
        assert!(!printed.contains("ab"), "no key bytes: {printed}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_puts_do_not_lose_an_account() {
        // The lock spans the read-modify-write. Without it, eight concurrent
        // `/link` calls each read the old map, each insert their user, and each
        // write — and the last write wins, dropping seven tokens with no error
        // anywhere. Multi-threaded on purpose: a current-thread runtime would
        // serialise the tasks and the race would not happen at all.
        let path = temp_path("concurrent");
        let store = std::sync::Arc::new(TokenStore::file(&path, &key(1)).await.expect("opens"));

        let mut handles = Vec::new();
        for i in 0..8 {
            let store = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                store
                    .put(&format!("user{i}"), "discord", link(&format!("lh-{i}")))
                    .await
                    .expect("stores");
            }));
        }
        for handle in handles {
            handle.await.expect("no task panicked");
        }

        // Reopened from disk, so this is about what was persisted.
        let reopened = TokenStore::file(&path, &key(1)).await.expect("reopens");
        assert_eq!(reopened.len().await, 8, "all eight accounts are on disk");
        for i in 0..8 {
            assert!(
                reopened.get(&format!("user{i}"), "discord").await.is_some(),
                "user{i} survived"
            );
        }
    }

    #[test]
    fn the_store_file_is_not_left_world_readable() {
        // 0600 before the rename, so the file is never briefly readable by
        // another user between the write and the rename.
        let path = temp_path("perms");
        runtime().block_on(async {
            let store = TokenStore::file(&path, &key(1)).await.expect("opens");
            store
                .put("alice", "discord", link("lh-alice"))
                .await
                .expect("stores");
        });

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "mode is {mode:o}, expected 600");
        }
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        // The write goes to `<path>.tmp.<pid>` and is renamed. A leftover temp
        // file is both a leak of an old token and a sign the rename failed.
        let path = temp_path("notemp");
        runtime().block_on(async {
            let store = TokenStore::file(&path, &key(1)).await.expect("opens");
            store
                .put("alice", "discord", link("lh-alice"))
                .await
                .expect("stores");
        });

        let dir = path.parent().expect("a parent");
        let leftovers: Vec<String> = std::fs::read_dir(dir)
            .expect("readable")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn a_store_in_a_missing_directory_is_created_on_first_write() {
        // An operator sets LOREBOT_STORE_PATH=/var/lib/lorebot/store.bin on a
        // fresh host. Failing on the first `/link` would be a support ticket;
        // creating the directory is what they meant.
        let dir = std::env::temp_dir().join(format!("lorebot-store-mkdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("store.bin");

        runtime().block_on(async {
            let store = TokenStore::file(&path, &key(1)).await.expect("opens");
            store
                .put("alice", "discord", link("lh-alice"))
                .await
                .expect("creates the directory on write");
        });
        assert!(path.exists(), "{}", path.display());
    }
}

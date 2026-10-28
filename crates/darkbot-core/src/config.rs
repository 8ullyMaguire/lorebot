//! Environment-driven configuration.
//!
//! Ported from `archivist-core::config`. The shape is the same — everything
//! from the environment, nothing from a file that could be committed by
//! accident — with the validation moved to the front.
//!
//! ## The rename, and the shim that goes with it
//!
//! This bot used to be `lorebot` and read `LOREBOT_*`; it is now `darkbot` and
//! reads `DARKBOT_*`. The old prefix still works, per variable, with a warning
//! that names the variable that moved — see [`var`]. A unit file and a binary
//! are updated at different times, and a deployment that silently lost its
//! configuration (or quietly stopped honouring `…_EPHEMERAL_STORE`) at the
//! moment of the rename would be a worse outcome than a warning.
//!
//! [`BotConfig::from_env`] validates *at startup* and returns an error rather
//! than letting a bad value through to first use. That ordering is the whole
//! design of this module: a bot that starts with a 4-byte store key and
//! discovers it on the first read has already written plaintext-shaped data
//! with a weak key, and a deployment that starts with no instance URL fails on
//! the first command instead of at boot, where an operator is watching.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::error::{BotError, Result};

/// The platforms this bot can run on.
///
/// A new platform is a variant here and a crate. The conformance suite takes
/// `impl Adapter`, so it runs against a fourth adapter without being touched.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    /// Discord.
    Discord,
    /// Telegram.
    Telegram,
    /// Matrix.
    Matrix,
}

impl Platform {
    /// The name used in environment variables and in store keys.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Telegram => "telegram",
            Self::Matrix => "matrix",
        }
    }

    /// Every platform, in a stable order.
    pub fn all() -> [Self; 3] {
        [Self::Discord, Self::Telegram, Self::Matrix]
    }

    /// Parse a platform name.
    pub fn parse(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "discord" => Ok(Self::Discord),
            "telegram" => Ok(Self::Telegram),
            "matrix" => Ok(Self::Matrix),
            other => Err(BotError::Config(format!(
                "unknown platform {other:?} — expected one of discord, telegram, matrix"
            ))),
        }
    }
}

/// The minimum store key length, in bytes.
///
/// AES-256-GCM takes a 32-byte key. Accepting something shorter would mean
/// padding or truncating, and both are worse than refusing: truncation means
/// two different configured keys can open the same file, and padding means a
/// weak key silently becomes a strong-looking one.
pub const MIN_STORE_KEY_BYTES: usize = 32;

/// A platform's credentials and options.
#[derive(Debug, Clone, Default)]
pub struct PlatformConfig {
    /// The bot's own token for this platform, as the platform issued it.
    pub bot_token: String,
    /// Rate limit: requests per minute.
    pub requests_per_minute: u32,
    /// How long to wait between polls.
    pub poll_interval: Duration,
}

/// The whole configuration.
#[derive(Debug, Clone)]
pub struct BotConfig {
    /// The Darkhold instance, ending in `/api/v1`.
    pub instance_url: String,
    /// The human-facing name of the instance, shown in notices.
    pub instance_name: String,
    /// Enabled platforms and their settings.
    pub platforms: BTreeMap<Platform, PlatformConfig>,
    /// Where the encrypted token store lives.
    pub store_path: std::path::PathBuf,
    /// The store's encryption key, exactly 32 bytes.
    pub store_key: [u8; MIN_STORE_KEY_BYTES],
    /// Keep tokens in memory only, never writing them to disk.
    pub ephemeral_store: bool,
}

impl BotConfig {
    /// Read and validate the configuration from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`BotError::Config`] naming the offending variable. Every
    /// message names the variable, because an operator reading a log at 3am
    /// needs to know which one to set and not which of eleven is missing.
    pub fn from_env() -> Result<Self> {
        let instance_url = required("DARKBOT_DARKHOLD_URL")?;
        let store_key_raw = required("DARKBOT_STORE_KEY")?;

        let store_key = decode_store_key(&store_key_raw)?;

        let mut platforms = BTreeMap::new();
        for platform in Platform::all() {
            let upper = platform.as_str().to_ascii_uppercase();
            // A platform with no token is *disabled*, not broken: an operator
            // running only Discord should not have to invent a Telegram token
            // to start the process.
            match var(&format!("DARKBOT_{upper}_TOKEN")) {
                Some(token) if !token.trim().is_empty() => {
                    platforms.insert(
                        platform,
                        PlatformConfig {
                            bot_token: token,
                            requests_per_minute: optional_u32(&format!(
                                "DARKBOT_{upper}_RATE_LIMIT"
                            ))?
                            .unwrap_or(20),
                            poll_interval: Duration::from_secs(
                                optional_u32(&format!("DARKBOT_{upper}_POLL_SECS"))?.unwrap_or(30)
                                    as u64,
                            ),
                        },
                    );
                }
                // A platform with no token is *disabled*, not broken: an
                // operator running only Discord should not have to invent a
                // Telegram token to start the process.
                _ => {}
            }
        }

        if platforms.is_empty() {
            return Err(no_platforms_error());
        }

        let config = Self {
            instance_url: normalise_instance_url(&instance_url)?,
            instance_name: var("DARKBOT_INSTANCE_NAME")
                .unwrap_or_else(|| "this Darkhold instance".into()),
            platforms,
            store_path: var("DARKBOT_STORE_PATH")
                .map_or_else(default_store_path, std::path::PathBuf::from),
            store_key,
            ephemeral_store: env_flag("DARKBOT_EPHEMERAL_STORE")?,
        };
        config.validate()?;
        Ok(config)
    }

    /// Check the assembled configuration.
    ///
    /// Separate from `from_env` so a config built in a test is checked the same
    /// way one read from the environment is.
    ///
    /// # Errors
    ///
    /// Returns [`BotError::Config`] on the first problem found.
    pub fn validate(&self) -> Result<()> {
        if self.platforms.is_empty() && !self.ephemeral_store {
            return Err(BotError::Config("no platforms enabled".into()));
        }
        if self
            .platforms
            .values()
            .any(|p| p.bot_token.trim().is_empty())
        {
            return Err(BotError::Config(
                "a platform is enabled with an empty bot token".into(),
            ));
        }
        // A rate limit of zero would block every request forever, which looks
        // exactly like a network failure and would be reported as one.
        if self.platforms.values().any(|p| p.requests_per_minute == 0) {
            return Err(BotError::Config(
                "a platform has a rate limit of 0 requests per minute, which would block \
                 every message"
                    .into(),
            ));
        }
        Ok(())
    }

    /// The API base for a public path, e.g. `public/search`.
    pub fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.instance_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// The human-facing URL of a work, for the "continue on Darkhold" link.
    ///
    /// §23.2 lists "return a link to continue on Darkhold" as one of the eight
    /// actions, and it is the one that needs no door: it is a client-side URL.
    /// Built from the API base by dropping `/api/v1`, because the reader is
    /// going to a page, not to the API.
    pub fn web_url(&self, path: &str) -> String {
        let base = self
            .instance_url
            .trim_end_matches('/')
            .trim_end_matches("/api/v1");
        format!("{}/{}", base, path.trim_start_matches('/'))
    }
}

/// The error for "no platform is enabled", naming the variables to set.
///
/// Separate from [`BotConfig::validate`]'s message on purpose: this one is read
/// by an operator at startup, who needs to know *which* variable to set, and
/// `validate` is called by a test or a caller that only needs to know the
/// config is unusable.
fn no_platforms_error() -> BotError {
    BotError::Config(
        "no platforms enabled — set at least one of DARKBOT_DISCORD_TOKEN, \
         DARKBOT_TELEGRAM_TOKEN, DARKBOT_MATRIX_TOKEN"
            .into(),
    )
}

/// Normalise and check the instance URL.
fn normalise_instance_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(BotError::Config(format!(
            "DARKBOT_DARKHOLD_URL must start with http:// or https://, got {trimmed:?}"
        )));
    }
    if !trimmed.ends_with("/api/v1") {
        return Err(BotError::Config(format!(
            "DARKBOT_DARKHOLD_URL must end with /api/v1, got {trimmed:?} — the bot talks to \
             the versioned API and nothing else"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Decode the store key, refusing anything that is not exactly 32 bytes.
fn decode_store_key(raw: &str) -> Result<[u8; MIN_STORE_KEY_BYTES]> {
    let trimmed = raw.trim();

    // Hex is tried first because it is what `openssl rand -hex 32` produces and
    // an operator will reach for that first.
    let bytes: Vec<u8> = if trimmed.len() == MIN_STORE_KEY_BYTES * 2
        && trimmed.chars().all(|c| c.is_ascii_hexdigit())
    {
        (0..MIN_STORE_KEY_BYTES)
            .map(|i| u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16).expect("checked hex"))
            .collect()
    } else {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(trimmed)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed))
            .map_err(|e| {
                BotError::Config(format!(
                    "DARKBOT_STORE_KEY must be {MIN_STORE_KEY_BYTES} bytes, as hex or base64 — \
                     could not decode it: {e}"
                ))
            })?
    };

    if bytes.len() != MIN_STORE_KEY_BYTES {
        return Err(BotError::Config(format!(
            "DARKBOT_STORE_KEY must be {MIN_STORE_KEY_BYTES} bytes, got {}. Generate one with \
             `openssl rand -base64 32`. A shorter key would have to be padded or truncated, and \
             truncation means two different configured keys can open the same store.",
            bytes.len()
        )));
    }

    let mut key = [0u8; MIN_STORE_KEY_BYTES];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn required(name: &str) -> Result<String> {
    var(name)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| BotError::Config(format!("{name} is required but not set")))
}

fn optional_u32(name: &str) -> Result<Option<u32>> {
    match var(name) {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => Ok(None),
        Some(raw) => raw
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| BotError::Config(format!("{name} must be a number, got {raw:?}"))),
    }
}

fn env_flag(name: &str) -> Result<bool> {
    match var(name) {
        None => Ok(false),
        Some(raw) => env_flag_value_in(&raw, name),
    }
}

/// Read a variable under its current `DARKBOT_` name, falling back to the
/// pre-rename `LOREBOT_` spelling.
///
/// The fallback is the whole compatibility story for the rename. A unit file
/// and a binary are updated at different times: without this, an operator who
/// upgraded the binary first would start with no instance URL and an error that
/// names a variable they have never set, or — worse — would lose
/// `…_EPHEMERAL_STORE` and silently begin writing a token file to disk.
///
/// The old spelling is read, never preferred, and it is announced: a shim that
/// works forever without saying so is one nobody removes.
fn var(name: &str) -> Option<String> {
    if let Ok(value) = std::env::var(name) {
        return Some(value);
    }
    let legacy = legacy_name(name)?;
    let value = std::env::var(&legacy).ok()?;
    warn_renamed(&legacy, name);
    Some(value)
}

/// The pre-rename spelling of a current variable.
///
/// Split out from [`var`] so the mapping is testable without touching the
/// process environment, which is racy across a parallel test binary.
fn legacy_name(name: &str) -> Option<String> {
    if let Some((_, legacy)) = LEGACY_ALIASES.iter().find(|(current, _)| *current == name) {
        return Some((*legacy).to_owned());
    }
    name.strip_prefix("DARKBOT_")
        .map(|rest| format!("LOREBOT_{rest}"))
}

/// Names whose pre-rename spelling is not a prefix swap.
///
/// The instance URL changed in both halves: it was `LOREBOT_LOREHAVEN_URL` and
/// is now `DARKBOT_DARKHOLD_URL`. The prefix rule alone would look for
/// `LOREBOT_DARKHOLD_URL`, which no deployment has ever set, so the shim would
/// be a shim for nothing.
const LEGACY_ALIASES: &[(&str, &str)] = &[("DARKBOT_DARKHOLD_URL", "LOREBOT_LOREHAVEN_URL")];

/// Warn once per variable that the old spelling was used.
fn warn_renamed(legacy: &str, current: &str) {
    static SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(BTreeSet::new()));
    let first_use = match seen.lock() {
        Ok(mut set) => set.insert(current.to_owned()),
        // A poisoned lock means another thread panicked while warning. The
        // warning is not worth a panic of its own.
        Err(poisoned) => poisoned.into_inner().insert(current.to_owned()),
    };
    if first_use {
        tracing::warn!(
            old = legacy,
            new = current,
            "configuration variable renamed — the old spelling still works, but move it: \
             this shim is temporary"
        );
    }
}

/// The default token store, and the pre-rename file it replaces.
///
/// The default name moved with the binary. An existing deployment has its links
/// in the old file, and a bot that quietly started a new empty store would
/// unlink every reader on restart — so the old file is used, and named, when it
/// is the only one on disk.
fn default_store_path() -> std::path::PathBuf {
    store_path_in(std::path::Path::new("."))
}

/// The default store name, resolved against `base`, preferring the new file.
///
/// Takes the directory rather than reading the working directory so a test can
/// probe both files in a directory of its own; `set_current_dir` in a test is
/// process-wide and would leak into every test running beside it.
fn store_path_in(base: &std::path::Path) -> std::path::PathBuf {
    let current = base.join("darkbot-store.json");
    let legacy = base.join("lorebot-store.json");
    if !current.exists() && legacy.exists() {
        tracing::warn!(
            old = %legacy.display(),
            new = %current.display(),
            "using the pre-rename token store — rename the file to the new name to stop this \
             warning, and keep every reader linked"
        );
        return legacy;
    }
    current
}

/// The boolean parser, split out so a test can reach it without touching the
/// process environment.
#[cfg(test)]
fn env_flag_value(raw: &str) -> Result<bool> {
    env_flag_value_in(raw, "DARKBOT_EPHEMERAL_STORE")
}

fn env_flag_value_in(raw: &str, name: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        other => Err(BotError::Config(format!(
            "{name} must be a boolean, got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config that passes validation, so each test can break exactly one
    /// thing. Building a valid one inline in every test would make it easy to
    /// change a field the test is not about.
    fn valid() -> BotConfig {
        let mut platforms = BTreeMap::new();
        platforms.insert(
            Platform::Discord,
            PlatformConfig {
                bot_token: "discord-token".into(),
                requests_per_minute: 20,
                poll_interval: Duration::from_secs(30),
            },
        );
        BotConfig {
            instance_url: "https://lore.example.test/api/v1".into(),
            instance_name: "example".into(),
            platforms,
            store_path: "store.json".into(),
            store_key: [7u8; MIN_STORE_KEY_BYTES],
            ephemeral_store: false,
        }
    }

    #[test]
    fn a_valid_config_passes() {
        valid().validate().expect("a good config validates");
    }

    #[test]
    fn the_instance_url_must_be_absolute_and_versioned() {
        for bad in [
            "lore.example.test",
            "https://lore.example.test",
            "ftp://x/api/v1",
        ] {
            let error = normalise_instance_url(bad).expect_err("must reject");
            assert!(
                error.to_string().contains("DARKBOT_DARKHOLD_URL"),
                "the message names the variable, not just the value: {error}"
            );
        }
    }

    #[test]
    fn a_trailing_slash_on_the_instance_url_is_tolerated() {
        // Operators paste URLs with a trailing slash. Rejecting that is a
        // support question, not a safety one, so it is normalised rather than
        // refused.
        assert_eq!(
            normalise_instance_url("https://lore.example.test/api/v1/").expect("normalised"),
            "https://lore.example.test/api/v1"
        );
    }

    #[test]
    fn a_store_key_that_is_too_short_is_rejected_with_the_minimum() {
        // The test worth writing: not just "rejected", but "says how to fix it".
        // A bot that starts with a 4-byte key has already written with it.
        let error = decode_store_key(&base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            [1u8; 16],
        ))
        .expect_err("16 bytes is not enough");
        let text = error.to_string();
        assert!(text.contains("32"), "names the minimum: {text}");
        assert!(text.contains("openssl rand"), "and how to make one: {text}");
    }

    #[test]
    fn a_store_key_that_is_not_base64_or_hex_is_rejected() {
        let error = decode_store_key("this is not a key at all!!").expect_err("must reject");
        assert!(error.to_string().contains("DARKBOT_STORE_KEY"), "{error}");
    }

    #[test]
    fn hex_and_base64_keys_both_work_and_give_the_same_bytes() {
        // `openssl rand -hex 32` and `openssl rand -base64 32` are what an
        // operator will actually run, and the hex path is tried first, so both
        // have to land on the same key.
        let raw = [9u8; MIN_STORE_KEY_BYTES];
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
        assert_eq!(
            decode_store_key(&hex).expect("hex"),
            decode_store_key(&b64).expect("base64"),
            "two spellings of one key must open the same store"
        );
    }

    #[test]
    fn a_platform_with_no_token_is_disabled_not_broken() {
        // Running only Discord should not require inventing a Telegram token.
        assert!(Platform::parse("discord").is_ok());
        assert!(Platform::parse("Discord").is_ok(), "case-insensitive");
        let error = Platform::parse("slack").expect_err("not a platform here yet");
        assert!(error.to_string().contains("slack"), "{error}");
    }

    #[test]
    fn a_zero_rate_limit_is_rejected() {
        // A limit of zero blocks every message forever, which presents exactly
        // like a network failure and would be reported as one.
        let mut config = valid();
        config
            .platforms
            .get_mut(&Platform::Discord)
            .expect("discord is present")
            .requests_per_minute = 0;
        let error = config.validate().expect_err("must reject");
        assert!(
            error.to_string().contains("0 requests per minute"),
            "{error}"
        );
    }

    #[test]
    fn no_platforms_at_all_is_rejected() {
        // Two messages, for two callers, and deliberately different.
        // `validate` is the programmatic check and says what is wrong; the
        // `from_env` path is where an operator is looking for a variable name,
        // so it says which three to set. An earlier draft of this test asserted
        // that `validate` named the variables, which was wrong — and the
        // mistake was worth making, because it showed the two messages had
        // collapsed into one.
        let mut config = valid();
        config.platforms.clear();
        assert!(
            config
                .validate()
                .expect_err("must reject")
                .to_string()
                .contains("no platforms"),
            "validate() says what is wrong"
        );

        let error = no_platforms_error();
        for name in ["DISCORD", "TELEGRAM", "MATRIX"] {
            assert!(error.to_string().contains(name), "names {name}: {error}");
        }
    }

    #[test]
    fn the_web_url_drops_the_api_prefix() {
        // "Return a link to continue on Darkhold" sends a reader to a page,
        // not to the API. Getting this wrong produces a link to a JSON
        // endpoint, which is worse than no link.
        let config = valid();
        assert_eq!(
            config.web_url("works/abc"),
            "https://lore.example.test/works/abc"
        );
        assert_eq!(
            config.endpoint("public/search"),
            "https://lore.example.test/api/v1/public/search"
        );
    }

    #[test]
    fn the_pre_rename_variable_names_are_derived_from_the_current_ones() {
        // The shim's mapping, asserted directly: every variable this bot reads
        // has exactly one predecessor, and it is the one a deployed unit file
        // already sets. A variable that is not ours has no predecessor, so the
        // fallback cannot reach into some other program's environment.
        assert_eq!(
            legacy_name("DARKBOT_DARKHOLD_URL").as_deref(),
            Some("LOREBOT_LOREHAVEN_URL"),
            "the instance URL, which carried both old names"
        );
        assert_eq!(
            legacy_name("DARKBOT_STORE_KEY").as_deref(),
            Some("LOREBOT_STORE_KEY")
        );
        assert_eq!(
            legacy_name("DARKBOT_EPHEMERAL_STORE").as_deref(),
            Some("LOREBOT_EPHEMERAL_STORE")
        );
        assert_eq!(legacy_name("RUST_LOG"), None, "not ours, no fallback");
    }

    #[test]
    fn the_default_store_is_the_new_name_unless_only_the_old_file_exists() {
        // Resolved against a directory of its own: the process working directory
        // is shared with every test running beside this one.
        let dir = std::env::temp_dir().join(format!("darkbot-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        assert_eq!(
            store_path_in(&dir),
            dir.join("darkbot-store.json"),
            "a fresh install gets the new name"
        );

        std::fs::write(dir.join("lorebot-store.json"), b"{}").expect("old store");
        assert_eq!(
            store_path_in(&dir),
            dir.join("lorebot-store.json"),
            "the old file is the one with the links in it"
        );

        std::fs::write(dir.join("darkbot-store.json"), b"{}").expect("new store");
        assert_eq!(
            store_path_in(&dir),
            dir.join("darkbot-store.json"),
            "once the new file exists it wins"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_env_flag_rejects_a_value_that_is_not_a_boolean() {
        // The parse itself is exercised through `from_env` in the integration
        // test, where the environment can be set. Re-implementing the match
        // here — which is what an earlier draft of this test did — tests a
        // copy, and a copy is exactly what drifts.
        let error = env_flag_value("maybe").expect_err("not a boolean");
        assert!(
            error.to_string().contains("DARKBOT_EPHEMERAL_STORE"),
            "names the variable: {error}"
        );
    }

    #[test]
    fn the_boolean_spellings_are_exactly_the_documented_ones() {
        // Asserted against the parser this time, through a helper that takes
        // the value directly rather than reading the process environment —
        // `std::env::set_var` is unsafe in edition 2024 and racy across a
        // parallel test binary.
        for raw in ["1", "true", "YES", "on"] {
            assert!(env_flag_value(raw).expect("a true spelling"), "{raw:?}");
        }
        for raw in ["0", "false", "no", "off", ""] {
            assert!(!env_flag_value(raw).expect("a false spelling"), "{raw:?}");
        }
    }
}

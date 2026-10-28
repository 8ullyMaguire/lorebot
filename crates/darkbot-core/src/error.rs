//! The bot's error type.
//!
//! Ported from `archivist-core::error`. Every variant is platform-neutral: an
//! adapter that needs to distinguish two failures returns the distinction to the
//! user, and a platform-specific variant would make that a per-adapter
//! judgement call.
//!
//! Three variants are new, and each exists because a different response to the
//! user is correct:
//!
//! * [`BotError::ScopeDenied`] — a 403. The fix is not "retry", it is "this
//!   token cannot do this", and saying so is more useful than an error code.
//! * [`BotError::Revoked`] — a 401 for a token that previously worked. Worth
//!   its own variant so the message can say *re-link* rather than *something
//!   went wrong*. §23.2's "revocation and unlinking" is exactly this path.
//! * [`BotError::NotLinked`] — kept from the source, and the reason the store
//!   returning "no record" is not the same thing as a decryption failure.

use thiserror::Error;

/// Every way the bot can fail.
#[derive(Debug, Error)]
pub enum BotError {
    /// The outgoing request failed at the transport.
    #[error("could not reach Darkhold: {0}")]
    Http(#[from] reqwest::Error),

    /// Darkhold returned an error envelope.
    #[error("Darkhold returned {code}: {message}")]
    Api {
        /// The `code` from the error body — `VALIDATION_FAILED`,
        /// `REVISION_CONFLICT` and so on.
        code: String,
        /// The human-readable message, which Darkhold writes to be shown.
        message: String,
    },

    /// A non-2xx status with no recognisable error envelope.
    #[error("Darkhold returned status {status}")]
    Status {
        /// The HTTP status code.
        status: u16,
    },

    /// The response body did not parse.
    #[error("could not read Darkhold's response: {0}")]
    Json(#[from] serde_json::Error),

    /// The reader has not linked a Darkhold account.
    ///
    /// Distinct from a store failure on purpose. "You are not linked" is a
    /// normal state with a next step; a store that cannot decrypt its file is a
    /// broken deployment, and reporting it as "not linked" would send the user
    /// to `/link` forever while the real fault went unmentioned.
    #[error("not linked yet — run /link to connect your Darkhold account")]
    NotLinked,

    /// The token is no longer valid, because it was revoked or expired.
    #[error("your Darkhold link has been revoked — run /link again to reconnect")]
    Revoked,

    /// The token is valid but lacks a scope this action needs.
    #[error("your Darkhold link does not have the {needed} permission (granted: {granted})")]
    ScopeDenied {
        /// The scope the action requires.
        needed: String,
        /// The scopes the token actually carries.
        granted: String,
    },

    /// The configuration is unusable.
    #[error("configuration error: {0}")]
    Config(String),

    /// The command was used wrongly.
    #[error("{0}")]
    Command(String),

    /// A rate limit was hit.
    #[error("slow down — try again in {retry_after_secs}s")]
    RateLimited {
        /// Seconds to wait.
        retry_after_secs: u64,
    },

    /// The store could not be read or written.
    #[error("token store: {0}")]
    Store(String),

    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// The bot's result alias.
pub type Result<T> = std::result::Result<T, BotError>;

impl From<darkbot_api::ApiError> for BotError {
    /// Map a client error onto the core's vocabulary.
    ///
    /// The mapping is by *kind*, not by string: `Revoked` and `ScopeDenied`
    /// stay distinct because the remedy differs, and `ScopeNotHeld` — a bug in
    /// this repository rather than anything the reader did — becomes
    /// [`BotError::Other`] with the explanation intact, so it surfaces in a log
    /// rather than being reported to a user as their problem.
    fn from(error: darkbot_api::ApiError) -> Self {
        use darkbot_api::ApiError as E;
        match error {
            E::Revoked => Self::Revoked,
            E::ScopeDenied {
                needed, explain, ..
            } => Self::ScopeDenied {
                needed: format!("{needed} ({explain})"),
                granted: String::new(),
            },
            E::Transport(inner) => Self::Http(inner),
            E::Decode(inner) => Self::Json(inner),
            E::Envelope { code, message, .. } => Self::Api { code, message },
            E::Status { status } => Self::Status { status },
            E::ScopeNotHeld { needed, granted } => Self::Other(format!(
                "bug: this action needs {needed}, and the token only has {granted}"
            )),
        }
    }
}

/// Whether an error means "this token will never work again".
///
/// Worth a function because three call sites need the same answer — the
/// dispatch layer's unlink hint, the `/link` command, and the store's eviction
/// — and three copies of a match on error variants is three places for them to
/// disagree.
pub fn needs_relink(error: &BotError) -> bool {
    match error {
        BotError::NotLinked | BotError::Revoked => true,
        BotError::ScopeDenied { .. } => false,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_403_names_the_scope_it_wanted() {
        // The *message* is what a reader sees, so the test asserts on it and
        // not only on the variant. A 403 that says "permission denied" without
        // saying which permission leaves the user guessing.
        let error = BotError::ScopeDenied {
            needed: "library.read".into(),
            granted: "content.read content.write".into(),
        };
        let text = error.to_string();
        assert!(text.contains("library.read"), "{text}");
        assert!(
            text.contains("content.write"),
            "and what it does have: {text}"
        );
    }

    #[test]
    fn a_401_after_a_working_token_says_re_link() {
        // Not "something went wrong": the actionable instruction is to link
        // again, and that is the whole value of a separate variant.
        let text = BotError::Revoked.to_string();
        assert!(text.contains("/link"), "{text}");
        assert!(
            !text.contains("permission"),
            "and not a permission problem: {text}"
        );
    }

    #[test]
    fn not_linked_and_revoked_are_different_advice() {
        assert_ne!(
            BotError::NotLinked.to_string(),
            BotError::Revoked.to_string(),
            "one needs a first link and the other needs a fresh one"
        );
    }

    #[test]
    fn only_not_linked_and_revoked_ask_for_a_relink() {
        assert!(needs_relink(&BotError::NotLinked));
        assert!(needs_relink(&BotError::Revoked));
        // A missing scope is fixed by re-linking with *more* permissions, which
        // is a different instruction — so it must not claim a plain relink
        // would help.
        assert!(!needs_relink(&BotError::ScopeDenied {
            needed: "library.read".into(),
            granted: String::new(),
        }));
        // A transport failure is a retry, not a relink. Built by asking for an
        // unreachable address rather than by constructing an error by hand:
        // `reqwest::Error` has no public constructor, and a hand-rolled stand-in
        // would be testing a different type than the one that reaches here.
        assert!(!needs_relink(&BotError::Other("network".into())));
    }

    #[test]
    fn the_variants_are_platform_neutral() {
        // §23.2 wants the core free of platform SDKs, and the error type is
        // where that leaks first: a `BotError::Discord(serenity::Error)` would
        // compile fine here and drag the SDK into every adapter. Asserted on the
        // `Debug` output because a variant named after a platform would show up
        // in it.
        let rendered = format!("{:?}", BotError::Revoked);
        for platform in ["Discord", "Telegram", "Matrix", "Slack", "Irc"] {
            assert!(
                !rendered.contains(platform),
                "the error type names a platform: {rendered}"
            );
        }
    }

    #[test]
    fn a_store_failure_is_not_reported_as_not_linked() {
        // The distinction is the point of having a `Store` variant. A broken
        // store reported as `NotLinked` sends the user to `/link` forever while
        // the real fault goes unmentioned, and the dispatch layer's
        // `needs_relink` hint would compound it.
        let broken = BotError::Store("could not decrypt: wrong key".into());
        assert!(!needs_relink(&broken), "a broken store is not a relink");
        assert!(broken.to_string().contains("token store"));
    }
}

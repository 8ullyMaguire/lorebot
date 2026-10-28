//! # darkbot-core
//!
//! Platform-neutral core for the Darkhold companion bot.
//!
//! This crate holds every piece of logic that is the same on Discord, Telegram
//! and Matrix. An adapter translates between a platform's message model and
//! [`core::PlatformMessage`], and nothing else — no command parsing, no API
//! calls, no token handling. That is the seam spec §23.2 asks for when it says
//! the bot is a *thin REST client* with a per-platform render matrix: a render
//! matrix only works if there is one message shape to render.
//!
//! ## What is here
//!
//! * [`core`] — the message IR. Ported from the FicHub-era `archivist-core`
//!   essentially unchanged, because it is the seam.
//! * [`error`] — [`error::BotError`]. Platform-neutral, with distinct
//!   variants for a revoked token and a denied scope because the advice to the
//!   user differs.
//! * [`config`] — environment-driven [`config::BotConfig`], validated at
//!   startup rather than at first use.
//! * [`store`] — the encrypted token store. **Not a database**.
//! * [`cache`] — bounded in-process response and cursor cache.
//! * [`dispatch`] — the eight §23.2 actions, and the commands that reach them.
//! * [`privacy`] — the §23.2 disclosures, in one place.
//!
//! ## The two constraints that shape everything
//!
//! **No database, ever.** darkbot is a REST client. The database-reachability
//! assertion is a test over the workspace lock file, not a convention, and it
//! has been run in the failing direction to prove it can fail.
//!
//! **Private results stay private.** §23.2 says no private library results in
//! public channels, and Darkhold cannot enforce that — it never learns where a
//! message went. So [`core::PlatformMessage::Ephemeral`] is the enforcement
//! point, every private action returns one, and an adapter that cannot render a
//! private message fails rather than falling back to public.
//!
//! ## Dependencies
//!
//! `darkbot-api` for the wire, and the standard set. No platform SDK, no
//! database driver, no Redis. Each of those absences is load-bearing and each
//! has a test.

pub mod cache;
pub mod config;
pub mod core;
pub mod dispatch;
pub mod error;
pub mod privacy;
pub mod store;

pub use config::{BotConfig, Platform};
pub use core::{ActionRow, Button, ButtonStyle, PlatformMessage, RichItem, SelectOption};
pub use error::{needs_relink, BotError, Result};

/// The core's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the bot is for, in one line, for `/help` and the adapter banners.
pub const TAGLINE: &str = "a thin REST client for your Darkhold instance";

#[cfg(test)]
mod db_reachability {
    //! The §23.2 "the bot never touches the database" requirement, as a
    //! property rather than a convention.
    //!
    //! A comment saying "do not add a database driver" is a request. This is a
    //! test that fails when one appears, and it has been run in the failing
    //! direction to prove it can fail — a test never seen red is not known to
    //! work.

    /// Anything that could open a connection to a database.
    const BANNED_CRATES: &[&str] = &[
        "sqlx",
        "rusqlite",
        "libsqlite3-sys",
        "diesel",
        "postgres",
        "tokio-postgres",
        "deadpool-postgres",
        "surrealdb",
        "rocksdb",
        "redb",
    ];

    /// Strings that would indicate a hand-rolled connection the lock file
    /// would not have caught.
    const BANNED_STRINGS: &[&str] = &[
        "DATABASE_URL",
        "postgres://",
        "postgresql://",
        "sqlite://",
        "sqlite3://",
    ];

    /// The workspace lock file, reached from any crate in the workspace.
    fn lock_path() -> std::path::PathBuf {
        // CARGO_MANIFEST_DIR is this crate; two levels up is the workspace root
        // that owns Cargo.lock. Resolved at runtime rather than with
        // `env!("CARGO_WORKSPACE_DIR")`, which is not stable on the 1.82 floor.
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("Cargo.lock")
    }

    /// Every `.rs` file in this workspace's `crates/`.
    fn source_files() -> Vec<std::path::PathBuf> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(root.join("crates")) else {
            return out;
        };
        for entry in entries.flatten() {
            collect_rs(&entry.path(), &mut out);
        }
        out
    }

    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn no_code_path_reaches_a_database() {
        let lock = std::fs::read_to_string(lock_path())
            .expect("the workspace has a Cargo.lock; if it does not, run `cargo fetch`");

        for banned in BANNED_CRATES {
            // Matched on the `name = "..."` line specifically, so a crate that
            // merely *mentions* another in a comment or a dependency alias is
            // not mistaken for a dependency on it.
            let needle = format!("name = \"{banned}\"");
            assert!(
                !lock.contains(&needle),
                "darkbot must not depend on {banned}: spec §23.2 says the bot is a REST client \
                 and never touches the Darkhold database. Found in Cargo.lock."
            );
        }

        // The file holding this very list is skipped. Otherwise the test fails
        // on itself: the banned strings must be written down here to be
        // searched for, and a scanner that cannot tell a mention from a use is
        // a scanner that ends up switched off.
        // Both sides are canonicalised. `source_files()` walks from
        // `CARGO_MANIFEST_DIR/../..`, so its paths carry `../..` in the middle,
        // and a plain `==` against an absolute path silently never matches —
        // which is how this test first ended up failing on the very file that
        // defines the list it searches for.
        let this_file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs")
            .canonicalize()
            .expect("this crate's own lib.rs exists");

        for path in source_files() {
            if path.canonicalize().is_ok_and(|p| p == this_file) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for banned in BANNED_STRINGS {
                assert!(
                    !text.contains(banned),
                    "{} mentions {banned}. A hand-rolled connection would not appear in \
                     Cargo.lock, so the lock file alone is not enough.",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn the_assertion_itself_is_not_vacuous() {
        // If `lock_path()` pointed at a file that did not exist, or at an
        // empty one, the test above would pass for the wrong reason. This
        // checks the two things that would make it do that.
        let path = lock_path();
        assert!(path.exists(), "Cargo.lock not found at {}", path.display());

        let lock = std::fs::read_to_string(&path).expect("readable");
        assert!(
            lock.contains("name = \""),
            "Cargo.lock has no package names, so the banned-crate scan would match nothing"
        );

        // And the source scan is not reading zero files.
        let files = source_files();
        assert!(
            files.len() >= 3,
            "expected to scan this crate's source, found {} files",
            files.len()
        );
    }
}

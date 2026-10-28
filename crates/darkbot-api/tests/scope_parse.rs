//! The test that was missing: `ScopeSet::parse` must accept the separator
//! Darkhold's own API returns.
//!
//! ## The defect
//!
//! `ScopeSet::parse` splits on **whitespace**:
//!
//! ```text
//! for part in raw.split_whitespace() { ... }
//! ```
//!
//! and `to_wire` joins with a **space**. So the pair is self-consistent and
//! invisible to a test written against the pair — which is why 252 tests pass.
//!
//! But `GET /me/credential` returns scopes as a **JSON array of strings**, and
//! the e2e suite's link is seeded from what a real Darkhold reports. A reader
//! who re-links, or any code path that stores the server's own list, produces
//!
//! ```text
//! "content.read,library.read"
//! ```
//!
//! `split_whitespace` yields ONE part — `"content.read,library.read"` — which
//! `Scope::parse` does not recognise. So it goes to `unknown`, `known` is empty,
//! and `allows(anything)` is false for every scope.
//!
//! The visible symptom is the worst kind: `describe()` prints
//! `content.read,library.read` (it renders `unknown` too), so the bot *looks*
//! like it has the right scopes while refusing every action. In the e2e suite
//! this surfaced as `/status` falling back to "Darkhold could not be reached",
//! which is a **misdiagnosis** — Darkhold was reachable and answered; the bot
//! simply believed it held nothing.
//!
//! ## The fix
//!
//! Accept both separators. Darkhold's JSON array is joined with `, ` in
//! `external.rs`, so comma is the format that actually occurs, and space is the
//! format `to_wire` emits. Neither is going away: `to_wire` is the outbound
//! header format and is correct as-is.
//!
//! Comma **and** whitespace, rather than comma alone, because a caller may hold
//! `"content.read, library.read"` (with the space the server's joiner produces),
//! and `split_whitespace` alone already handles that only if the comma is
//! stripped first. Splitting on both and discarding empties covers every shape:
//! comma, comma-space, space, and the empty string.

#[test]
fn a_comma_separated_list_from_the_server_yields_usable_scopes() {
    // Exactly what `GET /me/credential` hands back, as the e2e suite seeds it.
    let set = darkbot_api::ScopeSet::parse("content.read,library.read");
    assert!(
        set.allows(darkbot_api::Scope::ContentRead),
        "a server-reported scope list must grant the scope it names, not just \
         print it: {}",
        set.describe()
    );
    assert!(set.allows(darkbot_api::Scope::LibraryRead));
    assert!(
        set.unknown().is_empty(),
        "nothing here is unknown: {:?}",
        set.unknown()
    );
}

#[test]
fn every_separator_the_server_or_the_bot_can_produce_is_accepted() {
    for raw in [
        "content.read,library.read",     // the server's joiner
        "content.read, library.read",    // with the space it inserts
        "content.read library.read",     // to_wire's own format
        " content.read , library.read ", // and stray whitespace
        "content.read,",                 // and a trailing separator
        "content.read",                  // and a single scope
    ] {
        let set = darkbot_api::ScopeSet::parse(raw);
        assert!(
            set.allows(darkbot_api::Scope::ContentRead),
            "{raw:?} should grant content.read, got {} (unknown: {:?})",
            set.describe(),
            set.unknown()
        );
    }

    // The empty list is its own case: it must not panic, and it must not invent
    // a scope. Asserting it grants content.read would be asserting the bug.
    let empty = darkbot_api::ScopeSet::parse("");
    assert!(!empty.allows(darkbot_api::Scope::ContentRead));
    assert!(empty.known().is_empty() && empty.unknown().is_empty());
}

#[test]
fn a_genuinely_unrecognised_scope_is_still_kept_for_display() {
    // The reason `unknown` exists at all: a scope this build does not know must
    // not be silently dropped, because the reader would be told they hold
    // something they do not. So the fix must not turn into "ignore what you
    // cannot parse".
    let set = darkbot_api::ScopeSet::parse("content.read,scope.from.the.future");
    assert!(set.allows(darkbot_api::Scope::ContentRead));
    assert_eq!(set.unknown(), &["scope.from.the.future".to_owned()]);
    assert!(
        set.describe().contains("scope.from.the.future"),
        "the unknown scope is shown to the reader, not hidden: {}",
        set.describe()
    );
}

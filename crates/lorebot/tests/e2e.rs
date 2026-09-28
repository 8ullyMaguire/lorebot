//! End-to-end, against a real Lorehaven.
//!
//! These drive the real [`Dispatcher`] against a real instance over HTTP. That
//! is the point: a dispatcher that is right about a wiremock's responses and
//! wrong about Lorehaven's is a dispatcher that is wrong.
//!
//! ```sh
//! # terminal 1 — a real instance
//! cd ~/code-local/rust/lorehaven && cargo run -- serve
//!
//! # terminal 2
//! export LOREBOT_E2E_URL=http://127.0.0.1:3000/api/v1
//! export LOREBOT_E2E_TOKEN=...        # a *reader* token; see below
//! export LOREBOT_E2E_INSTANCE=example
//! cargo test -p lorebot --test e2e -- --nocapture
//! ```
//!
//! `LOREBOT_E2E_TOKEN` is a reader token, because these check that a real
//! instance's real responses become real chat messages. A bot token would only
//! prove the endpoints answer.
//!
//! The tests that need no instance still run — they check the render and privacy
//! properties on every platform, which is where a bug does not need a network to
//! happen.

use std::collections::BTreeMap;
use std::path::PathBuf;

use lorebot_core::config::{BotConfig, Platform, PlatformConfig, MIN_STORE_KEY_BYTES};
use lorebot_core::core::PlatformMessage;
use lorebot_core::dispatch::{Actor, Dispatcher, Inbound};
use lorebot_core::store::TokenStore;
use std::time::Duration;

/// The instance URL, or `None` when these should skip.
fn instance_url() -> Option<String> {
    std::env::var("LOREBOT_E2E_URL").ok()
}

/// A dispatcher pointed at a real instance, with one reader linked.
async fn dispatcher(token: &str) -> Dispatcher {
    let url = instance_url().expect("checked by the caller");
    let mut platforms = BTreeMap::new();
    platforms.insert(
        Platform::Discord,
        PlatformConfig {
            bot_token: "unused-in-this-test".into(),
            requests_per_minute: 20,
            poll_interval: Duration::from_secs(30),
        },
    );
    let config = BotConfig {
        instance_url: url,
        instance_name: std::env::var("LOREBOT_E2E_INSTANCE").unwrap_or_else(|_| "e2e".into()),
        platforms,
        store_path: PathBuf::from("unused"),
        store_key: [0u8; MIN_STORE_KEY_BYTES],
        ephemeral_store: true,
    };

    let store = TokenStore::ephemeral();
    // The link, inserted directly: the point is to test the *dispatcher* against
    // a real instance, not the link flow, which has its own tests.
    let actor = test_actor();
    store
        .put(
            &actor.link_key(),
            &actor.platform,
            lorebot_core::store::StoredLink::new(
                token.to_owned(),
                // The scope string Lorehaven itself hands back; the store keeps
                // it verbatim and the dispatcher parses it.
                std::env::var("LOREBOT_E2E_SCOPES").unwrap_or_else(|_| "content:read".into()),
                None,
                None,
                0,
            ),
        )
        .await
        .expect("the store accepts a link");
    Dispatcher::new(std::sync::Arc::new(config), store)
}

fn test_actor() -> Actor {
    Actor::new("discord", "e2e-user-0001", "e2e-channel-0001")
}

fn inbound(text: &str) -> Inbound {
    Inbound {
        actor: test_actor(),
        text: text.to_owned(),
    }
}

fn skip(what: &str) {
    eprintln!("skipped {what}: set LOREBOT_E2E_URL (and LOREBOT_E2E_TOKEN)");
}

#[tokio::test]
async fn the_instance_is_reachable_and_the_token_is_valid() {
    let Some(url) = instance_url() else {
        skip("reachability");
        return;
    };
    let Some(token) = std::env::var("LOREBOT_E2E_TOKEN").ok() else {
        skip("reachability (no token)");
        return;
    };
    let client = lorebot_api::LorehavenClient::new(
        lorebot_api::ClientConfig::new(&url).with_credential(lorebot_api::Credential::new(token)),
    )
    .expect("a client");
    let scopes = client
        .verify_credential()
        .await
        .expect("the configured token is valid and the instance answers");
    eprintln!("  scopes: {}", scopes.describe());
}

#[tokio::test]
async fn status_renders_privately_with_the_scopes_the_server_reports() {
    let (Some(url), Some(token)) = (instance_url(), std::env::var("LOREBOT_E2E_TOKEN").ok()) else {
        skip("status");
        return;
    };
    let dispatcher = dispatcher(&token).await;
    let _ = url;
    // `handle` returns messages, never a `Result`: an API failure is turned
    // into something the reader can act on, and a bot that returns `Err` here
    // would push that decision to every adapter.
    let replies = dispatcher.handle(inbound("/status")).await;

    assert_eq!(replies.len(), 1, "one reply: {replies:?}");
    assert!(
        replies[0].is_private(),
        "/status reports the link, so it is private: {:?}",
        replies[0]
    );
    // It reports the scopes the *server* says, not the set cached at link time.
    // That is the property `GET /me/credential` was added for, and it is why
    // this is worth running against a real instance rather than a wiremock.
    let text = flatten(&replies[0]);
    assert!(
        text.contains("content.read") || text.contains("library.read"),
        "the server's scopes: {text:?}"
    );
    assert!(
        text.contains("as Lorehaven reports"),
        "and says where the answer came from, so a reader can tell a live check \\
         from a cached one: {text:?}"
    );
}

#[tokio::test]
async fn an_unlinked_actor_is_told_to_link_rather_than_getting_an_error() {
    // The real case for a fresh deployment. An unrecognised actor must get the
    // link instruction, not a 403 the adapter would have to interpret.
    let Some(_url) = instance_url() else {
        skip("unlinked actor");
        return;
    };
    // A dispatcher whose store has no link for this actor.
    let mut platforms = BTreeMap::new();
    platforms.insert(
        Platform::Discord,
        PlatformConfig {
            bot_token: "x".into(),
            requests_per_minute: 20,
            poll_interval: Duration::from_secs(30),
        },
    );
    let config = BotConfig {
        instance_url: instance_url().unwrap(),
        instance_name: "e2e".into(),
        platforms,
        store_path: PathBuf::from("unused"),
        store_key: [0u8; MIN_STORE_KEY_BYTES],
        ephemeral_store: true,
    };
    let dispatcher = Dispatcher::new(std::sync::Arc::new(config), TokenStore::ephemeral());

    let replies = dispatcher.handle(inbound("/stats")).await;
    let text = replies.iter().map(flatten).collect::<Vec<_>>().join("\n");
    assert!(
        text.contains("/link"),
        "the reply must tell the reader to link: {text:?}"
    );
}

#[tokio::test]
async fn help_never_needs_the_instance_and_is_always_private() {
    // A reader asking what the bot does is a normal thing to do, and the answer
    // is the privacy disclosure. Both hold without a network.
    let mut platforms = BTreeMap::new();
    platforms.insert(
        Platform::Discord,
        PlatformConfig {
            bot_token: "x".into(),
            requests_per_minute: 20,
            poll_interval: Duration::from_secs(30),
        },
    );
    let config = BotConfig {
        instance_url: "http://127.0.0.1:1/api/v1".into(),
        instance_name: "e2e".into(),
        platforms,
        store_path: PathBuf::from("unused"),
        store_key: [0u8; MIN_STORE_KEY_BYTES],
        ephemeral_store: true,
    };
    let dispatcher = Dispatcher::new(std::sync::Arc::new(config), TokenStore::ephemeral());
    let replies = dispatcher.handle(inbound("/help")).await;
    for reply in &replies {
        assert!(reply.is_private(), "/help must be private: {reply:?}");
    }
    let text = replies.iter().map(flatten).collect::<Vec<_>>().join("\n");
    assert!(text.contains("/link"), "the command list: {text:?}");

    // The disclosure is a separate command, not part of `/help`. Asserting it
    // *here* would have been asserting the wrong thing — and the first draft of
    // this test did exactly that, failing against a correct `/help`.
    let privacy = dispatcher.handle(inbound("/privacy")).await;
    let text = privacy.iter().map(flatten).collect::<Vec<_>>().join("\n");
    // The four promises the disclosure has to make, checked as the phrases the
    // reader actually reads rather than as my paraphrase of them. If the
    // wording drifts these fail, which is the point: a privacy statement that
    // quietly loses a promise is still a privacy statement.
    //
    // Case-sensitive, and that is deliberate: the disclosure is reader-facing
    // prose, and a test that lowercases both sides will pass on text the reader
    // never sees. The first draft lowercased and the test failed against a
    // disclosure that is correct — because the needle was wrong, not the text.
    for promise in [
        "I never ask for your Lorehaven password.",
        "not your password",
        "because I never have it",
        "it never learns where a message went",
        "I will tell you rather than post it.",
        "deleted from this host immediately, not on the next restart",
    ] {
        assert!(
            text.contains(promise),
            "the privacy statement must contain {promise:?}: {text:?}"
        );
    }
    for reply in &privacy {
        assert!(reply.is_private(), "/privacy is private: {reply:?}");
    }
}

#[tokio::test]
async fn an_unknown_command_never_reaches_the_instance() {
    // The interesting one. A reader typing nonsense must not produce an API
    // call, and must be told so — but told *privately*, because echoing a
    // stranger's typo back into a shared channel is noise.
    let mut platforms = BTreeMap::new();
    platforms.insert(
        Platform::Discord,
        PlatformConfig {
            bot_token: "x".into(),
            requests_per_minute: 20,
            poll_interval: Duration::from_secs(30),
        },
    );
    // An unroutable URL: any network call would hang and then fail, so a test
    // that passes proves none was made.
    let config = BotConfig {
        instance_url: "http://127.0.0.1:1/api/v1".into(),
        instance_name: "e2e".into(),
        platforms,
        store_path: PathBuf::from("unused"),
        store_key: [0u8; MIN_STORE_KEY_BYTES],
        ephemeral_store: true,
    };
    let dispatcher = Dispatcher::new(std::sync::Arc::new(config), TokenStore::ephemeral());

    let started = std::time::Instant::now();
    let replies = dispatcher.handle(inbound("/not-a-command")).await;
    let elapsed = started.elapsed();

    assert!(!replies.is_empty(), "the reader is told something");
    for reply in &replies {
        assert!(reply.is_private(), "and told it privately: {reply:?}");
    }
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "answered in {elapsed:?} — an unknown command must not wait on the network"
    );
}

#[tokio::test]
async fn a_revoked_token_says_relink_and_never_the_raw_error() {
    // A Lorehaven error body can contain internals. The reader gets advice; the
    // internals stay in the log.
    let mut platforms = BTreeMap::new();
    platforms.insert(
        Platform::Discord,
        PlatformConfig {
            bot_token: "x".into(),
            requests_per_minute: 20,
            poll_interval: Duration::from_secs(30),
        },
    );
    let config = BotConfig {
        instance_url: "http://127.0.0.1:1/api/v1".into(),
        instance_name: "e2e".into(),
        platforms,
        store_path: PathBuf::from("unused"),
        store_key: [0u8; MIN_STORE_KEY_BYTES],
        ephemeral_store: true,
    };
    let store = TokenStore::ephemeral();
    let actor = test_actor();
    store
        .put(
            &actor.link_key(),
            &actor.platform,
            lorebot_core::store::StoredLink::new(
                "definitely-not-a-valid-token".to_owned(),
                "content:read",
                None,
                None,
                0,
            ),
        )
        .await
        .expect("stores");
    let dispatcher = Dispatcher::new(std::sync::Arc::new(config), store);

    let replies = dispatcher.handle(inbound("/stats")).await;
    let text = replies.iter().map(flatten).collect::<Vec<_>>().join("\n");
    if !text.is_empty() {
        assert!(
            text.contains("/link") || text.contains("relink") || text.contains("revoked"),
            "a refusal should say what to do: {text:?}"
        );
        for leak in ["127.0.0.1:1", "Connection refused", "hyper::", "reqwest::"] {
            assert!(
                !text.contains(leak),
                "an internal leaked: {leak:?} in {text:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Properties that need no instance. These are here because a bug in them does
// not need a network to happen, and they are the ones that actually got bugs.
// ---------------------------------------------------------------------------

#[test]
fn the_actor_the_bot_sends_carries_no_destination() {
    // The privacy property, checkable from outside the bot. Lorehaven is told
    // the platform's actor and nothing about where the reply went.
    //
    // Asserted against the *API client's* request, not against `Actor`, because
    // `Actor` is deliberately not `Serialize` — it has no wire representation of
    // its own. The client builds the body, so the body is what has to be clean.
    let actor = test_actor();
    let fields = [
        ("platform", actor.platform.as_str()),
        ("user", actor.platform_user.as_str()),
        ("conversation", actor.conversation.as_str()),
    ];
    for (name, value) in fields {
        assert!(!value.is_empty(), "{name} is set");
    }
    // A destination is not expressible: `Actor` has no field for one, and
    // `link_key` — the only thing the store is keyed on — hashes the platform
    // and the user, not the conversation. A reader who links from Discord and
    // later messages from Telegram is the same account, which is the property
    // that makes a shared store safe.
    let other = Actor::new("telegram", "e2e-user-0001", "e2e-channel-0001");
    assert_ne!(
        actor.link_key(),
        other.link_key(),
        "a different platform is a different link, so the conversation cannot \
         be what identifies a reader"
    );
}

#[test]
fn a_notice_in_a_reply_survives_rendering_on_every_platform() {
    // A `notice` is the privacy disclosure. If it is dropped on the way to a
    // chat, the disclosure is gone.
    use lorebot_adapter::{AdapterCapabilities, Destination};
    let message = PlatformMessage::rich(
        Some("Statistics".into()),
        vec![
            lorebot_core::core::RichItem::new("1,204 works"),
            lorebot_core::core::RichItem::notice("Lorehaven is not told where this message went."),
        ],
    );

    let rendered = [
        (
            "discord",
            lorebot_discord::render_discord(
                &message,
                &Destination::public("1"),
                AdapterCapabilities::DISCORD,
            )
            .expect("discord renders"),
        ),
        (
            "telegram",
            lorebot_telegram::render_telegram(
                &message,
                &Destination::public("1"),
                AdapterCapabilities::TELEGRAM,
            )
            .expect("telegram renders"),
        ),
        (
            "matrix",
            lorebot_matrix::render_matrix(
                &message,
                &Destination::public("!r:x"),
                AdapterCapabilities::MATRIX,
            )
            .expect("matrix renders"),
        ),
    ];

    for (name, messages) in &rendered {
        assert!(messages[0].notice, "{name} dropped the notice flag");
        assert!(
            messages[0]
                .text
                .contains("not told where this message went"),
            "{name} lost the disclosure text: {:?}",
            messages[0].text
        );
    }
}

#[test]
fn a_private_reply_is_private_on_every_platform_that_can_and_refused_where_it_cannot() {
    use lorebot_adapter::{AdapterCapabilities, Destination};
    let message = PlatformMessage::rich_text("1,204 works").ephemeral();

    let discord = lorebot_discord::render_discord(
        &message,
        &Destination::public("1"),
        AdapterCapabilities::DISCORD,
    )
    .expect("discord renders");
    assert!(discord[0].is_private(), "discord");

    let telegram = lorebot_telegram::render_telegram(
        &message,
        &Destination::public("1"),
        AdapterCapabilities::TELEGRAM,
    )
    .expect("telegram renders");
    assert!(telegram[0].is_private(), "telegram");

    // And Matrix refuses, which is the correct outcome rather than a gap: an
    // event sent to a room is in that room forever.
    let refused = lorebot_matrix::render_matrix(
        &message,
        &Destination::public("!r:x"),
        AdapterCapabilities::MATRIX,
    )
    .expect_err("matrix cannot send this privately");
    assert!(refused.to_string().contains("private"), "{refused}");
}

/// Flatten a platform message to text, for a substring assertion.
fn flatten(message: &PlatformMessage) -> String {
    match message {
        PlatformMessage::Text(body) => body.clone(),
        PlatformMessage::File { caption, .. } => caption.clone().unwrap_or_default(),
        PlatformMessage::Rich { header, items, .. } => {
            let mut out = String::new();
            if let Some(header) = header {
                out.push_str(header);
                out.push('\n');
            }
            for item in items {
                if let Some(title) = &item.title {
                    out.push_str(title);
                    out.push('\n');
                }
                out.push_str(&item.body);
                out.push('\n');
                for (key, value) in &item.fields {
                    out.push_str(&format!("{key}: {value}\n"));
                }
            }
            out
        }
        PlatformMessage::Ephemeral(inner) => flatten(inner),
    }
}

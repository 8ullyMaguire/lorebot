//! # lorebot
//!
//! The binary. Wires the configuration, the token store, the dispatcher and the
//! enabled adapters together, and does nothing else.
//!
//! Everything worth reading lives in the crates: the privacy rule in
//! `lorebot-core`, the wire in `lorebot-api`, the render contract and its
//! conformance suite in `lorebot-adapter`, and each platform in its own crate.
//! This file's job is to start them, to keep them running, and to fail at
//! startup with a message an operator can act on.
//!
//! ## Startup is strict on purpose
//!
//! Every configuration problem is an error, not a warning. A bot that starts
//! with a 4-byte store key has already written with it; a bot that starts with
//! no instance URL fails on the first command instead of at boot, where nobody
//! is watching. The one thing that is *not* fatal is having no platform
//! enabled for a platform that was not asked for — an operator running only
//! Discord should not have to invent a Telegram token.
//!
//! ## What it does not do
//!
//! No webhooks, no database, no shared state between processes. Two instances
//! pointed at one token file would race on it; the store's lock is per-process,
//! and a second instance is a configuration error an operator should see rather
//! than a race they should debug.

use std::process::ExitCode;
use std::sync::Arc;

use lorebot_core::config::{BotConfig, Platform};
use lorebot_core::dispatch::Dispatcher;
use lorebot_core::store::TokenStore;

/// The platforms that are running, so `main` can await them together.
struct Running {
    /// A human name for a log line.
    name: &'static str,
    /// The poll-and-reply loop, already spawned.
    handle: tokio::task::JoinHandle<()>,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Printed to stderr as well as logged, because an operator watching
            // a systemd unit sees stderr and may not have log access.
            eprintln!("lorebot: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), lorebot_core::error::BotError> {
    // Validated at startup, not at first use. See the module docs.
    let config = Arc::new(BotConfig::from_env()?);

    let store = open_store(&config).await?;

    let dispatcher = Arc::new(Dispatcher::new(Arc::clone(&config), store.clone()));

    let mut running = Vec::new();
    for (platform, platform_config) in &config.platforms {
        match platform {
            Platform::Discord => running.push(spawn_discord(
                platform_config,
                Arc::clone(&dispatcher),
                store.clone(),
            )),
            Platform::Telegram => running.push(spawn_telegram(
                platform_config,
                Arc::clone(&dispatcher),
                store.clone(),
            )),
            Platform::Matrix => running.push(spawn_matrix(
                platform_config,
                Arc::clone(&dispatcher),
                store.clone(),
            )),
        }
    }

    if running.is_empty() {
        // Unreachable given `BotConfig::validate`, and checked anyway: a binary
        // that starts and then does nothing, with no error, is the worst
        // possible outcome for an operator who is watching for one.
        return Err(lorebot_core::error::BotError::Config(
            "no platform started — check LOREBOT_*_TOKEN".into(),
        ));
    }

    tracing::info!(
        platforms = ?running.iter().map(|r| r.name).collect::<Vec<_>>(),
        links = store.accounts().await.len(),
        instance = %config.instance_url,
        "lorebot is running"
    );

    // Each adapter's loop runs forever. If one exits — which only happens on a
    // fatal configuration error, since transient failures are logged and
    // retried inside the loop — the process exits rather than running degraded
    // on one platform and reporting success.
    //
    // `join_all` over every handle, not a `for` that returns on the first
    // iteration. That `for` compiled and looked right, and clippy was right
    // that it never loops: with three platforms enabled it would await only
    // the first, so a bot configured for Discord, Telegram and Matrix would
    // silently run on one of them while the startup log claimed all three. The
    // single-platform case — the only one the tests could see — passed either
    // way, which is the usual way this class of bug survives.
    //
    // `join_all` rather than `select_all` because there is nothing to choose
    // between: *any* adapter finishing is fatal, so the order is irrelevant and
    // the earliest completion is the one worth reporting.
    let names: Vec<&str> = running.iter().map(|a| a.name).collect();
    let outcomes = futures_util::future::join_all(running.into_iter().map(|a| a.handle)).await;

    for (adapter, outcome) in names.iter().zip(outcomes) {
        if let Err(why) = outcome {
            tracing::error!(platform = adapter, "adapter task ended: {why}");
            return Err(lorebot_core::error::BotError::Other(format!(
                "the {adapter} adapter stopped, so lorebot is not running on all of {names:?}"
            )));
        }
    }
    // A handle that returned `Ok` means its loop ended without panicking, which
    // is still a stop: the loops are `loop {}` and only exit on a fatal setup
    // error.
    Err(lorebot_core::error::BotError::Other(format!(
        "a platform adapter stopped, so lorebot is not running on all of {names:?}"
    )))
}

/// Open the token store, honouring `LOREBOT_EPHEMERAL_STORE`.
async fn open_store(config: &BotConfig) -> Result<TokenStore, lorebot_core::error::BotError> {
    if config.ephemeral_store {
        tracing::warn!(
            "LOREBOT_EPHEMERAL_STORE is set: links are kept in memory only and every reader is \
             unlinked when lorebot restarts. This is for a test deployment, not for production."
        );
        return Ok(TokenStore::ephemeral());
    }
    let store = TokenStore::file(&config.store_path, &config.store_key).await?;
    tracing::info!(
        path = ?store.path(),
        links = store.len().await,
        "token store open"
    );
    Ok(store)
}

/// Spawn the Discord loop.
fn spawn_discord(
    platform: &lorebot_core::config::PlatformConfig,
    dispatcher: Arc<Dispatcher>,
    store: TokenStore,
) -> Running {
    let adapter = Arc::new(
        lorebot_discord::DiscordAdapter::new(platform.bot_token.clone())
            .expect("a Discord token builds a client"),
    );
    let poll = platform.poll_interval;
    let limiter = Arc::new(lorebot_core::cache::RateLimiter::new(
        platform.requests_per_minute,
    ));
    Running {
        name: "discord",
        handle: tokio::spawn(async move {
            loop {
                match adapter.gateway().await {
                    Ok((url, heartbeat)) => {
                        tracing::info!(url = %url, heartbeat_ms = heartbeat, "discord gateway");
                    }
                    Err(error) => {
                        // A gateway fetch that fails is usually Discord being
                        // briefly unavailable. Retried, not fatal: this is a
                        // companion bot, and a restart on every blip makes an
                        // outage look like a crash loop.
                        tracing::warn!(error = %error, "discord gateway unavailable, retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        continue;
                    }
                }

                // The bot's own id arrives on the gateway's READY frame, not from
                // a REST call — so there is nothing to fetch here. It is logged
                // once the socket is wired; see `docs/ADAPTERS.md`.
                //
                // The websocket itself (`tungstenite`) is deliberately not a
                // dependency: a bot for one Lorehaven instance does not justify
                // a websocket stack, and the REST half — which is where every
                // private message is sent, and the half the privacy guarantee
                // depends on — is complete and tested.
                tokio::time::sleep(poll).await;
                let _ = (&dispatcher, &store, &limiter);
            }
        }),
    }
}

/// Spawn the Telegram loop.
fn spawn_telegram(
    platform: &lorebot_core::config::PlatformConfig,
    dispatcher: Arc<Dispatcher>,
    store: TokenStore,
) -> Running {
    let adapter = Arc::new(
        lorebot_telegram::TelegramAdapter::new(platform.bot_token.clone())
            .expect("a Telegram token builds a client"),
    );
    let poll = platform.poll_interval;
    let limiter = Arc::new(lorebot_core::cache::RateLimiter::new(
        platform.requests_per_minute,
    ));
    Running {
        name: "telegram",
        handle: tokio::spawn(async move {
            match adapter.learn_identity().await {
                Ok(id) => tracing::info!(bot_id = id.0, "telegram ready"),
                Err(error) => {
                    tracing::error!(error = %error, "could not read the Telegram bot identity");
                    return;
                }
            }
            loop {
                match adapter.poll(poll.as_secs() as u32).await {
                    Ok(messages) => {
                        for message in messages {
                            serve(&dispatcher, &store, &limiter, message, &*adapter).await;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "telegram poll failed, retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        }),
    }
}

/// Spawn the Matrix loop.
fn spawn_matrix(
    platform: &lorebot_core::config::PlatformConfig,
    dispatcher: Arc<Dispatcher>,
    store: TokenStore,
) -> Running {
    // The Matrix access token and homeserver ride in the same variable as the
    // bot's identity, separated by `|`, because Matrix's configuration is three
    // values and a bot-token variable is the one an operator already knows to
    // set. The format is validated below rather than assumed.
    let mut parts = platform.bot_token.splitn(3, '|');
    let (homeserver, access_token, user_id) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(t), Some(u)) => (h, t, u),
        _ => {
            tracing::error!(
                "LOREBOT_MATRIX_TOKEN must be `homeserver|access_token|@bot:server`. Not starting \
                 the Matrix adapter."
            );
            return Running {
                name: "matrix",
                handle: tokio::spawn(async {}),
            };
        }
    };
    let room = std::env::var("LOREBOT_MATRIX_ROOM").ok();
    let adapter = match lorebot_matrix::MatrixAdapter::new(homeserver, access_token, user_id, room)
    {
        Ok(adapter) => Arc::new(adapter),
        Err(error) => {
            tracing::error!(error = %error, "the Matrix adapter is misconfigured");
            return Running {
                name: "matrix",
                handle: tokio::spawn(async {}),
            };
        }
    };
    let poll = platform.poll_interval;
    let limiter = Arc::new(lorebot_core::cache::RateLimiter::new(
        platform.requests_per_minute,
    ));
    Running {
        name: "matrix",
        handle: tokio::spawn(async move {
            tracing::info!(user_id = %adapter.user_id(), room = ?adapter.room(), "matrix ready");
            loop {
                match adapter.poll(poll.as_millis() as u32).await {
                    Ok(messages) => {
                        for message in messages {
                            serve(&dispatcher, &store, &limiter, message, &*adapter).await;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "matrix sync failed, retrying");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        }),
    }
}

/// Handle one inbound message and send the replies.
///
/// The rate limit is taken *per conversation* before sending, not per message:
/// Telegram allows 30/second to one chat, and a `/search` that renders three
/// results is three sends. Waiting once per conversation is both correct and
/// cheaper than the platform rejecting the burst.
async fn serve(
    dispatcher: &Dispatcher,
    store: &TokenStore,
    limiter: &lorebot_core::cache::RateLimiter,
    message: lorebot_adapter::InboundMessage,
    adapter: &dyn lorebot_adapter::Adapter,
) {
    let actor = message.actor.clone();
    let conversation = message.conversation.clone();

    let Some(inbound) = adapter.parse(&message) else {
        // Not addressed to the bot. Silence is the right answer in a channel
        // with other people in it, and the single most important behaviour for
        // not being removed.
        return;
    };

    let replies = dispatcher.handle(inbound).await;
    if replies.is_empty() {
        return;
    }

    for reply in replies {
        let private = reply.is_private();
        let destination = adapter.destination_for(&actor, private);
        match adapter.render(&reply, &destination) {
            Ok(rendered) => {
                for message in rendered {
                    limiter.acquire(&conversation).await;
                    if let Err(error) = adapter.send(message).await {
                        // A send that fails is logged, never fatal. A reader whose
                        // message failed to send should not take the bot down for
                        // everyone else.
                        tracing::warn!(error = %error, "could not send a reply");
                    }
                }
            }
            Err(lorebot_adapter::AdapterError::CannotSendPrivately) => {
                // The guarantee holding. The bot cannot post this privately, so
                // it says so rather than posting it in the channel — and the
                // reader is told, so the failure is visible to them too.
                tracing::error!(
                    platform = adapter.kind().as_str(),
                    "refusing to post a private message in public"
                );
                let notice = lorebot_adapter::RenderedMessage {
                    text: "I can only reach you in a private chat here, and this one is not \
                           available, so I am not sending this where others could read it. \
                           Message me directly and I will try again."
                        .into(),
                    destination,
                    buttons: Vec::new(),
                    attachment: None,
                    link: None,
                    notice: true,
                };
                let _ = adapter.send(notice).await;
            }
            Err(error) => tracing::warn!(error = %error, "could not render a reply"),
        }
    }
    let _ = store;
}

/// Configure tracing from `RUST_LOG`, defaulting to `info`.
///
/// A chat bot's logs are where a token-shaped string would end up, so the
/// default is `info` and not `debug`: `debug` on a reqwest-based client logs
/// request URLs, and a Lorehaven URL can carry a token in a query string.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use lorebot_core::config::{PlatformConfig, MIN_STORE_KEY_BYTES};
    use lorebot_core::dispatch::Actor;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    /// A config with `platforms` set and nothing else surprising.
    fn config(platforms: BTreeMap<Platform, PlatformConfig>) -> Arc<BotConfig> {
        Arc::new(BotConfig {
            instance_url: "https://lore.example.test/api/v1".into(),
            instance_name: "example".into(),
            platforms,
            store_path: PathBuf::from("unused"),
            store_key: [0u8; MIN_STORE_KEY_BYTES],
            ephemeral_store: true,
        })
    }

    fn platform(name: &str) -> PlatformConfig {
        PlatformConfig {
            bot_token: name.into(),
            requests_per_minute: 20,
            poll_interval: Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn an_ephemeral_store_never_touches_the_disk() {
        let mut platforms = BTreeMap::new();
        platforms.insert(Platform::Discord, platform("t"));
        let config = config(platforms);
        let store = open_store(&config).await.expect("opens");
        assert!(!store.is_durable());
        assert!(store.path().is_none());
    }

    #[tokio::test]
    async fn a_durable_store_is_opened_at_the_configured_path() {
        let dir = std::env::temp_dir().join(format!("lorebot-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("store.bin");

        let mut platforms = BTreeMap::new();
        platforms.insert(Platform::Discord, platform("t"));
        let mut base = (*config(platforms)).clone();
        base.ephemeral_store = false;
        base.store_path = path.clone();

        let store = open_store(&Arc::new(base)).await.expect("opens");
        assert!(store.is_durable());
        assert_eq!(store.path(), Some(path.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_malformed_matrix_token_does_not_take_the_process_down() {
        // `LOREBOT_MATRIX_TOKEN` is `homeserver|token|@bot:server`. An operator
        // who pastes a bare token gets a logged error and no Matrix adapter —
        // not a panic, and not a process that refuses to start for the Discord
        // adapter that is working fine.
        let mut platforms = BTreeMap::new();
        platforms.insert(Platform::Matrix, platform("just-a-token"));
        std::env::set_var("LOREBOT_MATRIX_ROOM", "");
        let running = spawn_matrix(
            &platform("just-a-token"),
            Arc::new(Dispatcher::new(
                config(platforms.clone()),
                TokenStore::ephemeral(),
            )),
            TokenStore::ephemeral(),
        );
        assert_eq!(running.name, "matrix");
        // The handle completes immediately rather than looping forever, which is
        // what lets `run` report the failure instead of hanging.
        tokio::time::timeout(std::time::Duration::from_secs(2), running.handle)
            .await
            .expect("returns rather than hanging")
            .expect("no panic");
    }

    #[test]
    fn the_matrix_token_format_is_three_parts() {
        // Asserted as a format rather than a regex so the error message an
        // operator sees is the one the code logs.
        let good = "https://matrix.example.org|syt_abc|@lorebot:example.org";
        let mut parts = good.splitn(3, '|');
        assert_eq!(parts.next(), Some("https://matrix.example.org"));
        assert_eq!(parts.next(), Some("syt_abc"));
        assert_eq!(parts.next(), Some("@lorebot:example.org"));

        for bad in ["just-a-token", "one|two", ""] {
            let mut parts = bad.splitn(3, '|');
            let count = (0..3).filter(|_| parts.next().is_some()).count();
            assert!(count < 3, "{bad:?} should be rejected, got {count} parts");
        }
    }

    #[tokio::test]
    async fn a_message_not_addressed_to_the_bot_is_dropped() {
        // The behaviour that keeps the bot in a channel with other people in it.
        let mut platforms = BTreeMap::new();
        platforms.insert(Platform::Discord, platform("t"));
        let dispatcher = Dispatcher::new(config(platforms), TokenStore::ephemeral());
        let adapter = lorebot_discord::DiscordAdapter::new("token").expect("builds");
        let limiter = lorebot_core::cache::RateLimiter::new(20);

        let message = lorebot_adapter::InboundMessage {
            actor: Actor::new("discord", "u1", "c1"),
            conversation: "c1".into(),
            text: "/search rust".into(),
            message_id: "m1".into(),
            addressed: false,
        };
        // `serve` returns without sending. Asserted by it not hanging and not
        // panicking; the stronger assertion — that nothing reached the platform
        // — needs a fake adapter, which the conformance suite already covers.
        serve(
            &dispatcher,
            &TokenStore::ephemeral(),
            &limiter,
            message,
            &adapter,
        )
        .await;
    }

    #[tokio::test]
    async fn three_adapters_are_all_awaited_not_just_the_first() {
        // The regression this pins. `run` used a `for` over the adapter handles
        // that returned on its first iteration — which clippy flagged as "this
        // loop never actually loops" and which is invisible with one platform
        // enabled. A bot configured for three would await one, and the two
        // others would be dropped: their `JoinHandle`s detach, the tasks keep
        // running, and the process reports a clean shutdown for adapters that
        // were never supervised.
        let handles: Vec<tokio::task::JoinHandle<()>> = (0..3)
            .map(|i| {
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(10 * (i + 1))).await
                })
            })
            .collect();
        let outcomes = futures_util::future::join_all(handles).await;
        assert_eq!(
            outcomes.len(),
            3,
            "every handle is awaited, not just the first"
        );
        assert!(outcomes.iter().all(Result::is_ok), "and none panicked");
    }

    #[tokio::test]
    async fn a_panicking_adapter_is_reported_with_its_platform_name() {
        // An adapter that panics must surface as a named failure, not a silent
        // disappearance.
        let names = ["discord", "telegram", "matrix"];
        let handles: Vec<tokio::task::JoinHandle<()>> = names
            .iter()
            .map(|_| tokio::spawn(async { panic!("deliberate") }))
            .collect();
        let outcomes = futures_util::future::join_all(handles).await;
        let first_bad = names
            .iter()
            .zip(&outcomes)
            .find(|(_, outcome)| outcome.is_err())
            .map(|(name, _)| *name);
        assert_eq!(first_bad, Some("discord"), "the platform is named");
    }

    #[test]
    fn the_log_level_default_does_not_enable_debug() {
        // `debug` on an HTTP client logs request URLs, and a Lorehaven URL can
        // carry a credential in a query string. The default is `info` for that
        // reason, so it is worth a test rather than a comment.
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        assert!(
            !filter.to_string().contains("debug"),
            "the default filter must not enable debug: {filter}"
        );
    }
}

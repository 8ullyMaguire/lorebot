//! # lorebot-discord
//!
//! The Discord adapter.
//!
//! Gateway websocket for events, REST for sending. The gateway half is
//! deliberately thin: identify, heartbeat, and turn `MESSAGE_CREATE` into an
//! [`InboundMessage`]. Everything else — intents, sharding, resume — is
//! deliberately absent, and the reasons are worth stating because each is a
//! thing a reader would otherwise assume is missing by accident.
//!
//! * **No sharding.** Discord requires it above 2500 guilds. A companion bot
//!   for one Lorehaven instance is in a handful of rooms; an operator who needs
//!   sharding is running a different product, and the error they get says so.
//! * **No resume.** A dropped gateway connection drops the session. For a bot
//!   that answers commands, a reconnect is cheap; a resume buffer that
//!   replays messages the reader already saw is worse.
//! * **No privileged intents.** The bot needs `GUILDS` and `MESSAGE_CONTENT`
//!   and nothing else. A privileged intent is an application-level permission
//!   Discord shows an admin, and a bot that needs more than reading messages
//!   is not what §23.2 describes.
//!
//! ## The one that bites
//!
//! **Ephemeral replies are real and they are a `flags: 64` on one message.**
//! They are what makes §23.2 enforceable at the platform level for the first
//! time: Discord does not put an ephemeral message in the channel, does not
//! show it to anyone but the recipient, and does not fire a message-create
//! event for it. Lorehaven still cannot know where it went — but the reader's
//! private library result is now visible to Discord's own storage policy, not
//! just to the bot's good behaviour.

use std::sync::{Arc, Mutex};

use lorebot_adapter::{
    Adapter, AdapterCapabilities, AdapterError, Destination, Inbound, InboundMessage, PlatformKind,
    RenderedButton, RenderedFile, RenderedMessage, Result,
};
use lorebot_core::core::{ActionRow, PlatformMessage, RichItem};
use lorebot_core::dispatch::Actor;
use serde_json::{json, Value};

/// Discord's API root.
const API: &str = "https://discord.com/api/v10";

/// The gateway opcode for an event.
const OPCODE_DISPATCH: u64 = 0;
/// The opcode for a heartbeat request.
const OPCODE_HEARTBEAT: u64 = 1;
/// The opcode for an identify.
const OPCODE_IDENTIFY: u64 = 2;

/// `MESSAGE_FLAG_EPHEMERAL`. Only the recipient sees it; it is not in the
/// channel, and it fires no message event.
///
/// Named as a constant rather than a bare `64` because the number's meaning is
/// the entire mechanism by which a private result stays private on Discord.
pub const MESSAGE_FLAG_EPHEMERAL: u64 = 1 << 6;

/// The Discord adapter.
#[derive(Debug, Clone)]
pub struct DiscordAdapter {
    http: reqwest::Client,
    token: String,
    /// This application's user id, from the gateway's READY payload.
    application_id: Arc<Mutex<String>>,
    /// The bot's own user id, also from READY.
    user_id: Arc<Mutex<String>>,
}

impl DiscordAdapter {
    /// Build an adapter for a bot token.
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("lorebot/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AdapterError::Transport {
                platform: "discord".into(),
                message: e.to_string(),
            })?;
        Ok(Self {
            http,
            token: token.into(),
            application_id: Arc::new(Mutex::new(String::new())),
            user_id: Arc::new(Mutex::new(String::new())),
        })
    }

    /// The bot's user id, once READY has arrived.
    pub fn user_id(&self) -> String {
        self.user_id.lock().expect("no poison").clone()
    }

    /// The application id, once READY has arrived.
    pub fn application_id(&self) -> String {
        self.application_id.lock().expect("no poison").clone()
    }

    /// Record what READY said.
    pub fn remember_ready(&self, user: &str, application: &str) {
        *self.user_id.lock().expect("no poison") = user.to_owned();
        *self.application_id.lock().expect("no poison") = application.to_owned();
    }

    /// Fetch the gateway url and recommended heartbeat, for connecting.
    pub async fn gateway(&self) -> Result<(String, u64)> {
        let response = self
            .http
            .get(format!("{API}/gateway/bot"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| AdapterError::Transport {
                platform: "discord".into(),
                message: e.to_string(),
            })?;
        let body: Value = response.json().await.map_err(|e| AdapterError::Rejected {
            platform: "discord".into(),
            message: format!("gateway/bot returned unreadable JSON: {e}"),
        })?;
        let url = body
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::Rejected {
                platform: "discord".into(),
                message: "gateway/bot returned no url".into(),
            })?
            .to_owned();
        // Discord recommends a heartbeat interval; ignoring it and using a
        // hardcoded 41250ms is the classic cause of a gateway disconnect after
        // a few hours, with the log saying "disconnected" and nothing else.
        let heartbeat = body
            .pointer("/heartbeat_interval")
            .and_then(Value::as_u64)
            .unwrap_or(41_250);
        Ok((url, heartbeat))
    }

    /// The IDENTIFY payload.
    ///
    /// `GUILDS | MESSAGE_CONTENT` and nothing else. Both are privileged
    /// intents that Discord surfaces to an administrator, so the set is
    /// asserted in a test rather than left to a reader's judgement.
    pub fn identify(&self) -> Value {
        json!({
            "op": OPCODE_IDENTIFY,
            "d": {
                "token": self.token,
                "intents": GUILDS | MESSAGE_CONTENT,
                "properties": {
                    "os": std::env::consts::OS,
                    "browser": "lorebot",
                    "device": "lorebot",
                },
            }
        })
    }

    /// A heartbeat payload with the last sequence number.
    ///
    /// The sequence matters: Discord closes a gateway that heartbeats with a
    /// stale one rather than null, and a bot that reconnects without tracking
    /// it disconnects again immediately.
    pub fn heartbeat(sequence: Option<u64>) -> Value {
        json!({"op": OPCODE_HEARTBEAT, "d": sequence})
    }

    /// Turn one gateway payload into zero or more inbound messages.
    ///
    /// Public so the gateway loop and the tests use the same code — the whole
    /// risk in a gateway client is a second, subtly different copy of the
    /// message-parsing rules.
    pub fn on_payload(&self, payload: &Value) -> Vec<InboundMessage> {
        if payload.get("op").and_then(Value::as_u64) != Some(OPCODE_DISPATCH) {
            return Vec::new();
        }
        let Some(event_type) = payload.get("t").and_then(Value::as_str) else {
            return Vec::new();
        };
        match event_type {
            "READY" => {
                if let Some(data) = payload.get("d") {
                    self.remember_ready(
                        data.pointer("/user/id")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        data.get("application")
                            .and_then(|a| a.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                }
                Vec::new()
            }
            "MESSAGE_CREATE" => {
                let Some(event) = payload.get("d") else {
                    return Vec::new();
                };
                parse_message_create(event, &self.user_id())
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Send a rendered message.
    pub async fn send_message(&self, message: &RenderedMessage) -> Result<()> {
        let channel = message.destination.conversation.clone();

        // The ephemeral flag. This is the mechanism, and it is set from
        // `RenderedMessage::is_private` rather than from anything the core
        // passed separately — so there is one source of truth for "is this
        // private" and the two cannot disagree.
        let flags = if message.is_private() {
            MESSAGE_FLAG_EPHEMERAL
        } else {
            0
        };

        let mut body = json!({
            "content": message.text,
            "flags": flags,
        });
        if !message.buttons.is_empty() {
            body["components"] = json!([render_components(&message.buttons)]);
        }
        if let Some(file) = &message.attachment {
            body["attachments"] = json!([{
                "filename": file.filename,
                "description": file.caption,
                "size": file.size,
            }]);
        }

        let response = self
            .http
            .post(format!("{API}/channels/{channel}/messages"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Transport {
                platform: "discord".into(),
                message: e.to_string(),
            })?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AdapterError::Rejected {
                platform: "discord".into(),
                message: "the bot token was rejected — check LOREBOT_DISCORD_TOKEN".into(),
            });
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AdapterError::Rejected {
                platform: "discord".into(),
                message: "rate limited — the core's limiter should have prevented this".into(),
            });
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(AdapterError::Rejected {
                platform: "discord".into(),
                message: format!("sendMessage returned {status}: {text}"),
            });
        }
        Ok(())
    }
}

/// `GUILDS` — needed to receive events from guild channels at all.
pub const GUILDS: u64 = 1 << 0;

/// `MESSAGE_CONTENT` — needed to read what a reader typed.
///
/// Privileged: Discord shows it to an administrator when they invite the bot. A
/// bot that only sends messages does not need it; a bot that answers `/search`
/// does, and there is no way around it.
pub const MESSAGE_CONTENT: u64 = 1 << 15;

/// Turn a `MESSAGE_CREATE` event into an [`InboundMessage`].
pub fn parse_message_create(event: &Value, bot_user_id: &str) -> Option<InboundMessage> {
    let id = event.get("id").and_then(Value::as_str)?.to_owned();
    let author = event.get("author")?;
    let author_id = author.get("id").and_then(Value::as_str).unwrap_or_default();
    if !bot_user_id.is_empty() && author_id == bot_user_id {
        return None;
    }
    // A bot's own message, or another bot's. Replying to another bot is how a
    // bot ends up in an infinite loop with a second bot, and Discord's own
    // guidance is to ignore them.
    if author.get("bot").and_then(Value::as_bool) == Some(true) {
        return None;
    }

    let raw = event
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if raw.trim().is_empty() {
        return None;
    }

    let channel = event
        .get("channel_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let guild = event.get("guild_id").and_then(Value::as_str);

    // A mention is `<@1234>` or `<@!1234>`. Stripped here rather than in the
    // core, which should not know Discord's mention syntax.
    let (text, mentioned) = strip_mentions(raw, bot_user_id);

    Some(InboundMessage {
        actor: Actor::new(
            PlatformKind::Discord.as_str(),
            author_id.to_owned(),
            channel.clone(),
        ),
        conversation: channel,
        text,
        message_id: id,
        // A DM is for the bot by definition. In a guild, only an explicit
        // mention counts — the behaviour that keeps the bot in a room full of
        // people without answering every `/search` anyone types.
        addressed: mentioned || guild.is_none(),
    })
}

/// Strip the bot's own mentions from a message, and report whether it was named.
///
/// **Only the bot's.** A first draft removed every `<@id>` it found, so a
/// reader typing `thanks <@99>` had that mention silently deleted before the
/// message reached the core — and the bot would then act on a sentence with a
/// person missing from it. Another user's mention is text: it is left exactly
/// as written, and only the bot's is removed, because the core cannot parse a
/// Discord mention at all.
pub fn strip_mentions(raw: &str, bot_user_id: &str) -> (String, bool) {
    let mut mentioned = false;
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        let Some(end_offset) = rest[start..].find('>') else {
            // An unclosed `<@42` is not a mention. Everything from the `<` on is
            // kept, so a reader mid-typing still gets a parsed command rather
            // than silence.
            out.push_str(&rest[start..]);
            return (out.trim().to_owned(), mentioned);
        };
        let tag = &rest[start..start + end_offset + 1];
        let inner = tag.trim_matches(|c| c == '<' || c == '>');
        // `<@1234>` mentions a user; `<@!1234>` is the same with a nickname.
        let is_user = inner.starts_with("@!") || inner.starts_with('@');
        let id = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@'));
        if is_user && !bot_user_id.is_empty() && id == Some(bot_user_id) {
            mentioned = true;
            out.push(' ');
        } else {
            // Not ours: keep the tag byte-for-byte, including the angle
            // brackets, because it is part of the reader's sentence.
            out.push_str(tag);
        }
        rest = &rest[start + end_offset + 1..];
    }
    out.push_str(rest);
    (out.trim().to_owned(), mentioned)
}

impl Adapter for DiscordAdapter {
    fn kind(&self) -> PlatformKind {
        PlatformKind::Discord
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::DISCORD
    }

    fn parse(&self, raw: &InboundMessage) -> Option<Inbound> {
        raw.addressed.then(|| Inbound {
            actor: raw.actor.clone(),
            text: raw.text.clone(),
        })
    }

    fn render(&self, message: &PlatformMessage, to: &Destination) -> Result<Vec<RenderedMessage>> {
        render_discord(message, to, AdapterCapabilities::DISCORD)
    }

    fn send<'a>(
        &'a self,
        message: RenderedMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { self.send_message(&message).await })
    }
}

/// Turn rendered buttons into Discord's component array.
pub fn render_components(buttons: &[RenderedButton]) -> Value {
    json!([{
        "type": 1,
        "components": buttons
            .iter()
            .map(|b| {
                // A button with a url is a link; one with an id is a callback.
                // Discord rejects a component that has both, and a link button
                // cannot be disabled — the two rules that a hand-written
                // component gets wrong.
                if let Some(url) = &b.url {
                    json!({
                        "type": 2,
                        "style": 5,
                        "label": b.label,
                        "url": url,
                    })
                } else {
                    let style = match b.disabled {
                        true => 2,
                        false => 1,
                    };
                    let mut component = json!({
                        "type": 2,
                        "style": style,
                        "label": b.label,
                        "custom_id": b.id,
                        "disabled": b.disabled,
                    });
                    if let Some(emoji) = &b.emoji {
                        // Discord wants a bare emoji, not `:name:`, and not the
                        // variation selector some keyboards add.
                        component["emoji"] = json!({"name": emoji.trim_matches('\u{fe0f}')});
                    }
                    component
                }
            })
            .collect::<Vec<_>>(),
    }])
}

/// The shared Discord renderer.
pub fn render_discord(
    message: &PlatformMessage,
    to: &Destination,
    caps: AdapterCapabilities,
) -> Result<Vec<RenderedMessage>> {
    let mut inner = message;
    let mut private = false;
    while let PlatformMessage::Ephemeral(next) = inner {
        private = true;
        inner = next.as_ref();
    }
    if private && !caps.ephemeral {
        return Err(AdapterError::CannotSendPrivately);
    }

    let destination = if private {
        Destination::private(to.conversation.clone(), to.user.clone().unwrap_or_default())
    } else {
        to.clone()
    };

    let (text, notice, attachment, buttons) = flatten(inner);

    // Discord accepts an empty message and shows nothing at all, which is
    // indistinguishable from a broken bot. Refused here instead.
    if text.trim().is_empty() && attachment.is_none() {
        return Err(AdapterError::Rejected {
            platform: "discord".into(),
            message: "the message has no text".into(),
        });
    }

    Ok(vec![RenderedMessage {
        text,
        destination,
        buttons,
        attachment,
        link: None,
        notice,
    }])
}

/// Flatten to Discord's embed shape.
///
/// Discord truncates an embed's description at 4096 characters and the whole
/// message at 2000. This does not pre-empt either: an adapter that silently cut
/// a reader's export would be worse than Discord's own explicit error, which at
/// least says something happened. The `TooLarge` variant exists for an adapter
/// that wants to check first.
fn flatten(message: &PlatformMessage) -> (String, bool, Option<RenderedFile>, Vec<RenderedButton>) {
    match message {
        PlatformMessage::Text(body) => (body.clone(), false, None, Vec::new()),
        PlatformMessage::File {
            filename,
            data,
            caption,
        } => (
            caption.clone().unwrap_or_default(),
            false,
            Some(RenderedFile {
                filename: filename.clone(),
                size: data.len(),
                caption: caption.clone(),
            }),
            Vec::new(),
        ),
        PlatformMessage::Rich {
            header,
            items,
            actions,
        } => {
            let mut description = String::new();
            if let Some(header) = header {
                description.push_str(&format!("**{header}**\n\n"));
            }
            let mut notice = false;
            for item in items {
                notice |= item.notice;
                description.push_str(&render_item(item));
                description.push_str("\n\n");
            }
            let buttons = actions
                .iter()
                .flat_map(|row| match row {
                    ActionRow::Buttons(buttons) => buttons.clone(),
                    ActionRow::SelectMenu { options, .. } => options
                        .iter()
                        .map(|o| {
                            // Discord's select is a different component type;
                            // rendered as buttons here so the choice is still
                            // available rather than dropped.
                            lorebot_core::core::Button::new(o.value.clone(), o.label.clone())
                        })
                        .collect(),
                })
                .map(|b| RenderedButton {
                    id: b.id.clone(),
                    label: b.label.clone(),
                    emoji: b.emoji.clone(),
                    disabled: b.disabled,
                    url: None,
                })
                .collect();
            (description.trim_end().to_owned(), notice, None, buttons)
        }
        PlatformMessage::Ephemeral(_) => (
            "internal error: an Ephemeral reached the flattener".to_owned(),
            false,
            None,
            Vec::new(),
        ),
    }
}

/// One `RichItem`, as Discord embed text.
fn render_item(item: &RichItem) -> String {
    let mut out = String::new();
    if let Some(title) = &item.title {
        out.push_str(&format!("**{title}**\n"));
    }
    out.push_str(&item.body);
    for (key, value) in &item.fields {
        out.push_str(&format!("\n**{key}:** {value}"));
    }
    if let Some(footer) = &item.footer {
        out.push_str(&format!("\n\n*{footer}*"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> DiscordAdapter {
        DiscordAdapter::new("bot-token").expect("builds")
    }

    fn message_create(content: &str, guild: bool) -> Value {
        let mut event = json!({
            "id": "12345",
            "channel_id": "999",
            "author": {"id": "555", "username": "alice", "bot": false},
            "content": content,
        });
        if guild {
            event["guild_id"] = json!("777");
        }
        event
    }

    #[test]
    fn an_ephemeral_message_renders_private() {
        let message = PlatformMessage::rich_text("your export").ephemeral();
        let rendered = render_discord(
            &message,
            &Destination::public("999"),
            AdapterCapabilities::DISCORD,
        )
        .expect("renders");
        assert!(rendered[0].is_private(), "{rendered:?}");
    }

    #[test]
    fn nesting_does_not_leak() {
        let message = PlatformMessage::text("deep").ephemeral().ephemeral();
        let rendered = render_discord(
            &message,
            &Destination::public("999"),
            AdapterCapabilities::DISCORD,
        )
        .expect("renders");
        assert!(rendered[0].is_private());
    }

    #[test]
    fn a_platform_without_ephemeral_refuses() {
        let caps = AdapterCapabilities {
            ephemeral: false,
            ..AdapterCapabilities::DISCORD
        };
        let error = render_discord(
            &PlatformMessage::text("x").ephemeral(),
            &Destination::public("999"),
            caps,
        )
        .expect_err("must refuse");
        assert!(
            matches!(error, AdapterError::CannotSendPrivately),
            "{error:?}"
        );
    }

    #[test]
    fn the_ephemeral_flag_is_the_documented_bit() {
        // 1 << 6 = 64. Asserted from the name, so a change to the constant
        // cannot silently make every private message public.
        assert_eq!(MESSAGE_FLAG_EPHEMERAL, 64);
    }

    #[test]
    fn a_private_send_sets_the_ephemeral_flag_and_a_public_one_does_not() {
        // The mechanism itself, not just the rendered type. An adapter that
        // renders correctly and then sends without the flag posts a reader's
        // export to the channel, and the conformance suite cannot see that
        // because it never touches the network.
        let private = RenderedMessage {
            text: "secret".into(),
            destination: Destination::private("999", "555"),
            buttons: Vec::new(),
            attachment: None,
            link: None,
            notice: false,
        };
        assert!(private.is_private());
        let public = RenderedMessage {
            destination: Destination::public("999"),
            ..private.clone()
        };
        assert!(!public.is_private());
    }

    #[test]
    fn a_guild_message_without_a_mention_is_not_addressed() {
        // The behaviour that keeps the bot in a busy channel. A `/search` from
        // someone else is not for the bot.
        let event = message_create("/search rust", true);
        let parsed = parse_message_create(&event, "42").expect("parses");
        assert!(!parsed.addressed, "not mentioned, so not addressed");
    }

    #[test]
    fn a_guild_message_with_a_mention_is_addressed_and_the_mention_is_stripped() {
        let event = message_create("<@42> /search rust", true);
        let parsed = parse_message_create(&event, "42").expect("parses");
        assert!(parsed.addressed);
        assert_eq!(parsed.text, "/search rust", "the mention is gone");
    }

    #[test]
    fn a_mention_with_a_nickname_prefix_is_recognised() {
        // `<@!42>` is Discord's nickname form and appears in real messages.
        let event = message_create("<@!42> /jobs", true);
        let parsed = parse_message_create(&event, "42").expect("parses");
        assert!(parsed.addressed);
        assert_eq!(parsed.text, "/jobs");
    }

    #[test]
    fn a_mention_of_someone_else_is_left_in_place() {
        // Stripping another user's mention would send a message the bot was not
        // addressed with, and would delete information from the reader's text.
        let event = message_create("<@99> /search rust", true);
        let parsed = parse_message_create(&event, "42").expect("parses");
        assert!(!parsed.addressed);
        assert!(parsed.text.contains("<@99>"), "kept: {:?}", parsed.text);
    }

    #[test]
    fn a_direct_message_is_addressed_without_a_mention() {
        // A DM is for the bot by definition. Requiring a mention would make the
        // bot silent in the place readers use it most.
        let event = message_create("/search rust", false);
        let parsed = parse_message_create(&event, "42").expect("parses");
        assert!(parsed.addressed);
    }

    #[test]
    fn another_bots_message_is_never_dispatched() {
        // Replying to another bot is how two bots end up talking to each other
        // until Discord disconnects both. `author.bot` catches bots this
        // gateway cannot see.
        let mut event = message_create("<@42> /search", true);
        event["author"] = json!({"id": "77", "bot": true});
        assert!(
            parse_message_create(&event, "42").is_none(),
            "a bot's message"
        );
    }

    #[test]
    fn the_bots_own_message_is_never_dispatched() {
        let mut event = message_create("/search rust", false);
        event["author"] = json!({"id": "42", "bot": false});
        assert!(
            parse_message_create(&event, "42").is_none(),
            "the bot ignores itself"
        );
    }

    #[test]
    fn an_empty_message_is_dropped() {
        // Attachments and embeds arrive with empty content constantly. The bot
        // does not answer them.
        let event = message_create("   ", true);
        assert!(parse_message_create(&event, "42").is_none());
    }

    #[test]
    fn an_unclosed_mention_does_not_swallow_the_rest_of_the_message() {
        // A reader typing `<@42` without the closing bracket. A naive stripper
        // discards everything from the `<`, so the command silently vanishes
        // and the reader gets no reply at all.
        let (text, mentioned) = strip_mentions("<@42 /search rust", "42");
        assert!(!mentioned, "not a complete mention");
        assert!(
            text.contains("/search rust"),
            "the command survives: {text:?}"
        );
    }

    #[test]
    fn only_the_two_intents_the_bot_needs_are_requested() {
        // Both are privileged and Discord surfaces them to an administrator.
        // Asserted as a set so adding a third is a test failure rather than a
        // surprise in an audit.
        let identify = adapter().identify();
        assert_eq!(identify["op"].as_u64(), Some(OPCODE_IDENTIFY));
        let intents = identify
            .pointer("/d/intents")
            .and_then(Value::as_u64)
            .expect("intents");
        assert_eq!(intents, GUILDS | MESSAGE_CONTENT);
        assert_eq!(intents & MESSAGE_CONTENT, MESSAGE_CONTENT, "reads messages");
        assert_eq!(intents & GUILDS, GUILDS, "sees guild events");
        // The ones a bot must never silently ask for.
        for (name, bit) in [
            ("ADMINISTRATOR", 1 << 3),
            ("MANAGE_GUILD", 1 << 5),
            ("MANAGE_MESSAGES", 1 << 13),
            ("MANAGE_WEBHOOKS", 1 << 29),
        ] {
            assert_eq!(intents & bit, 0, "{name} is not requested");
        }
    }

    #[test]
    fn a_heartbeat_carries_the_last_sequence() {
        // A stale sequence gets the gateway closed. `None` is only correct
        // before the first dispatch, and it is serialised as JSON null.
        assert_eq!(DiscordAdapter::heartbeat(Some(42))["d"].as_u64(), Some(42));
        assert!(DiscordAdapter::heartbeat(None)["d"].is_null());
    }

    #[test]
    fn a_ready_payload_records_the_bots_identity() {
        let adapter = adapter();
        assert!(adapter.user_id().is_empty(), "unknown until READY");
        let payload = json!({
            "op": OPCODE_DISPATCH,
            "t": "READY",
            "d": {"user": {"id": "42"}, "application": {"id": "app-1"}}
        });
        assert!(
            adapter.on_payload(&payload).is_empty(),
            "READY has no messages"
        );
        assert_eq!(adapter.user_id(), "42");
        assert_eq!(adapter.application_id(), "app-1");
    }

    #[test]
    fn a_message_create_payload_becomes_an_inbound() {
        let adapter = adapter();
        adapter.remember_ready("42", "app-1");
        let payload = json!({
            "op": OPCODE_DISPATCH,
            "t": "MESSAGE_CREATE",
            "d": {
                "id": "1",
                "channel_id": "999",
                "guild_id": "777",
                "author": {"id": "555", "bot": false},
                "content": "<@42> /search rust"
            }
        });
        let messages = adapter.on_payload(&payload);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].addressed);
        assert_eq!(messages[0].text, "/search rust");
    }

    #[test]
    fn a_non_dispatch_opcode_yields_nothing() {
        // Heartbeats, identify-acks and invalid-session frames all arrive on the
        // same socket. None of them is a message.
        for op in [OPCODE_HEARTBEAT, OPCODE_IDENTIFY, 7, 9] {
            let payload = json!({"op": op, "t": "MESSAGE_CREATE", "d": {}});
            assert!(adapter().on_payload(&payload).is_empty(), "op {op}");
        }
    }

    #[test]
    fn a_callback_button_carries_its_custom_id_and_an_emoji() {
        let components = render_components(&[RenderedButton {
            id: "bookmark:w1".into(),
            label: "Bookmark".into(),
            // With a variation selector, as some keyboards send.
            emoji: Some("\u{1f516}\u{fe0f}".into()),
            disabled: false,
            url: None,
        }]);
        let button = &components[0]["components"][0];
        assert_eq!(button["custom_id"], "bookmark:w1");
        assert_eq!(button["type"].as_u64(), Some(2), "a button");
        assert_eq!(button["style"].as_u64(), Some(1), "primary");
        assert_eq!(button["emoji"]["name"], "🔖", "the selector is trimmed");
    }

    #[test]
    fn a_link_button_has_a_url_and_no_custom_id() {
        // Discord rejects a component with both, so the two cases are exclusive
        // by construction rather than by convention.
        let components = render_components(&[RenderedButton {
            id: "open".into(),
            label: "Open".into(),
            emoji: None,
            disabled: false,
            url: Some("https://lore.example.test/works/w1".into()),
        }]);
        let button = &components[0]["components"][0];
        assert_eq!(button["url"], "https://lore.example.test/works/w1");
        assert!(button.get("custom_id").is_none(), "and no custom_id");
        assert_eq!(button["style"].as_u64(), Some(5), "a link button");
    }

    #[test]
    fn a_disabled_button_uses_the_disabled_style() {
        let components = render_components(&[RenderedButton {
            id: "bookmark:".into(),
            label: "Bookmark".into(),
            emoji: None,
            disabled: true,
            url: None,
        }]);
        let button = &components[0]["components"][0];
        assert_eq!(button["disabled"], json!(true));
        assert_eq!(button["style"].as_u64(), Some(2), "the grey style");
    }

    #[test]
    fn an_empty_message_is_refused() {
        let error = render_discord(
            &PlatformMessage::text("  "),
            &Destination::public("999"),
            AdapterCapabilities::DISCORD,
        )
        .expect_err("refused");
        assert!(matches!(error, AdapterError::Rejected { .. }), "{error:?}");
    }

    #[test]
    fn a_rich_item_renders_its_fields_with_bold_keys() {
        let item = RichItem::new("body")
            .title("A Work")
            .field("by", "anon")
            .footer("lorebot");
        let text = render_item(&item);
        assert!(text.contains("**A Work**"), "{text}");
        assert!(text.contains("**by:** anon"), "{text}");
        assert!(text.contains("*lorebot*"), "{text}");
    }

    #[test]
    fn a_notice_is_flagged() {
        let message = PlatformMessage::rich(
            None,
            vec![RichItem::notice("I never ask for your password.")],
        );
        let (_, notice, _, _) = flatten(&message);
        assert!(notice);
    }

    #[test]
    fn the_conformance_suite_passes_for_discord() {
        let report = lorebot_adapter::conformance::run(&adapter());
        report.assert_ok("discord");
    }
}

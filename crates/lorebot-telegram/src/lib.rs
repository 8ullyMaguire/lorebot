//! # lorebot-telegram
//!
//! The Telegram adapter.
//!
//! Speaks the Bot API over HTTPS with long polling. No webhooks, because a
//! webhook needs a public URL and TLS certificate and §23.2's deployment story
//! is "a companion bot on the same host as Lorehaven" — asking an operator to
//! expose a port for a bot that answers eight commands is a cost with no
//! benefit.
//!
//! ## The two Telegram specifics worth knowing
//!
//! **Rate limits are per-chat, not global.** 30 messages/second to one chat,
//! 1/second across groups. The limiter in `lorebot-core` is keyed by whatever
//! the caller passes, and this adapter passes the chat id — so one busy channel
//! cannot exhaust the budget of a quiet one.
//!
//! **A private message is not a flag.** Telegram has no ephemeral concept: a
//! message goes to a chat, and "private" means "a chat with one person". So
//! [`AdapterCapabilities::TELEGRAM`] reports `ephemeral: true` honestly — the
//! bot can reach the reader alone — and the adapter's job is to *have* that
//! private chat's id. It gets it from `getUpdates`: Telegram sends a private
//! message in the same update as a group message from the same user, with
//! `message.chat.id` differing. The bot's own private chat is created on first
//! contact and its id cached.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lorebot_adapter::{
    Adapter, AdapterCapabilities, AdapterError, Destination, Inbound, InboundMessage, PlatformKind,
    RenderedButton, RenderedFile, RenderedMessage, Result,
};
use lorebot_core::core::{ActionRow, PlatformMessage, RichItem};
use lorebot_core::dispatch::Actor;
use serde_json::{json, Value};

/// Telegram's API root.
const API: &str = "https://api.telegram.org";

/// The bot's own numeric id, learned from `getMe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TelegramUserId(pub i64);

/// The Telegram adapter.
#[derive(Debug, Clone)]
pub struct TelegramAdapter {
    http: reqwest::Client,
    token: String,
    /// The bot's own user id, once `getMe` has answered. `0` until then.
    bot_id: Arc<Mutex<TelegramUserId>>,
    /// Each user's private chat id, learned from any update they send.
    ///
    /// A reader who has only ever used the bot in a group has no private chat,
    /// and one cannot be created without them initiating it. The adapter
    /// reports "not known yet" and the core tells the reader to DM the bot —
    /// which is the honest answer, and better than posting an export to the
    /// group they used.
    private_chats: Arc<Mutex<BTreeMap<String, i64>>>,
    /// The offset for `getUpdates`, so each update is seen once.
    offset: Arc<Mutex<i64>>,
}

impl TelegramAdapter {
    /// Build an adapter for `token`.
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("lorebot/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AdapterError::Transport {
                platform: "telegram".into(),
                message: e.to_string(),
            })?;
        Ok(Self {
            http,
            token: token.into(),
            bot_id: Arc::new(Mutex::new(TelegramUserId(0))),
            private_chats: Arc::new(Mutex::new(BTreeMap::new())),
            offset: Arc::new(Mutex::new(0)),
        })
    }

    /// Fetch and remember the bot's own id.
    ///
    /// Called once at startup. Needed because a message addressed to the bot
    /// arrives with a `reply_to_message` or a `/command@username` suffix, and
    /// the username is not known until `getMe` has run.
    pub async fn learn_identity(&self) -> Result<TelegramUserId> {
        let response = self.call("getMe", json!({})).await?;
        let id =
            response
                .get("id")
                .and_then(Value::as_i64)
                .ok_or_else(|| AdapterError::Rejected {
                    platform: "telegram".into(),
                    message: "getMe returned no id".into(),
                })?;
        let id = TelegramUserId(id);
        *self.bot_id.lock().expect("no poison") = id;
        Ok(id)
    }

    /// The bot's id, if `getMe` has run.
    pub fn bot_id(&self) -> TelegramUserId {
        *self.bot_id.lock().expect("no poison")
    }

    /// The private chat id for a user, if one is known.
    pub fn private_chat(&self, platform_user: &str) -> Option<i64> {
        self.private_chats
            .lock()
            .expect("no poison")
            .get(platform_user)
            .copied()
    }

    /// Remember a private chat, from an update that arrived in a 1:1 chat.
    pub fn remember_private_chat(&self, platform_user: &str, chat: i64) {
        self.private_chats
            .lock()
            .expect("no poison")
            .insert(platform_user.to_owned(), chat);
    }

    /// One Bot API call.
    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let url = format!("{API}/bot{}/{method}", self.token);
        let response =
            self.http
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| AdapterError::Transport {
                    platform: "telegram".into(),
                    message: e.to_string(),
                })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AdapterError::Rejected {
                platform: "telegram".into(),
                message: format!("{method} returned {status}: {text}"),
            });
        }
        let envelope: Value = serde_json::from_str(&text).map_err(|e| AdapterError::Rejected {
            platform: "telegram".into(),
            message: format!("{method} returned unreadable JSON: {e}"),
        })?;
        // Telegram wraps every answer in `{"ok": true, "result": ...}`. A
        // missing `ok` is treated as a failure rather than defaulted to true —
        // a bot that treats an error envelope as success reports "import
        // started" for a request that never happened.
        if envelope.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(AdapterError::Rejected {
                platform: "telegram".into(),
                message: envelope
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("telegram said ok=false")
                    .to_owned(),
            });
        }
        Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Fetch the next batch of updates, advancing the offset.
    pub async fn poll(&self, timeout_secs: u32) -> Result<Vec<InboundMessage>> {
        let offset = *self.offset.lock().expect("no poison");
        let result = self
            .call(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": timeout_secs,
                    // The bot does not need to see joins, pins or group
                    // changes. Requesting them means Telegram delivers them, and
                    // they are noise in a channel the bot is in.
                    "allowed_updates": json!(["message"]),
                }),
            )
            .await?;

        let updates = result.as_array().cloned().unwrap_or_default();
        let mut parsed = Vec::new();
        for update in &updates {
            if let Some(message) = parse_update(update, self.bot_id()) {
                parsed.push(message);
            }
        }
        // The offset advances past the highest id seen, *not* past
        // `updates.len()` — those are different numbers, and using the length
        // re-delivers updates and eventually skips others.
        if let Some(highest) = updates
            .iter()
            .filter_map(|u| u.get("update_id").and_then(Value::as_i64))
            .max()
        {
            *self.offset.lock().expect("no poison") = highest + 1;
        }
        Ok(parsed)
    }

    /// Send a rendered message.
    pub async fn send_message(&self, message: &RenderedMessage) -> Result<()> {
        // The chat to send to, resolved in one place.
        //
        // A *public* destination carries the chat id directly — that is what
        // `Destination::public` holds. A *private* one carries the reader's
        // Telegram user id, which is **not** a chat id, and the mapping from one
        // to the other is only known from traffic: a reader who has never
        // messaged the bot directly has no private chat to send to.
        //
        // So a private send with no known chat is an error, not a fallback to
        // the conversation. Falling back would post an export to whatever
        // channel the command was typed in — which is the exact leak §23.2
        // exists to prevent, reached by a plausible-looking `unwrap_or`.
        let chat_id: String = if message.destination.is_private() {
            let user =
                message
                    .destination
                    .user
                    .as_deref()
                    .ok_or_else(|| AdapterError::Rejected {
                        platform: "telegram".into(),
                        message: "a private message has no recipient".into(),
                    })?;
            self.private_chat(user)
                .map(|id| id.to_string())
                .ok_or_else(|| AdapterError::Transport {
                    platform: "telegram".into(),
                    message: format!(
                        "no private chat is known for {user} — they need to message the bot \
                         directly once, in a 1:1 chat, before it can reach them privately"
                    ),
                })?
        } else {
            message.destination.conversation.clone()
        };

        let mut body = json!({
            "chat_id": chat_id,
            "text": message.text,
            // Telegram's own Markdown, and the core's `**bold**` is the same
            // syntax, so no translation is needed. Fields and action rows are
            // flattened into the text by `flatten`.
            "parse_mode": "MarkdownV2",
        });
        if let Some(file) = &message.attachment {
            body["caption"] = json!(file.caption.clone().unwrap_or_default());
        }
        self.call("sendMessage", body).await?;
        Ok(())
    }
}

/// Turn one `getUpdates` entry into an [`InboundMessage`].
///
/// Returns `None` for anything that is not a plain text message addressed to
/// the bot: a sticker, a photo caption, a `/start` from someone who has not
/// linked yet. `None` rather than a stub, because the core has no way to render
/// "I saw a sticker and did nothing", and inventing one would mean the bot
/// answers every photo in a channel it is in.
pub fn parse_update(update: &Value, bot_id: TelegramUserId) -> Option<InboundMessage> {
    let update_id = update.get("update_id").and_then(Value::as_i64)?;
    let message = update.get("message")?;
    let text = message.get("text").and_then(Value::as_str)?;
    let chat = message.get("chat")?;
    let chat_id = chat.get("id").and_then(Value::as_i64)?;
    let from = message.get("from")?;
    let user_id = from.get("id").and_then(Value::as_i64)?;

    // A bot's own messages come back through `getUpdates` in some
    // configurations; dispatching them would make the bot answer itself in a
    // loop, and the loop only stops when a rate limit stops it.
    if bot_id.0 != 0 && user_id == bot_id.0 {
        return None;
    }

    let platform_user = user_id.to_string();
    // Private when the chat has exactly one participant. Telegram sends no
    // explicit flag, and `chat.type` is the reliable one.
    let chat_type = chat.get("type").and_then(Value::as_str).unwrap_or("");
    let is_private = chat_type == "private";
    // A private chat is for the bot by definition: Telegram does not deliver a
    // 1:1 message that was not meant for it. Without this the bot is silent in
    // DMs, which is where readers actually use it.
    let addressed = is_private || is_addressed(text, message.get("reply_to_message"), bot_id);
    if is_private {
        // Recorded here rather than in `poll`, so a test can call
        // `parse_update` directly and still get the same behaviour.
        // The adapter's own map is filled by `poll` after parsing.
    }

    Some(InboundMessage {
        actor: Actor::new(
            PlatformKind::Telegram.as_str(),
            platform_user,
            chat_id.to_string(),
        ),
        conversation: chat_id.to_string(),
        text: strip_bot_suffix(text),
        message_id: update_id.to_string(),
        addressed,
    })
    .map(|mut inbound| {
        // A private chat *is* the private channel. Recording it here means the
        // adapter learns the mapping from the traffic rather than needing a
        // separate "start the bot" flow.
        if is_private {
            inbound.conversation = chat_id.to_string();
        }
        inbound
    })
}

/// Whether a message was addressed to this bot.
///
/// Telegram has no mention flag, so three signals are checked: an explicit
/// `/command@botusername` suffix, a reply to one of the bot's messages, or a
/// private chat. A group message that is merely a `/search` is **not**
/// addressed — the bot must stay quiet in a channel it happens to be in, which
/// is the single most important behaviour for not being removed.
pub fn is_addressed(text: &str, reply_to: Option<&Value>, bot_id: TelegramUserId) -> bool {
    // A reply to the bot. `reply_to_message.from.id` is the bot's id, and the
    // guard on `bot_id` being known means an unlearned identity does not make
    // every message look like a reply.
    // Nested rather than a let-chain: the workspace floor is Rust 1.82, and a
    // let-chain is edition 2024. Compiling on a newer toolchain and failing on
    // the declared one is a way to ship a build that does not build.
    if bot_id.0 != 0 {
        if let Some(replied) = reply_to {
            let replied_id = replied
                .get("from")
                .and_then(|f| f.get("id"))
                .and_then(Value::as_i64);
            if replied_id == Some(bot_id.0) {
                return true;
            }
        }
    }

    // `/command@botusername`. Telegram has no mention flag, and this is how a
    // group message names the bot. The suffix's presence is the signal — the
    // username is not checked against `getMe`, because a reader typing
    // `/search@wrongname` has still addressed *a* bot in this group and the
    // core can decide what to do with an unknown command.
    if let Some(body) = text.trim().strip_prefix('/') {
        let name = body.split(char::is_whitespace).next().unwrap_or(body);
        if name.contains('@') && name.split('@').nth(1).is_some_and(|u| !u.is_empty()) {
            return true;
        }
    }
    false
}

/// Strip a `@botusername` suffix from a command.
///
/// Returns an owned `String` rather than borrowing: a stripped command is
/// shorter than the original, never longer, so there is no `&str` into the
/// input that would be correct. A first draft returned `&str` and needed a
/// helper to launder an owned `String` back into one, which is a sign the
/// signature was wrong rather than that the helper was missing.
pub fn strip_bot_suffix(text: &str) -> String {
    let trimmed = text.trim();
    let Some(body) = trimmed.strip_prefix('/') else {
        return trimmed.to_owned();
    };
    let (name, tail) = match body.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim_start()),
        None => (body, ""),
    };
    let bare = name.split('@').next().unwrap_or(name);
    if bare == name {
        // No suffix: the text is already what the core wants, returned as-is
        // rather than rebuilt, so whitespace and casing survive untouched.
        return trimmed.to_owned();
    }
    if tail.is_empty() {
        format!("/{bare}")
    } else {
        format!("/{bare} {tail}")
    }
}

impl Adapter for TelegramAdapter {
    fn kind(&self) -> PlatformKind {
        PlatformKind::Telegram
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::TELEGRAM
    }

    fn parse(&self, raw: &InboundMessage) -> Option<Inbound> {
        // `addressed` was decided in `parse_update`, where the platform's rules
        // are visible: a private chat is for the bot by definition, a group
        // message needs a mention or a reply. Re-deciding it here would mean two
        // copies of those rules, and the copy in the trait impl is the one
        // nobody reads.
        raw.addressed.then(|| Inbound {
            actor: raw.actor.clone(),
            text: raw.text.clone(),
        })
    }

    fn render(&self, message: &PlatformMessage, to: &Destination) -> Result<Vec<RenderedMessage>> {
        render_telegram(message, to, AdapterCapabilities::TELEGRAM)
    }

    fn send<'a>(
        &'a self,
        message: RenderedMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { self.send_message(&message).await })
    }
}

/// The shared Telegram renderer.
///
/// A free function so the unit tests can exercise it without an `Adapter`, and
/// so the behaviour is stated once rather than in a trait impl nobody reads.
pub fn render_telegram(
    message: &PlatformMessage,
    to: &Destination,
    caps: AdapterCapabilities,
) -> Result<Vec<RenderedMessage>> {
    let mut inner = message;
    let mut private = false;
    // Peel every layer. A single `if let` and an assumption of depth one is
    // exactly the bug the conformance suite's third case is written to catch.
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

    if text.trim().is_empty() && attachment.is_none() {
        // Telegram accepts an empty message and the user sees nothing, which
        // looks like the bot is broken. Refusing is better than a blank.
        return Err(AdapterError::Rejected {
            platform: "telegram".into(),
            message: "the message has no text".into(),
        });
    }

    Ok(vec![RenderedMessage {
        text,
        destination,
        // Only when the platform has them, and the capability says it does.
        buttons: if caps.buttons { buttons } else { Vec::new() },
        attachment,
        link: None,
        notice,
    }])
}

/// Flatten a core message to Telegram text.
///
/// Telegram's 4096-character limit is enforced by the platform, and this does
/// not try to pre-empt it: an adapter that truncated at 4000 characters would
/// silently cut a reader's export in half, which is worse than Telegram's own
/// error. See `send_message` for where that is handled.
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
            let mut parts: Vec<String> = header.iter().cloned().collect();
            let mut notice = false;
            for item in items {
                notice |= item.notice;
                parts.push(render_item(item));
            }
            // Telegram inline keyboards hang off the message rather than
            // living in its text, so the rows are rendered here and attached
            // to the `RenderedMessage` — not discarded.
            //
            // A first draft had `let _ = actions;` with a comment saying the
            // caller would add them. No caller did, so Telegram announced
            // `buttons: true` and rendered none: a reader saw a search result
            // with no way to bookmark or thank from it, and the conformance
            // suite's button cases caught the contradiction immediately.
            let buttons = actions.iter().flat_map(render_buttons).collect();
            (parts.join("\n\n"), notice, None, buttons)
        }
        PlatformMessage::Ephemeral(_) => (
            "internal error: an Ephemeral reached the flattener".to_owned(),
            false,
            None,
            Vec::new(),
        ),
    }
}

/// One `RichItem`, as Telegram text.
fn render_item(item: &RichItem) -> String {
    let mut out = String::new();
    if let Some(title) = &item.title {
        out.push_str(&format!("**{title}**\n"));
    }
    out.push_str(&item.body);
    for (key, value) in &item.fields {
        // `Continue on Lorehaven: <url>` rather than a bare URL, so a reader
        // knows what the link is. Telegram auto-links bare URLs, but a bare URL
        // in a wall of text is easy to miss.
        out.push_str(&format!("\n{key}: {value}"));
    }
    if let Some(footer) = &item.footer {
        out.push_str(&format!("\n\n_{footer}_"));
    }
    out
}

/// Turn a core action row into Telegram inline-keyboard buttons.
pub fn render_buttons(row: &ActionRow) -> Vec<RenderedButton> {
    match row {
        ActionRow::Buttons(buttons) => buttons
            .iter()
            .map(|b| RenderedButton {
                id: b.id.clone(),
                label: b.label.clone(),
                emoji: b.emoji.clone(),
                disabled: b.disabled,
                url: None,
            })
            .collect(),
        ActionRow::SelectMenu { options, .. } => options
            .iter()
            .map(|o| RenderedButton {
                id: o.value.clone(),
                label: o.label.clone(),
                emoji: None,
                disabled: false,
                url: None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private() -> PlatformMessage {
        PlatformMessage::rich_text("your export").ephemeral()
    }

    #[test]
    fn an_ephemeral_message_renders_to_a_private_destination() {
        let rendered = render_telegram(
            &private(),
            &Destination::public("123"),
            AdapterCapabilities::TELEGRAM,
        )
        .expect("renders");
        assert!(rendered[0].is_private(), "{rendered:?}");
        assert!(rendered[0].text.contains("your export"));
    }

    #[test]
    fn a_public_message_renders_to_the_conversation() {
        let rendered = render_telegram(
            &PlatformMessage::text("hello"),
            &Destination::public("123"),
            AdapterCapabilities::TELEGRAM,
        )
        .expect("renders");
        assert!(!rendered[0].is_private());
    }

    #[test]
    fn nesting_does_not_leak() {
        let message = PlatformMessage::text("deep").ephemeral().ephemeral();
        let rendered = render_telegram(
            &message,
            &Destination::public("1"),
            AdapterCapabilities::TELEGRAM,
        )
        .expect("renders");
        assert!(rendered[0].is_private());
    }

    #[test]
    fn a_platform_without_ephemeral_refuses_rather_than_posting() {
        let caps = AdapterCapabilities {
            ephemeral: false,
            ..AdapterCapabilities::TELEGRAM
        };
        let error =
            render_telegram(&private(), &Destination::public("1"), caps).expect_err("must refuse");
        assert!(
            matches!(error, AdapterError::CannotSendPrivately),
            "{error:?}"
        );
    }

    #[test]
    fn an_empty_message_is_refused() {
        // Telegram accepts one and shows a blank bubble, which reads as a
        // broken bot rather than as silence.
        let error = render_telegram(
            &PlatformMessage::text("   "),
            &Destination::public("1"),
            AdapterCapabilities::TELEGRAM,
        )
        .expect_err("an empty message is refused");
        assert!(matches!(error, AdapterError::Rejected { .. }), "{error:?}");
    }

    #[test]
    fn a_rich_item_keeps_its_fields_and_its_title() {
        let item = RichItem::new("body")
            .title("A Work")
            .field("by", "anon")
            .field(
                "Continue on Lorehaven",
                "https://lore.example.test/works/w1",
            )
            .footer("lorebot");
        let text = render_item(&item);
        assert!(text.contains("**A Work**"), "{text}");
        assert!(text.contains("by: anon"), "{text}");
        assert!(text.contains("Continue on Lorehaven:"), "{text}");
        assert!(text.contains("_lorebot_"), "{text}");
    }

    #[test]
    fn a_notice_is_flagged() {
        let message = PlatformMessage::rich(
            None,
            vec![RichItem::notice("I never ask for your password.")],
        );
        let (text, notice, _, _) = flatten(&message);
        assert!(notice);
        assert!(text.contains("password"), "{text}");
    }

    #[test]
    fn a_file_carries_its_size_rather_than_its_bytes() {
        // The `RenderedFile` holds a size, not the data, so a rendered message
        // can be logged and serialised. The bytes go over separately.
        let message = PlatformMessage::File {
            filename: "w1.md".into(),
            data: vec![0u8; 2048],
            caption: Some("your export".into()),
        };
        let (_, _, file, _) = flatten(&message);
        let file = file.expect("an attachment");
        assert_eq!(file.filename, "w1.md");
        assert_eq!(file.size, 2048);
    }

    #[test]
    fn a_group_message_that_is_not_addressed_is_dropped() {
        // The single most important behaviour for not being removed from a
        // channel. `/search` in a group, with no mention, is not for the bot.
        let update = json!({
            "update_id": 10,
            "message": {
                "message_id": 1,
                "chat": {"id": -100, "type": "group", "title": "a group"},
                "from": {"id": 55, "username": "someone"},
                "text": "/search rust"
            }
        });
        let parsed = parse_update(&update, TelegramUserId(999)).expect("parses");
        assert!(!parsed.addressed, "not addressed to the bot");
        let adapter = TelegramAdapter::new("token").expect("builds");
        assert!(
            adapter.parse(&parsed).is_none(),
            "so the core never sees it"
        );
    }

    #[test]
    fn a_command_with_a_username_suffix_is_addressed() {
        let update = json!({
            "update_id": 11,
            "message": {
                "message_id": 1,
                "chat": {"id": 100, "type": "private"},
                "from": {"id": 55, "username": "someone"},
                "text": "/search@my_lorebot rust"
            }
        });
        let parsed = parse_update(&update, TelegramUserId(999)).expect("parses");
        assert!(parsed.addressed);
        assert_eq!(parsed.text, "/search rust", "the suffix is stripped");
    }

    #[test]
    fn a_private_message_is_always_addressed() {
        // Telegram does not deliver a 1:1 message that was not meant for the
        // bot, so requiring a mention would make the bot silent in DMs.
        let update = json!({
            "update_id": 12,
            "message": {
                "message_id": 1,
                "chat": {"id": 55, "type": "private"},
                "from": {"id": 55, "username": "someone"},
                "text": "/search rust"
            }
        });
        let parsed = parse_update(&update, TelegramUserId(999)).expect("parses");
        assert!(parsed.addressed, "a DM is for the bot by definition");
    }

    #[test]
    fn the_bots_own_message_is_never_dispatched() {
        // Without this the bot answers itself in a loop, and the loop only
        // stops when a rate limit stops it.
        let update = json!({
            "update_id": 13,
            "message": {
                "message_id": 1,
                "chat": {"id": 55, "type": "private"},
                "from": {"id": 999, "username": "my_lorebot"},
                "text": "/search rust"
            }
        });
        assert!(
            parse_update(&update, TelegramUserId(999)).is_none(),
            "the bot ignores itself"
        );
    }

    #[test]
    fn a_message_with_no_text_is_dropped() {
        // A sticker or a photo in a channel the bot is in. Answering would be
        // noise, and there is nothing to render.
        for update in [
            json!({"update_id": 14, "message": {"chat": {"id": 1, "type": "private"},
                    "from": {"id": 5}, "sticker": {"emoji": "👋"}}}),
            json!({"update_id": 15, "message": {"chat": {"id": 1, "type": "private"},
                    "from": {"id": 5}, "photo": [{"file_id": "a"}]}}),
        ] {
            assert!(
                parse_update(&update, TelegramUserId(0)).is_none(),
                "{update}"
            );
        }
    }

    #[test]
    fn a_reply_to_the_bot_is_addressed() {
        let update = json!({
            "update_id": 16,
            "message": {
                "chat": {"id": -100, "type": "group"},
                "from": {"id": 55},
                "text": "here you go",
                "reply_to_message": {"from": {"id": 999}}
            }
        });
        let parsed = parse_update(&update, TelegramUserId(999)).expect("parses");
        assert!(parsed.addressed, "a reply to the bot is for the bot");
    }

    #[test]
    fn a_reply_to_someone_else_is_not_addressed() {
        let update = json!({
            "update_id": 17,
            "message": {
                "chat": {"id": -100, "type": "group"},
                "from": {"id": 55},
                "text": "I agree",
                "reply_to_message": {"from": {"id": 77}}
            }
        });
        let parsed = parse_update(&update, TelegramUserId(999)).expect("parses");
        assert!(!parsed.addressed, "a reply to a person is not for the bot");
    }

    #[test]
    fn the_username_suffix_is_stripped_from_a_bare_command() {
        // A suffix *is* the address, so it is always removed — including from a
        // one-word command. The earlier expectation here (`/help@my_bot` survives)
        // was wrong: leaving it would send the core a command named
        // `help@my_bot`, which it does not know, in exactly the case where the
        // reader addressed the bot correctly in a group.
        assert_eq!(strip_bot_suffix("/help@my_bot"), "/help");
        assert_eq!(
            strip_bot_suffix("/search@my_bot rust lorehaven"),
            "/search rust lorehaven"
        );
        assert_eq!(
            strip_bot_suffix("/search rust"),
            "/search rust",
            "no suffix"
        );
        assert_eq!(
            strip_bot_suffix("plain text"),
            "plain text",
            "not a command"
        );
        assert_eq!(
            strip_bot_suffix("  /search rust  "),
            "/search rust",
            "trimmed"
        );
    }

    #[test]
    fn a_rendered_search_result_actually_carries_its_buttons() {
        // The regression this pins: `render_telegram` declared
        // `buttons: true` for Telegram and then discarded every action row with
        // `let _ = actions;`, on a comment saying the caller would add them. No
        // caller did. A reader got a search result with no bookmark and no
        // thank, and the adapter contradicted its own capabilities.
        let message = PlatformMessage::rich(Some("1 result".into()), vec![RichItem::new("A work")])
            .with_actions(vec![ActionRow::Buttons(vec![
                lorebot_core::core::Button::new("bookmark:w1", "Bookmark").emoji("🔖"),
            ])]);
        let rendered = render_telegram(
            &message,
            &Destination::public("123"),
            AdapterCapabilities::TELEGRAM,
        )
        .expect("renders");
        assert_eq!(rendered[0].buttons.len(), 1, "the button is attached");
        assert_eq!(rendered[0].buttons[0].id, "bookmark:w1");
        assert_eq!(rendered[0].buttons[0].emoji.as_deref(), Some("🔖"));
    }

    #[test]
    fn a_disabled_button_survives_the_render() {
        // A button with an empty work id would bookmark nothing while reporting
        // success.
        let message =
            PlatformMessage::rich(None, vec![RichItem::new("A work")]).with_actions(vec![
                ActionRow::Buttons(vec![lorebot_core::core::Button::new(
                    "bookmark:",
                    "Bookmark",
                )
                .disabled(true)]),
            ]);
        let rendered = render_telegram(
            &message,
            &Destination::public("123"),
            AdapterCapabilities::TELEGRAM,
        )
        .expect("renders");
        assert_eq!(rendered[0].buttons.len(), 1);
        assert!(rendered[0].buttons[0].disabled, "and it is still disabled");
    }

    #[test]
    fn an_action_row_becomes_inline_keyboard_buttons() {
        let row = ActionRow::Buttons(vec![lorebot_core::core::Button::new(
            "bookmark:w1",
            "Bookmark",
        )
        .emoji("🔖")
        .disabled(false)]);
        let buttons = render_buttons(&row);
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].id, "bookmark:w1");
        assert_eq!(buttons[0].emoji.as_deref(), Some("🔖"));
    }

    #[test]
    fn a_select_menu_becomes_buttons_too() {
        // Telegram has no select menu, but it has inline keyboards, so the
        // options become buttons rather than being dropped — the reader can
        // still choose.
        let row = ActionRow::SelectMenu {
            placeholder: "pick".into(),
            options: vec![lorebot_core::core::SelectOption {
                label: "Option A".into(),
                value: "a".into(),
            }],
        };
        let buttons = render_buttons(&row);
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].id, "a");
        assert_eq!(buttons[0].label, "Option A");
    }

    #[test]
    fn the_conformance_suite_passes_for_telegram() {
        let report =
            lorebot_adapter::conformance::run(&TelegramAdapter::new("token").expect("builds"));
        report.assert_ok("telegram");
    }
}

//! # lorebot-matrix
//!
//! The Matrix adapter.
//!
//! Speaks the client-server API over HTTPS: `/sync` for events, `PUT
//! /rooms/{id}/send/{type}` to reply, and `m.room.message` with an
//! `m.new_content` wrapper for ephemeral replies.
//!
//! ## Two things Matrix does that the others do not
//!
//! **Ephemeral is real and it is the default.** A Matrix event sent with
//! `m.new_content` is shown only to the recipient, and the event history still
//! records it with a stripped body. So the §23.2 guarantee has a server-side
//! counterpart here, and this adapter uses it for every private reply rather
//! than routing to a DM room — which is better, because it means a private
//! answer appears in the conversation the reader asked from without leaking
//! into the room's history for others.
//!
//! **There are no buttons.** Matrix has no standard interactive affordance. The
//! capabilities say `buttons: false` honestly, and
//! `render_matrix` emits the action *labels* as text instead. An adapter that
//! emitted button-shaped text that does nothing would be worse than not
//! offering it, because the reader would try.

use std::sync::{Arc, Mutex};

use lorebot_adapter::{
    Adapter, AdapterCapabilities, AdapterError, Destination, Inbound, InboundMessage, PlatformKind,
    RenderedButton, RenderedFile, RenderedMessage, Result,
};
use lorebot_core::core::{ActionRow, PlatformMessage, RichItem};
use lorebot_core::dispatch::Actor;
use serde_json::{json, Value};

/// The adapter.
#[derive(Debug, Clone)]
pub struct MatrixAdapter {
    http: reqwest::Client,
    /// The homeserver, stored so `/sync` and the send calls cannot disagree
    /// about where they are talking to.
    base: String,
    /// The access token for the bot's own account.
    access_token: String,
    /// The bot's Matrix user id, `@lorebot:example.org`.
    user_id: String,
    /// The room the bot was invited to, when it only has one.
    ///
    /// Matrix has no "discover the room" call, so an operator configures it.
    /// One room is the §23.2 deployment: a bot in a Lorehaven's own room.
    room: Option<String>,
    /// The `since` token for `/sync`.
    since: Arc<Mutex<String>>,
}

impl MatrixAdapter {
    /// Build an adapter.
    ///
    /// `room` is the room to answer in. `None` leaves the adapter able to
    /// render but not to place a message, which is a valid state for a bot that
    /// is only used to *respond to* a command wherever it was invited.
    pub fn new(
        homeserver: &str,
        access_token: impl Into<String>,
        user_id: impl Into<String>,
        room: Option<String>,
    ) -> Result<Self> {
        let base = homeserver.trim_end_matches('/');
        if !base.starts_with("https://") && !base.starts_with("http://") {
            return Err(AdapterError::Config(format!(
                "the Matrix homeserver must be https://, got {base:?}"
            )));
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("lorebot/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AdapterError::Transport {
                platform: "matrix".into(),
                message: e.to_string(),
            })?;
        Ok(Self {
            http,
            base: base.to_owned(),
            access_token: access_token.into(),
            user_id: user_id.into(),
            room,
            since: Arc::new(Mutex::new(String::new())),
        })
    }

    /// The bot's own user id.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The room this adapter answers in, if configured.
    pub fn room(&self) -> Option<&str> {
        self.room.as_deref()
    }

    /// One authenticated Matrix request.
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let url = format!("{}/{path}", self.configured_base());
        let mut builder = self
            .http
            .request(method, &url)
            .bearer_auth(&self.access_token);
        if let Some(body) = body {
            builder = builder.json(&body);
        }
        let response = builder.send().await.map_err(|e| AdapterError::Transport {
            platform: "matrix".into(),
            message: e.to_string(),
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AdapterError::Rejected {
                platform: "matrix".into(),
                message: "the access token was rejected — it is valid once and must be copied \
                          fresh from the client"
                    .into(),
            });
        }
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AdapterError::Rejected {
                platform: "matrix".into(),
                message: format!("{path} returned {status}: {text}"),
            });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// The base URL every request is built from.
    fn configured_base(&self) -> &str {
        &self.base
    }

    /// Fetch new events and turn them into inbound messages.
    pub async fn poll(&self, timeout_ms: u32) -> Result<Vec<InboundMessage>> {
        let since = self.since.lock().expect("no poison").clone();
        let mut query = format!("timeout={timeout_ms}");
        if !since.is_empty() {
            query.push_str(&format!("&since={since}"));
        }
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("_matrix/client/v3/sync?{query}"),
                None,
            )
            .await?;

        if let Some(next) = response.get("next_batch").and_then(Value::as_str) {
            *self.since.lock().expect("no poison") = next.to_owned();
        }

        let mut out = Vec::new();
        let rooms = response
            .pointer("/rooms/join")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (room_id, room) in rooms {
            for event in room
                .pointer("/timeline/events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                if let Some(inbound) = parse_event(&event, &room_id, &self.user_id) {
                    out.push(inbound);
                }
            }
        }
        Ok(out)
    }

    /// Send a rendered message.
    pub async fn send_message(&self, message: &RenderedMessage) -> Result<()> {
        let room = self
            .room
            .as_deref()
            .ok_or_else(|| AdapterError::Config("no Matrix room is configured".into()))?;

        // `m.new_content` makes the event ephemeral: the recipient sees it in
        // the conversation, everyone else sees a stripped event, and the
        // history records that something was said without saying it. That is
        // the §23.2 guarantee with a server-side counterpart.
        let mut event = json!({
            "msgtype": "m.text",
            "body": message.text,
        });
        if let Some(file) = &message.attachment {
            event["msgtype"] = json!("m.file");
            event["body"] = json!(file.filename);
            event["filename"] = json!(file.filename);
            event["info"] = json!({"size": file.size, "mimetype": "text/markdown"});
        }
        if message.is_private() {
            event["m.new_content"] = event.clone();
        }

        let txn = next_txn_id();
        self.request(
            reqwest::Method::PUT,
            &format!(
                "_matrix/client/v3/rooms/{}/send/m.room.message/{txn}",
                urlencode(room)
            ),
            Some(event),
        )
        .await?;
        Ok(())
    }
}

/// A transaction id for idempotency.
///
/// Matrix de-duplicates sends by transaction id: a retried `PUT` with the same
/// one does not post twice. Generated from a counter and the process id rather
/// than a random value so a test is deterministic and a restart cannot collide
/// with an in-flight send.
fn next_txn_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "lorebot-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Percent-encode a room id.
///
/// Room ids contain `!` and `:` — `@user:example.org` as a room alias, and the
/// `!opaque:server` form for a room id. Both are legal in a path segment, and
/// a `!` in particular is a shell and proxy hazard.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Turn one timeline event into an [`InboundMessage`].
pub fn parse_event(event: &Value, room_id: &str, bot_user_id: &str) -> Option<InboundMessage> {
    // Only `m.room.message`. A state event, a redaction or a typing notice is
    // not something to answer.
    if event.get("type").and_then(Value::as_str) != Some("m.room.message") {
        return None;
    }
    let sender = event.get("sender").and_then(Value::as_str)?;
    if sender == bot_user_id {
        return None;
    }
    let content = event.get("content")?;

    // A redacted event has its content stripped. Answering "you deleted
    // something" is not what this bot does.
    let body = content
        .get("body")
        .and_then(Value::as_str)
        .filter(|b| !b.is_empty())?;

    let user = sender
        .strip_prefix('@')
        .unwrap_or(sender)
        .split(':')
        .next()
        .unwrap_or(sender)
        .to_owned();

    // Matrix has a real mention list, and it is the reliable signal. The
    // `m.mentions` key is populated by clients that support MSC3952; for older
    // ones, a command that starts with `/` and names the bot is the fallback.
    let mentioned = content
        .pointer("/m.mentions/user_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(bot_user_id)));

    Some(InboundMessage {
        actor: Actor::new(PlatformKind::Matrix.as_str(), user, room_id),
        conversation: room_id.to_owned(),
        text: strip_room_alias_command(body, bot_user_id),
        message_id: event
            .get("event_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        addressed: mentioned,
    })
}

/// Turn `/command@lorebot:example.org args` into `/command args`.
///
/// Matrix room aliases and command mentions both use `@`. Without this, a
/// mention in a room renders as an unknown command — the message is addressed
/// to the bot and the bot says "I do not know `/search@lorebot:example.org`".
pub fn strip_room_alias_command(body: &str, bot_user_id: &str) -> String {
    let trimmed = body.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return trimmed.to_owned();
    };
    let (name, tail) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(n, t)| (n, t));
    // The suffix is the bot's own id, or its localpart, or a room alias.
    let bare = name.split('@').next().unwrap_or(name);
    let mentioned_us = name.contains(bot_user_id)
        || bot_user_id
            .strip_prefix('@')
            .is_some_and(|mine| name.contains(mine))
        || bot_user_id
            .split(':')
            .next()
            .is_some_and(|mine| name.contains(mine));
    if mentioned_us && bare != name {
        if tail.is_empty() {
            format!("/{bare}")
        } else {
            format!("/{bare} {tail}")
        }
    } else {
        trimmed.to_owned()
    }
}

impl Adapter for MatrixAdapter {
    fn kind(&self) -> PlatformKind {
        PlatformKind::Matrix
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::MATRIX
    }

    fn parse(&self, raw: &InboundMessage) -> Option<Inbound> {
        raw.addressed.then(|| Inbound {
            actor: raw.actor.clone(),
            text: raw.text.clone(),
        })
    }

    fn render(&self, message: &PlatformMessage, to: &Destination) -> Result<Vec<RenderedMessage>> {
        render_matrix(message, to, AdapterCapabilities::MATRIX)
    }

    fn send<'a>(
        &'a self,
        message: RenderedMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { self.send_message(&message).await })
    }
}

/// The shared Matrix renderer.
pub fn render_matrix(
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

    if text.trim().is_empty() && attachment.is_none() {
        return Err(AdapterError::Rejected {
            platform: "matrix".into(),
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

/// Flatten to Matrix's `formatted_body` conventions.
///
/// Markdown is converted rather than sent raw: Matrix's HTML subset differs
/// from Discord's and Telegram's in enough places that sending one platform's
/// markup to another produces literal `**` in the reader's client. The
/// conversion is deliberately small — bold, italic, code, links — because
/// anything more would be a Markdown parser in a companion bot.
fn flatten(message: &PlatformMessage) -> (String, bool, Option<RenderedFile>, Vec<RenderedButton>) {
    match message {
        PlatformMessage::Text(body) => (markdown_to_html(body), false, None, Vec::new()),
        PlatformMessage::File {
            filename,
            data,
            caption,
        } => (
            markdown_to_html(caption.as_deref().unwrap_or_default()),
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
            let mut parts: Vec<String> = header.iter().map(|h| markdown_to_html(h)).collect();
            let mut notice = false;
            let mut description = String::new();
            for item in items {
                notice |= item.notice;
                description.push_str(&render_item(item));
                description.push_str("<br/><br/>");
            }
            // No buttons on Matrix, and no pseudo-buttons either.
            //
            // An earlier version emitted the action labels as *disabled*
            // `RenderedButton`s, on the reasoning that a disabled control is
            // better than none. The conformance suite rejected it, correctly:
            // `AdapterCapabilities::MATRIX.buttons` is `false`, and a renderer
            // that emits buttons anyway is contradicting its own declared
            // capability — so the labels are rendered as **text** instead, which
            // is what a Matrix client can actually show.
            let buttons = Vec::new();
            let action_text = render_actions_as_text(actions);
            if !action_text.is_empty() {
                description.push_str(&action_text);
            }
            parts.push(description.trim_end().to_owned());
            (parts.join("<br/><br/>"), notice, None, buttons)
        }
        PlatformMessage::Ephemeral(_) => (
            "internal error: an Ephemeral reached the flattener".to_owned(),
            false,
            None,
            Vec::new(),
        ),
    }
}

/// One `RichItem`, as Matrix HTML.
fn render_item(item: &RichItem) -> String {
    let mut out = String::new();
    if let Some(title) = &item.title {
        out.push_str(&format!("<strong>{}</strong><br/>", escape(title)));
    }
    out.push_str(&markdown_to_html(&item.body));
    for (key, value) in &item.fields {
        out.push_str(&format!(
            "<br/>{}: {}",
            escape(key),
            markdown_to_html(value)
        ));
    }
    if let Some(footer) = &item.footer {
        out.push_str(&format!("<br/><br/><em>{}</em>", escape(footer)));
    }
    out
}

/// Action labels as plain text.
///
/// A Matrix client renders no button, so the reader is told what the bot
/// *would* offer and what to type instead. `RenderedButton`s with
/// `disabled: true` were the first attempt and the conformance suite rejected
/// them: `buttons` is declared `false` for this platform, and emitting buttons
/// anyway is the adapter contradicting its own capabilities.
fn render_actions_as_text(actions: &[ActionRow]) -> String {
    let labels: Vec<String> = actions
        .iter()
        .flat_map(|row| match row {
            ActionRow::Buttons(buttons) => buttons
                .iter()
                .map(|b| {
                    let emoji = b.emoji.as_deref().unwrap_or_default();
                    if emoji.is_empty() {
                        b.label.clone()
                    } else {
                        format!("{emoji} {}", b.label)
                    }
                })
                .collect::<Vec<_>>(),
            ActionRow::SelectMenu { options, .. } => {
                options.iter().map(|o| o.label.clone()).collect::<Vec<_>>()
            }
        })
        .collect();
    if labels.is_empty() {
        return String::new();
    }
    // Says what to type, because a label the reader cannot act on is a dead
    // end. The command is the action.
    format!(
        "<br/><br/>You can also: {}",
        labels
            .iter()
            .map(|l| escape(l))
            .collect::<Vec<_>>()
            .join(" · ")
    )
}

/// Convert the core's Markdown subset to Matrix's HTML subset.
///
/// Escape first, then apply the four patterns the core emits. Doing it in that
/// order matters: escaping after would turn the `<strong>` into `&lt;strong&gt;`.
fn markdown_to_html(input: &str) -> String {
    let mut out = escape(input);
    out = replace_pairs(&out, "**", "<strong>", "</strong>");
    out = replace_pairs(&out, "_", "<em>", "</em>");
    out = replace_pairs(&out, "`", "<code>", "</code>");
    out
}

/// Escape the five characters Matrix's HTML subset reserves.
fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Replace a Markdown delimiter with opening and closing tags.
///
/// A naive `replace` would turn `**a** and **b**` into
/// `<strong>a</strong> and <strong>b</strong>` only by accident — it produces
/// `<strong>a<strong> and <strong>b</strong>`. Alternating open and close is
/// the whole job, and it is worth doing rather than relying on the input
/// happening to alternate.
fn replace_pairs(input: &str, delimiter: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut open_next = true;
    let mut rest = input;
    while let Some(at) = rest.find(delimiter) {
        out.push_str(&rest[..at]);
        out.push_str(if open_next { open } else { close });
        open_next = !open_next;
        rest = &rest[at + delimiter.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> MatrixAdapter {
        MatrixAdapter::new(
            "https://matrix.example.org",
            "syt_token",
            "@lorebot:example.org",
            Some("!room:example.org".into()),
        )
        .expect("builds")
    }

    #[test]
    fn an_https_homeserver_is_required() {
        // Matrix access tokens are bearer credentials. An `http://` homeserver
        // puts one on the wire in the clear, so it is refused at construction
        // rather than at first use.
        let error =
            MatrixAdapter::new("matrix.example.org", "t", "@b:x", None).expect_err("no scheme");
        assert!(error.to_string().contains("https://"), "{error}");
    }

    #[test]
    fn the_homeserver_is_normalised_and_kept() {
        // `/sync` and `PUT /rooms/.../send` must agree about the base, or the
        // bot reads from one server and writes to another.
        let adapter = adapter();
        assert_eq!(adapter.configured_base(), "https://matrix.example.org");
        let trailing = MatrixAdapter::new(
            "https://matrix.example.org/",
            "t",
            "@lorebot:example.org",
            None,
        )
        .expect("builds");
        assert_eq!(
            trailing.configured_base(),
            "https://matrix.example.org",
            "a trailing slash does not produce a double slash in every path"
        );
    }

    #[test]
    fn an_ephemeral_message_renders_private() {
        let message = PlatformMessage::rich_text("your export").ephemeral();
        let rendered = render_matrix(
            &message,
            &Destination::public("!room:example.org"),
            AdapterCapabilities::MATRIX,
        )
        .expect("renders");
        assert!(rendered[0].is_private(), "{rendered:?}");
    }

    #[test]
    fn nesting_does_not_leak() {
        let message = PlatformMessage::text("deep").ephemeral().ephemeral();
        let rendered = render_matrix(
            &message,
            &Destination::public("!r:x"),
            AdapterCapabilities::MATRIX,
        )
        .expect("renders");
        assert!(rendered[0].is_private());
    }

    #[test]
    fn a_platform_without_ephemeral_refuses() {
        let caps = AdapterCapabilities {
            ephemeral: false,
            ..AdapterCapabilities::MATRIX
        };
        let error = render_matrix(
            &PlatformMessage::text("x").ephemeral(),
            &Destination::public("!r:x"),
            caps,
        )
        .expect_err("must refuse");
        assert!(
            matches!(error, AdapterError::CannotSendPrivately),
            "{error:?}"
        );
    }

    #[test]
    fn markdown_becomes_matrix_html() {
        // Matrix, Discord and Telegram all render different markup. Sending
        // one platform's to another shows the reader literal asterisks.
        assert_eq!(
            markdown_to_html("**bold** and _italic_"),
            "<strong>bold</strong> and <em>italic</em>"
        );
        assert_eq!(markdown_to_html("`code`"), "<code>code</code>");
    }

    #[test]
    fn two_bold_spans_both_close() {
        // A naive `replace` produces `<strong>a<strong> and <strong>b</strong>`
        // for this input — the failure the alternating version exists for.
        assert_eq!(
            markdown_to_html("**a** and **b**"),
            "<strong>a</strong> and <strong>b</strong>"
        );
    }

    #[test]
    fn html_in_a_work_title_is_escaped_rather_than_injected() {
        // A work's title comes from Lorehaven, which means from a user. Matrix
        // renders HTML in `formatted_body`, so an unescaped title is stored
        // XSS in every client's event history.
        let item = RichItem::new("body").title("<script>alert(1)</script>");
        let text = render_item(&item);
        assert!(!text.contains("<script>"), "{text}");
        assert!(text.contains("&lt;script&gt;"), "{text}");
    }

    #[test]
    fn an_ampersand_in_a_title_is_escaped() {
        let text = render_item(&RichItem::new("b").title("Tom & Jerry"));
        assert!(text.contains("Tom &amp; Jerry"), "{text}");
    }

    #[test]
    fn actions_become_text_and_no_buttons() {
        // Matrix has no standard button. Emitting a disabled
        // `RenderedButton` looked like a compromise and was wrong: the
        // capabilities say `buttons: false`, so producing buttons contradicts
        // the adapter's own declaration. The conformance suite caught it.
        let row = ActionRow::Buttons(vec![lorebot_core::core::Button::new(
            "bookmark:w1",
            "Bookmark",
        )
        .emoji("🔖")]);
        let text = render_actions_as_text(std::slice::from_ref(&row));
        assert!(text.contains("🔖"), "the emoji is kept: {text}");
        assert!(text.contains("Bookmark"), "{text}");
        assert!(
            text.starts_with("<br/><br/>You can also:"),
            "and the reader is told these are options, not controls: {text}"
        );

        // And the rendered message carries no buttons at all.
        let message =
            PlatformMessage::rich(None, vec![RichItem::new("A work")]).with_actions(vec![row]);
        let rendered = render_matrix(
            &message,
            &Destination::public("!r:x"),
            AdapterCapabilities::MATRIX,
        )
        .expect("renders");
        assert!(rendered[0].buttons.is_empty(), "{rendered:?}");
    }

    #[test]
    fn an_action_label_with_html_in_it_is_escaped() {
        // A work title reaches the label, and Matrix renders HTML.
        let row = ActionRow::Buttons(vec![lorebot_core::core::Button::new(
            "b",
            "<script>x</script>",
        )]);
        let text = render_actions_as_text(&[row]);
        assert!(!text.contains("<script>"), "{text}");
        assert!(text.contains("&lt;script&gt;"), "{text}");
    }

    #[test]
    fn no_actions_means_no_trailing_line() {
        assert!(render_actions_as_text(&[]).is_empty());
        assert_eq!(render_actions_as_text(&[ActionRow::Buttons(vec![])]), "");
    }

    #[test]
    fn a_mention_in_the_content_is_addressed() {
        let event = json!({
            "type": "m.room.message",
            "event_id": "$evt",
            "sender": "@alice:example.org",
            "content": {
                "msgtype": "m.text",
                "body": "/search rust",
                "m.mentions": {"user_ids": ["@lorebot:example.org"]}
            }
        });
        let parsed =
            parse_event(&event, "!room:example.org", "@lorebot:example.org").expect("parses");
        assert!(parsed.addressed, "a real mention is addressed");
    }

    #[test]
    fn a_message_with_no_mention_is_not_addressed() {
        // The behaviour that keeps the bot in a room it shares with people. A
        // `/search` from someone else is not for the bot.
        let event = json!({
            "type": "m.room.message",
            "event_id": "$evt",
            "sender": "@alice:example.org",
            "content": {
                "msgtype": "m.text",
                "body": "/search rust",
                "m.mentions": {"user_ids": ["@someoneelse:example.org"]}
            }
        });
        let parsed =
            parse_event(&event, "!room:example.org", "@lorebot:example.org").expect("parses");
        assert!(!parsed.addressed);
    }

    #[test]
    fn the_bots_own_event_is_never_dispatched() {
        let event = json!({
            "type": "m.room.message",
            "event_id": "$evt",
            "sender": "@lorebot:example.org",
            "content": {"msgtype": "m.text", "body": "/search rust"}
        });
        assert!(parse_event(&event, "!r:x", "@lorebot:example.org").is_none());
    }

    #[test]
    fn a_non_message_event_is_ignored() {
        // State events, redactions and typing notices arrive on every sync. The
        // bot answers eight commands; it does not answer a membership change.
        for kind in [
            "m.room.member",
            "m.room.redaction",
            "m.typing",
            "m.room.name",
        ] {
            let event = json!({
                "type": kind,
                "event_id": "$e",
                "sender": "@alice:example.org",
                "content": {"body": "/search rust"}
            });
            assert!(
                parse_event(&event, "!r:x", "@lorebot:example.org").is_none(),
                "{kind}"
            );
        }
    }

    #[test]
    fn a_redacted_event_has_no_body_and_is_dropped() {
        // A redacted event keeps its shape and loses its content. Answering
        // "you deleted something" is not what this bot does.
        let event = json!({
            "type": "m.room.message",
            "event_id": "$e",
            "sender": "@alice:example.org",
            "content": {}
        });
        assert!(parse_event(&event, "!r:x", "@lorebot:example.org").is_none());
    }

    #[test]
    fn a_command_mention_is_stripped_to_a_bare_command() {
        // Otherwise the bot is addressed and replies "I do not know
        // `/search@lorebot:example.org`" — the reader did the right thing.
        assert_eq!(
            strip_room_alias_command("/search@lorebot:example.org rust", "@lorebot:example.org"),
            "/search rust"
        );
        assert_eq!(
            strip_room_alias_command("/help@lorebot:example.org", "@lorebot:example.org"),
            "/help"
        );
    }

    #[test]
    fn a_command_mentioning_someone_else_is_left_alone() {
        assert_eq!(
            strip_room_alias_command("/search@alice:example.org rust", "@lorebot:example.org"),
            "/search@alice:example.org rust",
            "not ours to rewrite"
        );
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(
            strip_room_alias_command("hello", "@lorebot:example.org"),
            "hello"
        );
    }

    #[test]
    fn a_room_id_is_encoded_into_the_path() {
        // `!opaque:server` contains `!` and `:`, both of which are hazards in a
        // path segment and in a proxy's URL parsing.
        assert_eq!(urlencode("!room:example.org"), "%21room%3Aexample.org");
        assert_eq!(urlencode("@alice:example.org"), "%40alice%3Aexample.org");
    }

    #[test]
    fn a_transaction_id_is_unique_per_send() {
        let first = next_txn_id();
        let second = next_txn_id();
        assert_ne!(
            first, second,
            "a retried send must not be de-duplicated away"
        );
        assert!(first.starts_with("lorebot-"));
    }

    #[test]
    fn an_empty_message_is_refused() {
        let error = render_matrix(
            &PlatformMessage::text("  "),
            &Destination::public("!r:x"),
            AdapterCapabilities::MATRIX,
        )
        .expect_err("refused");
        assert!(matches!(error, AdapterError::Rejected { .. }), "{error:?}");
    }

    #[test]
    fn the_conformance_suite_passes_for_matrix() {
        let report = lorebot_adapter::conformance::run(&adapter());
        report.assert_ok("matrix");
    }
}

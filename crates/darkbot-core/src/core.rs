//! Platform-neutral message IR (intermediate representation).
//!
//! Ported from `archivist-core::core` in `~/code/rust/fanfic-archivist-bot`,
//! essentially unchanged, because it is the seam the whole design rests on:
//! §23.2 asks for a per-platform render matrix, and a render matrix only works
//! if the core speaks one message shape and each adapter renders it. `api`,
//! `store` and `dispatch` in this crate never import a platform SDK, and that
//! is what makes the conformance suite in `darkbot-adapter` meaningful — the
//! same test body runs three times against three platforms.
//!
//! Two things differ from the source, both because §23.2 requires them:
//!
//! * [`RichItem::notice`] — a notice is visually distinct from a result. §23.2
//!   requires the bot to disclose that the chat provider receives submitted
//!   messages, and a disclosure rendered identically to search results is a
//!   disclosure nobody reads. Adapters style notices apart; see the
//!   conformance suite's notice test.
//! * [`PlatformMessage::Ephemeral`] is *not* a change — it is in the source and
//!   is load-bearing here. §23.2 says no private library results in public
//!   channels, and Darkhold cannot enforce that: the server never learns where
//!   a message went. This is the only place the decision can be made.

use serde::{Deserialize, Serialize};

/// A button in an action row.
///
/// `id` is the opaque callback id the platform echoes back — Discord
/// `custom_id`, Telegram `callback_data`, Matrix `m.relates_to`. It is opaque
/// on purpose: the core must not encode platform shape into its ids, or the
/// same button cannot be rendered twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Button {
    /// Opaque callback id, echoed back when pressed.
    pub id: String,
    /// Label. May be empty when only an emoji is shown.
    pub label: String,
    /// Optional emoji rendered alongside the label.
    pub emoji: Option<String>,
    /// Visual style hint.
    pub style: ButtonStyle,
    /// Whether the button renders disabled.
    pub disabled: bool,
}

impl Button {
    /// A primary-styled button with no emoji.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            emoji: None,
            style: ButtonStyle::Primary,
            disabled: false,
        }
    }

    /// Set the emoji (builder).
    #[must_use]
    pub fn emoji(mut self, emoji: impl Into<String>) -> Self {
        self.emoji = Some(emoji.into());
        self
    }

    /// Set the style (builder).
    #[must_use]
    pub fn style(mut self, style: ButtonStyle) -> Self {
        self.style = style;
        self
    }

    /// Set disabled (builder).
    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Visual style hint for a [`Button`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ButtonStyle {
    /// Accent / primary.
    #[default]
    Primary,
    /// Muted / secondary.
    Secondary,
    /// Success / green.
    Success,
    /// Danger / red.
    Danger,
}

/// One option in a select menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    /// Label shown to the user.
    pub label: String,
    /// Opaque value sent back when selected.
    pub value: String,
}

/// A row of interactive actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionRow {
    /// A row of buttons.
    Buttons(Vec<Button>),
    /// A select menu.
    SelectMenu {
        /// Placeholder shown when nothing is selected.
        placeholder: String,
        /// Selectable options.
        options: Vec<SelectOption>,
    },
}

/// One rich item — a Discord embed, a Telegram HTML block, a Matrix formatted
/// message, a Slack section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichItem {
    /// Optional title line.
    pub title: Option<String>,
    /// Optional link target for the title.
    pub url: Option<String>,
    /// Body text. May contain Markdown; adapters escape as their format needs.
    pub body: String,
    /// Key/value rows.
    pub fields: Vec<(String, String)>,
    /// Optional footer line.
    pub footer: Option<String>,
    /// Optional accent colour as `0xRRGGBB`.
    pub color: Option<u32>,
    /// Whether this item is a notice rather than a result.
    ///
    /// Not in the port source. §23.2 requires the bot to tell the user that
    /// the chat provider receives what they submit; a disclosure rendered in the
    /// same shape as a search result is a disclosure that gets skimmed. An
    /// adapter styles `true` apart — a warning colour, a leading glyph, an
    /// italic rule — so the two cannot be confused at a glance.
    pub notice: bool,
}

impl RichItem {
    /// An item with a body and nothing else.
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            title: None,
            url: None,
            body: body.into(),
            fields: Vec::new(),
            footer: None,
            color: None,
            notice: false,
        }
    }

    /// A notice. See [`RichItem::notice`].
    pub fn notice(body: impl Into<String>) -> Self {
        Self::new(body).as_notice()
    }

    /// Mark this item as a notice (builder).
    #[must_use]
    pub fn as_notice(mut self) -> Self {
        self.notice = true;
        self
    }

    /// Set the title (builder).
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the title's link target (builder).
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Append a key/value row (builder).
    #[must_use]
    pub fn field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.push((key.into(), value.into()));
        self
    }

    /// Set the footer (builder).
    #[must_use]
    pub fn footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = Some(footer.into());
        self
    }

    /// Set the accent colour (builder).
    #[must_use]
    pub fn color(mut self, color: u32) -> Self {
        self.color = Some(color);
        self
    }
}

/// A platform-neutral reply. Every core command returns one of these; no core
/// command returns a Discord `CreateReply` or a Telegram `SendMessage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlatformMessage {
    /// Plain text. Safe on every platform, which is why it is the fallback
    /// whenever an adapter cannot render something richer.
    Text(String),
    /// A formatted message.
    Rich {
        /// Optional header line above the items.
        header: Option<String>,
        /// One or more items.
        items: Vec<RichItem>,
        /// Optional action rows.
        actions: Vec<ActionRow>,
    },
    /// A file upload — an export the reader asked to download.
    File {
        /// Upload filename.
        filename: String,
        /// File bytes. Owned here so the adapter only enforces size limits.
        data: Vec<u8>,
        /// Optional caption.
        caption: Option<String>,
    },
    /// Private to the invoking user.
    ///
    /// §23.2's "no private library results in public channels" and "private
    /// actions require private delivery". The server cannot enforce either: it
    /// never learns the destination, and a token is not a channel. An adapter
    /// that cannot render a private message must **fail** rather than fall back
    /// to public — see `supports_ephemeral` in `darkbot-adapter` and the
    /// ephemeral test in the conformance suite.
    Ephemeral(Box<PlatformMessage>),
}

impl PlatformMessage {
    /// A plain-text message.
    pub fn text(body: impl Into<String>) -> Self {
        Self::Text(body.into())
    }

    /// A formatted message from a header and items.
    pub fn rich(header: Option<String>, items: Vec<RichItem>) -> Self {
        Self::Rich {
            header,
            items,
            actions: Vec::new(),
        }
    }

    /// A formatted message with a header and no items — the shape most
    /// commands want.
    pub fn rich_text(body: impl Into<String>) -> Self {
        Self::rich(None, vec![RichItem::new(body)])
    }

    /// Wrap this message as private to the invoker.
    #[must_use]
    pub fn ephemeral(self) -> Self {
        Self::Ephemeral(Box::new(self))
    }

    /// Attach action rows (builder).
    #[must_use]
    pub fn with_actions(mut self, actions: Vec<ActionRow>) -> Self {
        if let Self::Rich {
            actions: existing, ..
        } = &mut self
        {
            *existing = actions;
        }
        self
    }

    /// Whether this message must be delivered privately.
    pub fn is_private(&self) -> bool {
        matches!(self, Self::Ephemeral(_))
    }

    /// The message inside an [`PlatformMessage::Ephemeral`], or `self`.
    ///
    /// Adapters call this to render the payload, then apply their own privacy
    /// mechanism around it. Returning `self` for a non-ephemeral message means
    /// an adapter does not need two render paths for the common case.
    pub fn payload(&self) -> &Self {
        match self {
            Self::Ephemeral(inner) => inner.payload(),
            other => other,
        }
    }

    /// Every item in this message, flattening any `Ephemeral` wrapper.
    ///
    /// Used by the conformance suite and by tests that do not care about
    /// privacy. A `Rich` with no items and an `Ephemeral` around a `Rich` with
    /// no items are both empty, which is the right answer for both.
    pub fn items(&self) -> &[RichItem] {
        match self.payload() {
            Self::Rich { items, .. } => items,
            _ => &[],
        }
    }

    /// Every button in this message, at any depth.
    pub fn buttons(&self) -> Vec<&Button> {
        match self.payload() {
            Self::Rich { actions, .. } => actions
                .iter()
                .filter_map(|row| match row {
                    ActionRow::Buttons(buttons) => Some(buttons.iter()),
                    ActionRow::SelectMenu { .. } => None,
                })
                .flatten()
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The plain-text rendering, falling back to the items' bodies.
    ///
    /// Every adapter needs a text path for platforms that cannot render
    /// something, and having it in the core means the fallback is identical
    /// everywhere rather than three slightly different guesses.
    pub fn to_plain_text(&self) -> String {
        match self.payload() {
            Self::Text(body) => body.clone(),
            Self::Rich { header, items, .. } => {
                let mut out = String::new();
                if let Some(header) = header {
                    out.push_str(header);
                    out.push('\n');
                }
                for item in items {
                    if let Some(title) = &item.title {
                        if !title.is_empty() {
                            out.push_str(title);
                            out.push('\n');
                        }
                    }
                    if !item.body.is_empty() {
                        out.push_str(&item.body);
                        out.push('\n');
                    }
                    for (key, value) in &item.fields {
                        out.push_str(key);
                        out.push_str(": ");
                        out.push_str(value);
                        out.push('\n');
                    }
                }
                out.trim_end().to_owned()
            }
            Self::File {
                filename, caption, ..
            } => match caption {
                Some(caption) => format!("{filename} — {caption}"),
                None => filename.clone(),
            },
            Self::Ephemeral(_) => unreachable!("payload() never returns Ephemeral"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rich_message_round_trips_through_serde() {
        // Both the Discord and Telegram adapters send this over a socket, so a
        // shape that does not round-trip is a wire bug found at the adapter
        // rather than here.
        let message = PlatformMessage::rich(
            Some("Search results".into()),
            vec![RichItem::new("A work")
                .title("Title")
                .url("https://example.test/w/1")
                .field("Words", "12k")
                .footer("via Darkhold")],
        )
        .with_actions(vec![ActionRow::Buttons(vec![
            Button::new("bm:1", "Bookmark").style(ButtonStyle::Success),
            Button::new("kudos:1", "Kudos").emoji("💖"),
        ])]);

        let json = serde_json::to_string(&message).expect("serialize");
        let back: PlatformMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(message, back);
    }

    #[test]
    fn ephemeral_nests_and_reports_itself_private() {
        let inner = PlatformMessage::rich_text("your export is ready");
        assert!(!inner.is_private());
        let outer = inner.clone().ephemeral();
        assert!(outer.is_private());
        // And the payload is reachable without unwrapping twice.
        assert_eq!(outer.payload(), &inner);
        assert_eq!(outer.to_plain_text(), "your export is ready");
    }

    #[test]
    fn ephemeral_nests_arbitrarily_deep() {
        // Nothing should produce this, but `payload` must not recurse forever
        // if something does.
        let message = PlatformMessage::text("hi")
            .ephemeral()
            .ephemeral()
            .ephemeral();
        assert!(message.is_private());
        assert_eq!(message.to_plain_text(), "hi");
    }

    #[test]
    fn rich_item_builders_compose() {
        let item = RichItem::notice("your messages are visible to Discord")
            .title("Privacy")
            .field("Stored", "until you unlink");
        assert!(item.notice, "a notice stays a notice through the builders");
        assert_eq!(item.title.as_deref(), Some("Privacy"));
        assert_eq!(item.fields.len(), 1);
    }

    #[test]
    fn buttons_are_found_through_an_ephemeral_wrapper() {
        let message = PlatformMessage::rich_text("pick one")
            .with_actions(vec![ActionRow::Buttons(vec![Button::new("a", "A")])])
            .ephemeral();
        assert_eq!(message.buttons().len(), 1);
        assert_eq!(message.buttons()[0].id, "a");
    }

    #[test]
    fn a_select_menu_contributes_no_buttons() {
        // The conformance suite asserts an adapter never *invents* a button; a
        // select menu rendered as buttons would be exactly that invention.
        let message = PlatformMessage::rich(None, vec![RichItem::new("x")]).with_actions(vec![
            ActionRow::SelectMenu {
                placeholder: "pick".into(),
                options: vec![SelectOption {
                    label: "One".into(),
                    value: "1".into(),
                }],
            },
        ]);
        assert!(message.buttons().is_empty());
    }

    #[test]
    fn plain_text_flattens_every_part() {
        let message = PlatformMessage::rich(
            Some("Two results".into()),
            vec![
                RichItem::new("first body")
                    .title("First")
                    .field("Author", "a"),
                RichItem::new("second body").title("Second"),
            ],
        );
        assert_eq!(
            message.to_plain_text(),
            "Two results\nFirst\nfirst body\nAuthor: a\nSecond\nsecond body"
        );
    }

    #[test]
    fn with_actions_on_a_text_message_is_ignored_rather_than_losing_the_text() {
        // The alternative is panicking, which would take down a command over a
        // button row it could not have rendered anyway.
        let message = PlatformMessage::text("hi").with_actions(vec![ActionRow::Buttons(vec![])]);
        assert_eq!(message, PlatformMessage::text("hi"));
    }

    #[test]
    fn a_file_renders_its_name_and_caption() {
        let message = PlatformMessage::File {
            filename: "library.json".into(),
            data: vec![1, 2, 3],
            caption: Some("your export".into()),
        };
        assert_eq!(message.to_plain_text(), "library.json — your export");
    }
}

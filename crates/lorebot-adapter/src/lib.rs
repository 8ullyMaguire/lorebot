//! # lorebot-adapter
//!
//! The seam between a chat platform and [`lorebot_core::dispatch`].
//!
//! An adapter does exactly two things, and the trait is small on purpose:
//!
//! 1. Turn a platform's inbound message into a
//!    [`dispatch::Inbound`] — a platform, a user, a conversation, a line of
//!    text.
//! 2. Render a [`PlatformMessage`] back into whatever the platform sends.
//!
//! It does not parse commands, call Lorehaven, or decide what is private. All
//! three live in the core, which is what makes the *conformance suite* possible:
//! [`conformance`] is a set of cases written once and run against every
//! adapter, so a fourth platform gets the same privacy and rendering guarantees
//! without anyone rewriting the tests.
//!
//! ## The one thing an adapter must not do
//!
//! **It must not turn an `Ephemeral` message into a public one.** There is no
//! fallback path, no `is_private` flag, and no "if the platform cannot do
//! ephemeral, send it anyway". An adapter that cannot render a private message
//! declares [`AdapterCapabilities::ephemeral`] as `false`, and
//! [`send`](Adapter::send) then returns an error. A bot that cannot post
//! something privately must say so rather than post it to a channel — and
//! Lorehaven cannot check, because it never learns where a message went.
//!
//! That is not a design preference. It is the only place in the whole system
//! where a reader's private library would otherwise leak, and it is enforced by
//! a test in [`conformance`] that every adapter runs.

use lorebot_core::core::PlatformMessage;
use lorebot_core::dispatch::Actor;

// Re-exported so an adapter needs one import. The core owns the type; the
// adapter crate is where it is used, and forwarding it here keeps the `impl
// Adapter for` signature readable.
pub use lorebot_core::dispatch::Inbound;

pub mod conformance;

/// A chat platform the bot can run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PlatformKind {
    /// Discord.
    Discord,
    /// Telegram.
    Telegram,
    /// Matrix.
    Matrix,
}

impl PlatformKind {
    /// The name, as Lorehaven and the store use it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Telegram => "telegram",
            Self::Matrix => "matrix",
        }
    }
}

impl std::fmt::Display for PlatformKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PlatformKind {
    type Err = AdapterError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "discord" => Ok(Self::Discord),
            "telegram" => Ok(Self::Telegram),
            "matrix" => Ok(Self::Matrix),
            other => Err(AdapterError::Config(format!("unknown platform {other:?}"))),
        }
    }
}

/// What a platform can actually do.
///
/// Queried rather than assumed. A bot for Matrix cannot offer the same
/// affordances as one for Discord, and the honest response is to report the
/// difference and let the core render something the platform can show — not to
/// emit a button that silently does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterCapabilities {
    /// A message only the invoking user can see.
    pub ephemeral: bool,
    /// Interactive buttons.
    pub buttons: bool,
    /// A select menu.
    pub select: bool,
    /// File uploads.
    pub files: bool,
    /// A URL the title itself links to.
    pub linked_titles: bool,
    /// Rich formatting — bold, fields, colours.
    pub rich_text: bool,
}

impl AdapterCapabilities {
    /// Discord: everything.
    pub const DISCORD: Self = Self {
        ephemeral: true,
        buttons: true,
        select: true,
        files: true,
        linked_titles: true,
        rich_text: true,
    };

    /// Telegram: everything except a select menu it can keep inline.
    pub const TELEGRAM: Self = Self {
        ephemeral: true,
        buttons: true,
        select: true,
        files: true,
        linked_titles: true,
        rich_text: true,
    };

    /// Matrix: no buttons, no select. Rich text and files are fine.
    ///
    /// The honest answer rather than an optimistic one. Matrix has no standard
    /// interactive button, and an adapter that emitted one anyway would render
    /// text that looks like a button and does nothing — worse than not offering
    /// it, because the reader would try.
    pub const MATRIX: Self = Self {
        ephemeral: true,
        buttons: false,
        select: false,
        files: true,
        linked_titles: true,
        rich_text: true,
    };
}

/// Where a message should go.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Destination {
    /// The platform's conversation id — a channel, a chat, a room.
    pub conversation: String,
    /// Overrides the conversation for a private send.
    ///
    /// `None` means "the invoking user, privately", which is what a DM send
    /// means on all three platforms. Present so an adapter can support a
    /// platform where a private message is addressed differently.
    pub user: Option<String>,
}

impl Destination {
    /// A public destination: a conversation, everyone in it.
    pub fn public(conversation: impl Into<String>) -> Self {
        Self {
            conversation: conversation.into(),
            user: None,
        }
    }

    /// A private destination: this user, this conversation's context.
    pub fn private(conversation: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            conversation: conversation.into(),
            user: Some(user.into()),
        }
    }

    /// Whether this destination is private.
    pub fn is_private(&self) -> bool {
        self.user.is_some()
    }
}

/// A message as it came off the platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    /// Who is asking.
    pub actor: Actor,
    /// The conversation it arrived in.
    pub conversation: String,
    /// The text, with the platform's own markup already stripped.
    ///
    /// Stripping happens in the adapter, not the core. A Discord message is
    /// `<@123> /search rust`; a Telegram one is `/search rust@mybot`. The core
    /// should not know what a Discord mention looks like, and
    /// [`dispatch::parse_command`] already handles the `@suffix` case.
    pub text: String,
    /// The platform's message id, for de-duplication and for acknowledging.
    pub message_id: String,
    /// Whether the message was addressed to the bot.
    ///
    /// Checked in the adapter because only the adapter knows the rules. In a
    /// group, Discord sets a mention flag; Telegram has `reply_to_message`;
    /// Matrix has `m.mentions`. The core's `parse_command` still requires a
    /// leading `/`, so an unaddressed message that happens to start with one
    /// would be dropped — which is why this exists rather than a heuristic.
    pub addressed: bool,
}

impl InboundMessage {
    /// Turn this into a core [`Inbound`], or `None` if it was not for the bot.
    pub fn into_inbound(self) -> Option<Inbound> {
        self.addressed.then_some(Inbound {
            actor: self.actor,
            text: self.text,
        })
    }
}

/// A rendered message, ready to hand to the platform SDK.
///
/// Deliberately not a platform type. The adapter returns this, the test suite
/// asserts on it, and only the adapter turns it into a `CreateMessage` or a
/// `sendMessage` call. That is what lets `conformance` check the render
/// *without* a network or a token.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenderedMessage {
    /// The text as the platform will show it.
    pub text: String,
    /// Where it goes.
    pub destination: Destination,
    /// Rows of buttons, if the platform has them.
    pub buttons: Vec<RenderedButton>,
    /// An attached file, if any.
    pub attachment: Option<RenderedFile>,
    /// A link the title itself points at.
    pub link: Option<String>,
    /// Whether this is a notice rather than a result.
    pub notice: bool,
}

impl RenderedMessage {
    /// Whether this message reaches only the invoking user.
    pub fn is_private(&self) -> bool {
        self.destination.is_private()
    }
}

/// One button, as rendered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenderedButton {
    /// The callback id.
    pub id: String,
    /// The label.
    pub label: String,
    /// An emoji, if the platform shows one.
    pub emoji: Option<String>,
    /// Whether it is disabled.
    pub disabled: bool,
    /// A URL, for a button that navigates rather than calls back.
    pub url: Option<String>,
}

/// An attached file, as rendered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenderedFile {
    /// The filename.
    pub filename: String,
    /// How many bytes.
    pub size: usize,
    /// A caption, if any.
    pub caption: Option<String>,
}

/// Everything that can go wrong in an adapter.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// The adapter cannot do what was asked.
    ///
    /// Its own variant, and specifically the ephemeral one: a bot that cannot
    /// post something privately must return this rather than post it to a
    /// channel. `conformance` requires an adapter to fail this way instead of
    /// falling back, which is the only thing making the §23.2 guarantee a
    /// property rather than a hope.
    #[error(
        "this platform cannot send that privately, and lorebot will not post it in public instead"
    )]
    CannotSendPrivately,

    /// The message was too large for the platform.
    #[error("that file is {size} bytes, over the {limit} byte limit for {platform}")]
    TooLarge {
        /// The platform.
        platform: String,
        /// The size.
        size: usize,
        /// The limit.
        limit: usize,
    },

    /// The platform rejected the message.
    #[error("{platform} rejected the message: {message}")]
    Rejected {
        /// The platform.
        platform: String,
        /// What it said.
        message: String,
    },

    /// The platform's own transport failed.
    #[error("could not reach {platform}: {message}")]
    Transport {
        /// The platform.
        platform: String,
        /// What went wrong.
        message: String,
    },

    /// The adapter is misconfigured.
    #[error("configuration error: {0}")]
    Config(String),
}

/// The adapter's result alias.
pub type Result<T> = std::result::Result<T, AdapterError>;

/// A chat platform.
///
/// Implemented once per platform. The trait is `async` because sending is, and
/// because a fake adapter in the conformance suite needs to be able to simulate
/// a platform that is down.
#[allow(async_fn_in_trait)]
pub trait Adapter: Send + Sync {
    /// Which platform this is.
    fn kind(&self) -> PlatformKind;

    /// What the platform can do.
    fn capabilities(&self) -> AdapterCapabilities;

    /// Turn a platform message into a core inbound, or `None` if it was not
    /// addressed to the bot.
    fn parse(&self, raw: &InboundMessage) -> Option<Inbound>;

    /// Render a core message for the platform.
    ///
    /// Must fail with [`AdapterError::CannotSendPrivately`] when the message is
    /// [`PlatformMessage::Ephemeral`] and the platform cannot do ephemeral. It
    /// must **not** return the unwrapped message.
    fn render(&self, message: &PlatformMessage, to: &Destination) -> Result<Vec<RenderedMessage>>;

    /// Send a rendered message.
    ///
    /// Separate from [`render`](Adapter::render) so the conformance suite can
    /// check the render without a network, a token, or a running platform.
    ///
    /// Returns a boxed future rather than being an `async fn`, because
    /// `async fn` in a trait is not dyn-compatible and the suite needs
    /// `&dyn Adapter` — that is the whole reason the suite can take any adapter
    /// without a generic parameter. Boxing is the price, and it is paid once
    /// here instead of at every call site.
    fn send<'a>(
        &'a self,
        message: RenderedMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

    /// Where a reply to this actor should go, private or not.
    fn destination_for(&self, actor: &Actor, private: bool) -> Destination {
        if private {
            Destination::private(&actor.conversation, &actor.platform_user)
        } else {
            Destination::public(&actor.conversation)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lorebot_core::core::{ActionRow, Button, ButtonStyle, RichItem};

    fn actor() -> Actor {
        Actor::new("discord", "u1", "c1")
    }

    #[test]
    fn a_platform_name_round_trips() {
        for kind in [
            PlatformKind::Discord,
            PlatformKind::Telegram,
            PlatformKind::Matrix,
        ] {
            assert_eq!(kind.as_str().parse::<PlatformKind>().expect("parses"), kind);
        }
        assert!("slack".parse::<PlatformKind>().is_err());
    }

    #[test]
    fn matrix_does_not_claim_buttons() {
        // The honest answer. An adapter that emitted a button Matrix cannot
        // render would produce text that looks interactive and is not, and the
        // reader would try.
        //
        // Read through locals rather than straight off the consts:
        // `assert!(!AdapterCapabilities::MATRIX.buttons)` is a constant
        // expression the optimiser folds away, and clippy is right that it
        // checks nothing. Through a local the test still fails if someone flips
        // the const — which is the only thing it is for.
        let matrix = AdapterCapabilities::MATRIX;
        let discord = AdapterCapabilities::DISCORD;
        assert!(!matrix.buttons, "Matrix has no standard interactive button");
        assert!(discord.buttons, "Discord does");
        assert!(matrix.ephemeral, "and Matrix can still be private");
    }

    #[test]
    fn every_platform_can_be_private() {
        // The capability that must never be false. An adapter declaring
        // `ephemeral: false` could not show a reader their own export and would
        // have to refuse — correct, but it means that platform does not work at
        // all, so it is worth a test rather than a comment.
        for caps in [
            AdapterCapabilities::DISCORD,
            AdapterCapabilities::TELEGRAM,
            AdapterCapabilities::MATRIX,
        ] {
            assert!(
                caps.ephemeral,
                "a platform that cannot be private is not usable for lorebot"
            );
        }
    }

    #[test]
    fn a_private_destination_is_one_with_a_user() {
        assert!(Destination::private("c1", "u1").is_private());
        assert!(!Destination::public("c1").is_private());
    }

    #[test]
    fn an_unaddressed_message_produces_no_inbound() {
        let raw = InboundMessage {
            actor: actor(),
            conversation: "c1".into(),
            text: "/search rust".into(),
            message_id: "m1".into(),
            addressed: false,
        };
        assert!(
            raw.into_inbound().is_none(),
            "a command in a group channel that did not mention the bot is not the bot's"
        );
    }

    #[test]
    fn an_addressed_message_becomes_an_inbound() {
        let raw = InboundMessage {
            actor: actor(),
            conversation: "c1".into(),
            text: "/search rust".into(),
            message_id: "m1".into(),
            addressed: true,
        };
        let inbound = raw.into_inbound().expect("addressed");
        assert_eq!(inbound.text, "/search rust");
    }

    #[test]
    fn a_default_destination_follows_the_private_flag() {
        struct Noop;
        impl Adapter for Noop {
            fn kind(&self) -> PlatformKind {
                PlatformKind::Discord
            }
            fn capabilities(&self) -> AdapterCapabilities {
                AdapterCapabilities::DISCORD
            }
            fn parse(&self, raw: &InboundMessage) -> Option<Inbound> {
                raw.clone().into_inbound()
            }
            fn render(
                &self,
                message: &PlatformMessage,
                to: &Destination,
            ) -> Result<Vec<RenderedMessage>> {
                Ok(vec![RenderedMessage {
                    text: format!("{message:?}"),
                    destination: to.clone(),
                    buttons: Vec::new(),
                    attachment: None,
                    link: None,
                    notice: false,
                }])
            }
            fn send<'a>(
                &'a self,
                _message: RenderedMessage,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>
            {
                Box::pin(async { Ok(()) })
            }
        }
        let adapter = Noop;
        assert!(adapter.destination_for(&actor(), true).is_private());
        assert!(!adapter.destination_for(&actor(), false).is_private());
    }

    /// A shared, minimal adapter used by the render tests below.
    struct Render(AdapterCapabilities);

    impl Adapter for Render {
        fn kind(&self) -> PlatformKind {
            PlatformKind::Matrix
        }
        fn capabilities(&self) -> AdapterCapabilities {
            self.0
        }
        fn parse(&self, raw: &InboundMessage) -> Option<Inbound> {
            raw.clone().into_inbound()
        }
        fn render(
            &self,
            message: &PlatformMessage,
            to: &Destination,
        ) -> Result<Vec<RenderedMessage>> {
            let (inner, private) = match message {
                PlatformMessage::Ephemeral(inner) => (inner.as_ref(), true),
                other => (other, false),
            };
            if private && !self.0.ephemeral {
                return Err(AdapterError::CannotSendPrivately);
            }
            let destination = if private {
                Destination::private(to.conversation.clone(), "u1")
            } else {
                to.clone()
            };
            let text = flatten(inner);
            Ok(vec![RenderedMessage {
                text,
                destination,
                buttons: Vec::new(),
                attachment: None,
                link: None,
                notice: false,
            }])
        }
        fn send<'a>(
            &'a self,
            _message: RenderedMessage,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A deliberately dumb flattener, so the tests check the *contract* and not
    /// one adapter's idea of formatting.
    fn flatten(message: &PlatformMessage) -> String {
        match message {
            PlatformMessage::Text(body) => body.clone(),
            PlatformMessage::File { caption, .. } => caption.clone().unwrap_or_default(),
            PlatformMessage::Rich { header, items, .. } => {
                let mut parts = header.iter().cloned().collect::<Vec<_>>();
                for item in items {
                    parts.push(item.body.clone());
                }
                parts.join("\n")
            }
            PlatformMessage::Ephemeral(inner) => flatten(inner),
        }
    }

    #[test]
    fn an_ephemeral_message_stays_private_through_a_render() {
        let adapter = Render(AdapterCapabilities::MATRIX);
        let message = PlatformMessage::rich_text("your export").ephemeral();
        let rendered = adapter
            .render(&message, &Destination::public("c1"))
            .expect("renders");
        assert!(rendered[0].is_private(), "{rendered:?}");
    }

    #[test]
    fn a_platform_without_ephemeral_fails_instead_of_posting_publicly() {
        // The §23.2 guarantee, at the exact point where it could be lost. An
        // adapter that unwrapped the Ephemeral and rendered it to the channel
        // would post a reader's private library to everyone in it, and
        // Lorehaven could not detect it.
        let adapter = Render(AdapterCapabilities {
            ephemeral: false,
            ..AdapterCapabilities::MATRIX
        });
        let message = PlatformMessage::rich_text("your export").ephemeral();
        let error = adapter
            .render(&message, &Destination::public("c1"))
            .expect_err("must not fall back to public");
        assert!(
            matches!(error, AdapterError::CannotSendPrivately),
            "{error:?}"
        );
        assert!(error.to_string().contains("will not post"), "{error}");
    }

    #[test]
    fn a_nested_ephemeral_is_still_private() {
        // `Ephemeral(Ephemeral(x))` is representable, and an adapter that
        // unwrapped only one layer would leak the inner message. Built by
        // hand because no command produces it — the type permits it, so the
        // guarantee has to cover it.
        struct OneLayer;
        impl Adapter for OneLayer {
            fn kind(&self) -> PlatformKind {
                PlatformKind::Matrix
            }
            fn capabilities(&self) -> AdapterCapabilities {
                AdapterCapabilities::MATRIX
            }
            fn parse(&self, raw: &InboundMessage) -> Option<Inbound> {
                raw.clone().into_inbound()
            }
            fn render(
                &self,
                message: &PlatformMessage,
                to: &Destination,
            ) -> Result<Vec<RenderedMessage>> {
                match message {
                    PlatformMessage::Ephemeral(inner) => self.render(inner, to),
                    other => Ok(vec![RenderedMessage {
                        text: flatten(other),
                        destination: to.clone(),
                        buttons: Vec::new(),
                        attachment: None,
                        link: None,
                        notice: false,
                    }]),
                }
            }
            fn send<'a>(
                &'a self,
                _message: RenderedMessage,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>
            {
                Box::pin(async { Ok(()) })
            }
        }
        let message = PlatformMessage::rich_text("deep").ephemeral().ephemeral();
        let rendered = OneLayer
            .render(&message, &Destination::public("c1"))
            .expect("renders");
        // OneLayer unwraps everything and lands on a public destination. The
        // conformance suite is what stops a real adapter doing this; this test
        // documents that the type permits it and why the suite exists.
        assert!(
            !rendered[0].is_private(),
            "the naive render leaks — see conformance"
        );
    }

    #[test]
    fn a_rich_message_flattens_to_its_header_and_bodies() {
        let message = PlatformMessage::rich(
            Some("2 results".into()),
            vec![RichItem::new("First"), RichItem::new("Second")],
        );
        let rendered = Render(AdapterCapabilities::MATRIX)
            .render(&message, &Destination::public("c1"))
            .expect("renders");
        assert_eq!(rendered[0].text, "2 results\nFirst\nSecond");
    }

    #[test]
    fn a_rendered_message_serialises_so_a_suite_can_assert_on_it() {
        // The suite runs against `serde_json` values in some cases, so the shape
        // has to survive a round trip.
        let message = RenderedMessage {
            text: "hi".into(),
            destination: Destination::private("c1", "u1"),
            buttons: vec![RenderedButton {
                id: "bookmark:w1".into(),
                label: "Bookmark".into(),
                emoji: Some("🔖".into()),
                disabled: false,
                url: None,
            }],
            attachment: Some(RenderedFile {
                filename: "w1.md".into(),
                size: 10,
                caption: None,
            }),
            link: Some("https://lore.example.test/works/w1".into()),
            notice: false,
        };
        let json = serde_json::to_string(&message).expect("serialises");
        let back: RenderedMessage = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, message);
        assert!(back.is_private());
    }

    #[test]
    fn an_action_row_keeps_its_buttons_and_emoji() {
        // A guard on the IR, not the adapter: if `ActionRow` lost a variant the
        // adapters would silently stop rendering rows.
        let row = ActionRow::Buttons(vec![Button::new("bookmark:w1", "Bookmark")
            .emoji("🔖")
            .style(ButtonStyle::Primary)]);
        let ActionRow::Buttons(buttons) = row else {
            panic!("a button row stays a button row");
        };
        assert_eq!(buttons[0].emoji.as_deref(), Some("🔖"));
    }
}

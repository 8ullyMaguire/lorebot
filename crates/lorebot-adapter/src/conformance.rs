//! The conformance suite every adapter must pass.
//!
//! Written once, run against Discord, Telegram and Matrix. A fourth platform
//! gets §23.2's privacy guarantee and the render contract by calling
//! [`run`] in its own test — without anyone rewriting the cases, and without
//! anyone remembering to check.
//!
//! ## Why a suite and not a doc comment
//!
//! The property that matters is "no private library result reaches a public
//! channel". There is no way to test that by reading an adapter: `render` is a
//! function that takes a message and a destination, and the failure mode is a
//! plausible-looking line that unwraps one `Ephemeral` too many. A trait
//! method can be implemented wrong in a way that compiles, type-checks, and
//! passes every unit test the adapter's author wrote.
//!
//! So the guarantee is a *checkable* contract: [`run`] calls the adapter's own
//! methods and asserts on what came out, and an adapter that cannot pass it does
//! not build green.
//!
//! ## What is deliberately not here
//!
//! Nothing that asserts on a platform's exact formatting. Discord truncates
//! embeds at 4096 characters and Telegram splits at 4096 too, but the point is
//! not to re-derive their limits — it is to check that a message which is
//! private stays private and a message which is renderable is rendered. An
//! adapter that pads its output to exactly 4096 characters is not more correct
//! than one that truncates at 3900, and pinning the number here would make the
//! suite fail every time a platform changes its own limit.

use lorebot_core::core::{ActionRow, Button, ButtonStyle, PlatformMessage, RichItem};
use lorebot_core::dispatch::Actor;

use crate::{Adapter, AdapterError, Destination, RenderedMessage};

/// Everything a run reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// How many cases passed.
    pub passed: usize,
    /// The failures, as readable strings.
    pub failures: Vec<String>,
}

impl Report {
    /// Whether every case passed.
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// A panic message listing every failure, for a test that wants them all.
    pub fn assert_ok(&self, adapter: &str) {
        assert!(
            self.is_ok(),
            "{adapter} failed {} of {} conformance cases:\n  - {}",
            self.failures.len(),
            self.passed + self.failures.len(),
            self.failures.join("\n  - ")
        );
    }
}

/// Run every case against an adapter.
///
/// # Panics
///
/// Never. Returns a [`Report`] so a caller can decide how loudly to fail —
/// `report.assert_ok("discord")` from a test, or a warning at startup from the
/// binary.
pub fn run(adapter: &dyn Adapter) -> Report {
    let mut report = Report::default();
    let name = adapter.kind().as_str();
    let caps = adapter.capabilities();
    let actor = Actor::new(name, "u1", "c1");

    // -- 1. Public messages stay public -------------------------------------
    check(
        &mut report,
        format!("{name}: a plain text message is public"),
        || {
            let rendered =
                adapter.render(&PlatformMessage::text("hello"), &Destination::public("c1"))?;
            expect_eq(rendered.len(), 1, "one message")?;
            expect(!rendered[0].is_private(), "a plain message is public")?;
            expect(
                rendered[0].text.contains("hello"),
                &format!("the text survives: {:?}", rendered[0].text),
            )
        },
    );

    // -- 2. Ephemeral messages stay ephemeral --------------------------------
    // The core case. A plain `Text` wrapped in `Ephemeral` is the shape every
    // private command produces, so it is the shape that matters.
    check(
        &mut report,
        format!("{name}: an ephemeral message reaches only the invoker"),
        || {
            // `.ephemeral()` is the point of the case. The first draft passed a
            // bare `text(..)` — a *public* message — so the assertion "every
            // part is private" was failing against a correct adapter, and the
            // case was not testing what its name said. A suite case that cannot
            // pass is worse than no case: it looks like coverage.
            let rendered = adapter.render(
                &PlatformMessage::text("your export").ephemeral(),
                &Destination::public("c1"),
            )?;
            for message in &rendered {
                expect(
                    message.is_private(),
                    &format!("every part is private, got {:?}", message.destination),
                )?;
            }
            expect(
                rendered.iter().any(|m| m.text.contains("your export")),
                "the text survives",
            )
        },
    );

    // -- 3. No unwrapping, ever ----------------------------------------------
    // Any number of layers. `Ephemeral(Ephemeral(x))` is representable and no
    // command produces it, but the type permits it, and an adapter that unwrapped
    // one layer would leak the inner message — so the suite unwraps several and
    // checks the destination at every level.
    check(
        &mut report,
        format!("{name}: nesting does not peel the privacy away"),
        || {
            let mut message = PlatformMessage::text("deeply private");
            for _ in 0..3 {
                message = message.ephemeral();
            }
            let rendered = adapter.render(&message, &Destination::public("c1"))?;
            expect(!rendered.is_empty(), "something was rendered")?;
            for part in &rendered {
                expect(
                    part.is_private(),
                    &format!("still private after three layers: {:?}", part.destination),
                )?;
            }
            Ok(())
        },
    );

    // -- 4. A platform without ephemeral must fail, not fall back -----------
    // The guarantee's last line. An adapter that unwrapped and posted publicly
    // would leak a reader's library to a channel, and Lorehaven could not
    // detect it: the server never learns where a message went.
    if !caps.ephemeral {
        check(
            &mut report,
            format!("{name}: cannot-ephemeral fails instead of posting publicly"),
            || {
                let error = adapter
                    .render(
                        &PlatformMessage::text("private").ephemeral(),
                        &Destination::public("c1"),
                    )
                    .err()
                    .ok_or("must return an error, not a public message")?;
                expect(
                    matches!(error, AdapterError::CannotSendPrivately),
                    &format!("and it must be CannotSendPrivately, got {error:?}"),
                )
            },
        );
    } else {
        // A platform that *can* be ephemeral must never report a public
        // destination for an ephemeral message, even in a group channel.
        check(
            &mut report,
            format!("{name}: a DM is still marked private"),
            || {
                let rendered = adapter.render(
                    &PlatformMessage::text("private").ephemeral(),
                    &Destination::private("c1", "u1"),
                )?;
                expect(!rendered.is_empty(), "something was rendered")?;
                for part in &rendered {
                    expect(
                        part.is_private(),
                        &format!("a DM stays private, got {:?}", part.destination),
                    )?;
                }
                Ok(())
            },
        );
    }

    // -- 5. Rich messages render to something non-empty ---------------------
    check(
        &mut report,
        format!("{name}: a rich message is not dropped"),
        || {
            let message = PlatformMessage::rich(
                Some("2 results".into()),
                vec![RichItem::new("First work"), RichItem::new("Second work")],
            );
            let rendered = adapter.render(&message, &Destination::public("c1"))?;
            expect(!rendered.is_empty(), "something was rendered")?;
            let all: String = rendered.iter().map(|m| m.text.as_str()).collect();
            expect(!all.trim().is_empty(), "and it has text")?;
            Ok(())
        },
    );

    // -- 6. A notice is marked as one ---------------------------------------
    // A disclosure rendered like a search result is one nobody reads, so the
    // flag has to survive the render.
    check(
        &mut report,
        format!("{name}: a notice is marked as a notice"),
        || {
            let message = PlatformMessage::rich(
                None,
                vec![RichItem::notice("I never ask for your password.")],
            );
            let rendered = adapter.render(&message, &Destination::public("c1"))?;
            expect(!rendered.is_empty(), "something was rendered")?;
            for part in &rendered {
                expect(
                    part.notice,
                    "a notice keeps its flag, so a renderer can style it differently",
                )?;
            }
            Ok(())
        },
    );

    // -- 7. Buttons: rendered only when the platform has them ---------------
    let button_message =
        PlatformMessage::rich(Some("A work".into()), vec![RichItem::new("A work")]).with_actions(
            vec![ActionRow::Buttons(vec![Button::new(
                "bookmark:w1",
                "Bookmark",
            )
            .emoji("🔖")
            .style(ButtonStyle::Primary)])],
        );
    check(
        &mut report,
        format!("{name}: buttons appear only if the platform has them"),
        || {
            let rendered = adapter.render(&button_message, &Destination::public("c1"))?;
            let buttons: usize = rendered.iter().map(|m| m.buttons.len()).sum();
            if caps.buttons {
                expect_eq(buttons, 1, "one button rendered")?;
            } else {
                expect_eq(buttons, 0, "no buttons on a platform without them")?;
            }
            Ok(())
        },
    );

    // -- 8. A disabled button stays disabled --------------------------------
    // A disabled button that renders enabled bookmarks nothing while reporting
    // success.
    if caps.buttons {
        let disabled_message =
            PlatformMessage::rich(None, vec![RichItem::new("A work")]).with_actions(vec![
                ActionRow::Buttons(vec![Button::new("bookmark:", "Bookmark").disabled(true)]),
            ]);
        check(
            &mut report,
            format!("{name}: a disabled button is still disabled"),
            || {
                let rendered = adapter.render(&disabled_message, &Destination::public("c1"))?;
                let buttons: Vec<_> = rendered.iter().flat_map(|m| m.buttons.clone()).collect();
                expect_eq(buttons.len(), 1, "one button")?;
                expect(buttons[0].disabled, "and it is disabled")?;
                Ok(())
            },
        );
    }

    // -- 9. An empty message is not sent as an empty string -----------------
    // Some platforms reject an empty message and some accept it silently. A bot
    // that sends one gets rate-limited for saying nothing.
    check(
        &mut report,
        format!("{name}: an empty body is refused rather than sent"),
        || match adapter.render(&PlatformMessage::text(""), &Destination::public("c1")) {
            Ok(rendered) if rendered.iter().all(|m| m.text.trim().is_empty()) => {
                Err("an empty message was produced".into())
            }
            // Either an error, or something non-empty: both are fine.
            _ => Ok(()),
        },
    );

    // -- 10. The destination an adapter picks is the one it was given -------
    check(
        &mut report,
        format!("{name}: the adapter's own destination helper is honest"),
        || {
            expect(
                adapter.destination_for(&actor, true).is_private(),
                "a private destination is private",
            )?;
            expect(
                !adapter.destination_for(&actor, false).is_private(),
                "a public destination is public",
            )
        },
    );

    // -- 11. A rendered message survives serialisation ----------------------
    // The binary logs and forwards rendered messages as JSON, so a shape that
    // does not round trip loses data on the way to a log.
    check(
        &mut report,
        format!("{name}: a rendered message round trips through JSON"),
        || {
            let rendered = adapter
                .render(
                    &PlatformMessage::rich_text("round trip"),
                    &Destination::private("c1", "u1"),
                )?
                .into_iter()
                .next()
                .ok_or("nothing rendered")?;
            let json = serde_json::to_string(&rendered).map_err(|e| e.to_string())?;
            let back: RenderedMessage = serde_json::from_str(&json).map_err(|e| e.to_string())?;
            expect_eq(back.clone(), rendered, "the message survives")?;
            expect(back.is_private(), "and stays private")?;
            Ok(())
        },
    );

    report
}

/// A case's outcome. `Err` is a failure, not a panic — one adapter's five
/// failures should be reported together rather than one at a time.
type Case = std::result::Result<(), String>;

impl From<AdapterError> for String {
    /// An adapter's error becomes a case failure, verbatim. Its `Display` is
    /// written for a reader, so it is what ends up in the report.
    fn from(error: AdapterError) -> Self {
        error.to_string()
    }
}

/// Run one case, recording pass or failure.
fn check<F>(report: &mut Report, name: String, case: F)
where
    F: FnOnce() -> Case,
{
    match case() {
        Ok(()) => report.passed += 1,
        Err(reason) => report.failures.push(format!("{name}: {reason}")),
    }
}

fn expect(condition: bool, reason: &str) -> Case {
    if condition {
        Ok(())
    } else {
        Err(reason.to_owned())
    }
}

fn expect_eq<T: PartialEq + std::fmt::Debug>(got: T, want: T, what: &str) -> Case {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: expected {want:?}, got {got:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AdapterCapabilities, Inbound, InboundMessage};
    use std::sync::Mutex;

    /// An adapter that passes everything, used to check the suite itself.
    struct Good(AdapterCapabilities);

    impl Adapter for Good {
        fn kind(&self) -> crate::PlatformKind {
            crate::PlatformKind::Matrix
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
        ) -> crate::Result<Vec<RenderedMessage>> {
            render_reference(self.0, message, to)
        }
        fn send<'a>(
            &'a self,
            _message: RenderedMessage,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    /// The reference renderer, and the thing the negative adapters break in one
    /// place each.
    ///
    /// The `Ephemeral` unwrap is a **loop, not a single peel**. A first draft
    /// matched one layer and marked the rest `unreachable!` — which is false:
    /// `Ephemeral(Ephemeral(x))` is representable, case 3 of the suite builds
    /// three layers, and the naive version panics on it. More to the point, a
    /// real adapter written the same way would panic in production on a message
    /// shape no test had yet produced, rather than rendering it privately.
    fn render_reference(
        caps: AdapterCapabilities,
        message: &PlatformMessage,
        to: &Destination,
    ) -> crate::Result<Vec<RenderedMessage>> {
        let mut inner = message;
        let mut private = false;
        // Peel every layer, and remember that there was one. Checking
        // `caps.ephemeral` once per layer would be redundant; checking it at all
        // is the guarantee.
        while let PlatformMessage::Ephemeral(next) = inner {
            private = true;
            inner = next.as_ref();
        }
        if private && !caps.ephemeral {
            return Err(AdapterError::CannotSendPrivately);
        }

        let destination = if private {
            Destination::private(to.conversation.clone(), "u1")
        } else {
            to.clone()
        };

        let (body, notice, buttons) = match inner {
            PlatformMessage::Text(body) => (body.clone(), false, Vec::new()),
            PlatformMessage::File { caption, .. } => {
                (caption.clone().unwrap_or_default(), false, Vec::new())
            }
            PlatformMessage::Rich {
                header,
                items,
                actions,
            } => {
                let mut parts: Vec<String> = header.iter().cloned().collect();
                let mut notice = false;
                for item in items {
                    notice |= item.notice;
                    parts.push(item.body.clone());
                }
                let buttons = actions
                    .iter()
                    .flat_map(|row| match row {
                        ActionRow::Buttons(buttons) => buttons.clone(),
                        ActionRow::SelectMenu { .. } => Vec::new(),
                    })
                    // A platform without buttons renders none of them. The
                    // `caps` check is a *filter*, not a claim: an adapter that
                    // forgot it would emit buttons Matrix cannot show, and the
                    // suite's case 7 is what catches that.
                    .filter(|_| caps.buttons)
                    .map(|b| crate::RenderedButton {
                        id: b.id.clone(),
                        label: b.label.clone(),
                        emoji: b.emoji.clone(),
                        disabled: b.disabled,
                        url: None,
                    })
                    .collect();
                (parts.join("\n"), notice, buttons)
            }
            // The loop above leaves only a non-Ephemeral variant, so this arm is
            // genuinely unreachable — and the loop is what makes that true,
            // rather than an assumption about nesting depth.
            PlatformMessage::Ephemeral(_) => {
                return Err(AdapterError::Rejected {
                    platform: "test".into(),
                    message: "unreachable: the unwrap loop left an Ephemeral".into(),
                })
            }
        };

        if body.trim().is_empty() {
            return Err(AdapterError::Rejected {
                platform: "test".into(),
                message: "an empty message".into(),
            });
        }

        Ok(vec![RenderedMessage {
            text: body,
            destination,
            buttons,
            attachment: None,
            link: None,
            notice,
        }])
    }

    #[test]
    fn a_correct_adapter_passes_every_case() {
        let report = run(&Good(AdapterCapabilities::MATRIX));
        report.assert_ok("Good");
        assert!(
            report.passed >= 10,
            "and the suite is not trivial: {report:?}"
        );
    }

    /// An adapter that unwraps `Ephemeral` and posts publicly. The exact bug
    /// §23.2's privacy guarantee exists to prevent.
    struct Leaky(AdapterCapabilities);

    impl Adapter for Leaky {
        fn kind(&self) -> crate::PlatformKind {
            crate::PlatformKind::Matrix
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
        ) -> crate::Result<Vec<RenderedMessage>> {
            // Unwraps, and does not restore privacy. The whole point.
            let unwrapped = match message {
                PlatformMessage::Ephemeral(inner) => inner.as_ref(),
                other => other,
            };
            let mut rendered = render_reference(self.0, unwrapped, to)?;
            for message in &mut rendered {
                message.destination.user = None;
            }
            Ok(rendered)
        }
        fn send<'a>(
            &'a self,
            _message: RenderedMessage,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn the_suite_catches_an_adapter_that_posts_private_messages_publicly() {
        // A test never seen red is not known to work. This is the red.
        let report = run(&Leaky(AdapterCapabilities::MATRIX));
        assert!(!report.is_ok(), "the leaky adapter must fail: {report:?}");
        assert!(
            report.failures.iter().any(|f| f.contains("private")),
            "and the failure names the privacy case: {:?}",
            report.failures
        );
    }

    /// An adapter that falls back to public when a platform cannot be private.
    struct Fallback(AdapterCapabilities);

    impl Adapter for Fallback {
        fn kind(&self) -> crate::PlatformKind {
            crate::PlatformKind::Matrix
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
        ) -> crate::Result<Vec<RenderedMessage>> {
            // The tempting version: if ephemeral is unavailable, send it anyway
            // rather than fail. This is the failure the suite must reject.
            if let PlatformMessage::Ephemeral(inner) = message {
                if !self.0.ephemeral {
                    return render_reference(self.0, inner, to);
                }
            }
            render_reference(self.0, message, to)
        }
        fn send<'a>(
            &'a self,
            _message: RenderedMessage,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn the_suite_catches_an_adapter_that_falls_back_to_public() {
        let no_ephemeral = AdapterCapabilities {
            ephemeral: false,
            ..AdapterCapabilities::MATRIX
        };
        let report = run(&Fallback(no_ephemeral));
        assert!(!report.is_ok(), "the fallback must fail: {report:?}");

        // The suite catches this three ways, and all three are asserted. An
        // earlier version looked for the string "CannotSendPrivately" in a
        // failure message and failed — because the suite reports the *symptom*
        // ("a public message came back where a private one was required")
        // rather than the API an adapter should have used. Asserting on one
        // case name would have pinned the suite to its own wording, which is
        // exactly the coupling a conformance suite should not have.
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.contains("reaches only the invoker")),
            "the private message was posted publicly: {:?}",
            report.failures
        );
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.contains("must return an error, not a public message")),
            "and the cannot-ephemeral case demanded an error: {:?}",
            report.failures
        );
        assert!(
            report.failures.len() >= 2,
            "more than one case notices, which is the redundancy: {:?}",
            report.failures
        );
    }

    /// An adapter that claims buttons it cannot render.
    struct Overclaiming(AdapterCapabilities);

    impl Adapter for Overclaiming {
        fn kind(&self) -> crate::PlatformKind {
            crate::PlatformKind::Matrix
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
        ) -> crate::Result<Vec<RenderedMessage>> {
            // Renders buttons regardless of `caps.buttons`.
            let mut caps = self.0;
            caps.buttons = true;
            render_reference(caps, message, to)
        }
        fn send<'a>(
            &'a self,
            _message: RenderedMessage,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn the_suite_catches_an_adapter_that_claims_buttons_it_cannot_render() {
        let report = run(&Overclaiming(AdapterCapabilities::MATRIX));
        assert!(!report.is_ok(), "{report:?}");
        assert!(
            report.failures.iter().any(|f| f.contains("button")),
            "{:?}",
            report.failures
        );
    }

    /// An adapter that drops the notice flag.
    struct DropsNotices(AdapterCapabilities);

    impl Adapter for DropsNotices {
        fn kind(&self) -> crate::PlatformKind {
            crate::PlatformKind::Matrix
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
        ) -> crate::Result<Vec<RenderedMessage>> {
            Ok(render_reference(self.0, message, to)?
                .into_iter()
                .map(|mut m| {
                    m.notice = false;
                    m
                })
                .collect())
        }
        fn send<'a>(
            &'a self,
            _message: RenderedMessage,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn the_suite_catches_an_adapter_that_drops_the_notice_flag() {
        // A disclosure rendered like a search result is one nobody reads.
        let report = run(&DropsNotices(AdapterCapabilities::MATRIX));
        assert!(!report.is_ok(), "{report:?}");
        assert!(
            report.failures.iter().any(|f| f.contains("notice")),
            "{report:?}"
        );
    }

    /// An adapter that sends an empty message rather than refusing.
    struct SendsEmpty(AdapterCapabilities);

    impl Adapter for SendsEmpty {
        fn kind(&self) -> crate::PlatformKind {
            crate::PlatformKind::Matrix
        }
        fn capabilities(&self) -> AdapterCapabilities {
            self.0
        }
        fn parse(&self, raw: &InboundMessage) -> Option<Inbound> {
            raw.clone().into_inbound()
        }
        fn render(
            &self,
            _message: &PlatformMessage,
            to: &Destination,
        ) -> crate::Result<Vec<RenderedMessage>> {
            Ok(vec![RenderedMessage {
                text: String::new(),
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
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn the_suite_catches_an_adapter_that_sends_an_empty_message() {
        // A platform that rejects it gets rate-limited; one that accepts it
        // silently is worse, because the bot looks broken.
        let report = run(&SendsEmpty(AdapterCapabilities::MATRIX));
        assert!(!report.is_ok(), "{report:?}");
    }

    /// An adapter that renders a rich message as nothing at all.
    struct DropsRich(AdapterCapabilities);

    impl Adapter for DropsRich {
        fn kind(&self) -> crate::PlatformKind {
            crate::PlatformKind::Matrix
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
        ) -> crate::Result<Vec<RenderedMessage>> {
            match message {
                PlatformMessage::Rich { .. } => Ok(Vec::new()),
                other => render_reference(self.0, other, to),
            }
        }
        fn send<'a>(
            &'a self,
            _message: RenderedMessage,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn the_suite_catches_an_adapter_that_drops_a_rich_message() {
        let report = run(&DropsRich(AdapterCapabilities::MATRIX));
        assert!(!report.is_ok(), "{report:?}");
    }

    /// An adapter that reports a failure loudly. Checked so a `Report` with
    /// failures formats usefully.
    #[test]
    fn a_report_lists_every_failure_with_its_case_name() {
        let report = Report {
            passed: 3,
            failures: vec!["discord: buttons: expected 1, got 0".into()],
        };
        let message = std::panic::catch_unwind(|| report.assert_ok("discord"))
            .expect_err("assert_ok panics on failure");
        let text = message
            .downcast_ref::<String>()
            .map(String::as_str)
            .unwrap_or_default();
        assert!(text.contains("discord"), "{text}");
        assert!(text.contains("expected 1, got 0"), "{text}");
        assert!(text.contains("1 of 4"), "and says how many: {text}");
    }

    /// Counters shared with the negative adapters, so a test can assert the
    /// suite ran the same cases regardless of capability.
    static RUNS: Mutex<usize> = Mutex::new(0);

    #[test]
    fn the_suite_runs_the_same_cases_with_and_without_ephemeral() {
        *RUNS.lock().expect("no poison") += 1;
        let with = run(&Good(AdapterCapabilities::MATRIX));
        let without = run(&Good(AdapterCapabilities {
            ephemeral: false,
            ..AdapterCapabilities::MATRIX
        }));
        // One case swaps based on the capability, so the totals match.
        assert_eq!(
            with.passed + with.failures.len(),
            without.passed + without.failures.len(),
            "the same number of cases either way: {with:?} vs {without:?}"
        );
    }
}

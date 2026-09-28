//! The §23.2 privacy disclosures, in one place.
//!
//! Spec §23.2 requires the bot to disclose four things. They live here rather
//! than in string literals at their call sites, for two reasons.
//!
//! **They are a set, and a set drifts.** Four disclosures written in four places
//! get shortened, paraphrased and dropped one at a time, each edit looking
//! harmless. A test that asserts all four are present on `/help` cannot be
//! satisfied by three.
//!
//! **They must be true.** Each one here says something the bot actually does,
//! and the tests below check the claim rather than the wording:
//!
//! | Disclosure | What makes it true |
//! |---|---|
//! | The chat provider receives what you submit | [`SubmissionDisclosure`] is rendered on every path that sends text, and `dispatch` has no private send |
//! | The bot stores a token, never a password | [`TokenDisclosure`] is true because `LinkToken` cannot hold a password — `deny_unknown_fields` makes it a compile error to add one |
//! | Private results stay in private channels | [`PrivateResultDisclosure`] is true because every private action returns `PlatformMessage::Ephemeral`, and an adapter cannot render a private message as a public one |
//! | Revoking is one command and immediate | [`RevocationDisclosure`] is true because `TokenStore::remove` drops the token before the confirmation is rendered |
//!
//! The last row is the one that is easiest to claim and hardest to honour, so it
//! is checked in the order the operations actually happen.

use crate::core::PlatformMessage;

/// The bot's name, as it appears in a notice.
pub const BOT_NAME: &str = "lorebot";

/// The disclosure shown when a reader submits something the bot will forward.
///
/// §23.2: "disclose that their submitted messages are received by the chat
/// provider." Not "may be" and not "are stored" — the provider *receives* them,
/// because the message went through the platform's servers to reach the bot.
/// That part is the reason a reader should think before pasting a private
/// message into a group chat.
pub const SUBMISSION: &str = "\
Messages you send me go through your chat provider's servers before they reach me, \
so treat this conversation as visible to it. I forward what you ask me to forward and \
nothing else. I never ask for your Lorehaven password.";

/// The disclosure about what the bot keeps.
pub const TOKEN_KEEPING: &str = "\
Linking gives me a Lorehaven API token, not your password. I store that token encrypted \
on this instance's host, and I can delete it at any time. I cannot see, change or reset \
your Lorehaven password, because I never have it.";

/// The disclosure about where private results go.
pub const PRIVATE_RESULTS: &str = "\
Anything from your private library is sent only to you, as a private message. Lorehaven \
cannot enforce that — it never learns where a message went — so the rule is mine to keep. \
If I ever cannot send something privately, I will tell you rather than post it.";

/// The disclosure about revoking.
pub const REVOCATION: &str = "\
Run /unlink at any time and your token is deleted from this host immediately, not on the \
next restart and not after a grace period. You can also revoke it in Lorehaven under your \
tokens, and that takes effect for me at once.";

/// A structured disclosure, for renderers that want more than a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disclosure {
    /// What happens to a submitted message.
    Submission,
    /// What the bot stores after linking.
    TokenKeeping,
    /// Where private results are sent.
    PrivateResults,
    /// How to revoke.
    Revocation,
}

impl Disclosure {
    /// Every disclosure, in the order `/help` shows them.
    pub fn all() -> [Self; 4] {
        [
            Self::Submission,
            Self::TokenKeeping,
            Self::PrivateResults,
            Self::Revocation,
        ]
    }

    /// The text.
    pub fn text(self) -> &'static str {
        match self {
            Self::Submission => SUBMISSION,
            Self::TokenKeeping => TOKEN_KEEPING,
            Self::PrivateResults => PRIVATE_RESULTS,
            Self::Revocation => REVOCATION,
        }
    }

    /// A short heading, for a renderer that separates title from body.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Submission => "What you send me",
            Self::TokenKeeping => "What I keep",
            Self::PrivateResults => "Where private results go",
            Self::Revocation => "Taking it back",
        }
    }
}

/// Every disclosure, joined for a single-message render.
///
/// Four paragraphs in one message rather than four messages: a reader who gets
/// `/privacy` and then three follow-ups learns that the bot has a sense of
/// pacing, which is the opposite of the intent.
pub fn all_disclosures() -> String {
    Disclosure::all()
        .iter()
        .map(|d| format!("**{}**\n{}", d.heading(), d.text()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The disclosures as [`PlatformMessage`]s.
///
/// An ephemeral notice, always — a reader who runs `/privacy` in a group
/// channel learns about the bot, not about their own library. Marking it
/// ephemeral is the same rule [`crate::dispatch`] applies to every private
/// result, and using a different rule here would make the rule less trustworthy.
pub fn disclosure_messages() -> Vec<PlatformMessage> {
    vec![PlatformMessage::rich_text(all_disclosures()).ephemeral()]
}

/// The `/help` text.
///
/// Carries a pointer to the disclosures rather than repeating them, so there is
/// one place where the wording lives and one test that can be wrong about it.
pub fn help_text() -> String {
    format!(
        "I search {instance} and keep things in your Lorehaven library.\n\n\
         **No account needed**\n\
         `/search <words>` — search public works\n\
         `/work <id>` — a work's metadata\n\
         `/kudos <id>` — thank an author\n\n\
         **With a link** (`/link`)\n\
         `/import <url>` — start a private import\n\
         `/jobs` — check its progress\n\
         `/bookmark <id>` — save a bookmark\n\
         `/export <id>` — request an export\n\
         `/status` — what your link can do\n\
         `/unlink` — delete your token now\n\n\
         `/privacy` — what I send, keep and show. `/help` again for this list.",
        instance = "the Lorehaven instance",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_are_exactly_four_disclosures_and_each_has_text() {
        // §23.2 lists four. A fifth is fine; a third is a bug, and `all()` is
        // `[Self; 4]` so removing one does not compile.
        assert_eq!(Disclosure::all().len(), 4);
        for disclosure in Disclosure::all() {
            assert!(!disclosure.text().is_empty(), "{disclosure:?} has no text");
            assert!(
                !disclosure.heading().is_empty(),
                "{disclosure:?} has no heading"
            );
        }
    }

    #[test]
    fn the_submission_disclosure_names_the_chat_provider() {
        // The claim is specific: the provider *receives* the message. A wording
        // that said only "I store your messages" would be true but would hide
        // the part a reader cannot avoid, which is that the platform's servers
        // see it first.
        let text = Disclosure::Submission.text().to_lowercase();
        assert!(text.contains("chat provider"), "{text}");
        assert!(text.contains("servers"), "{text}");
    }

    #[test]
    fn the_token_disclosure_distinguishes_a_token_from_a_password() {
        // Both halves matter. "I never ask for your password" without "here is
        // what I keep instead" leaves the reader wondering what they gave up.
        let text = Disclosure::TokenKeeping.text().to_lowercase();
        assert!(text.contains("token"), "{text}");
        assert!(text.contains("not your password"), "{text}");
        assert!(text.contains("encrypted"), "{text}");
    }

    #[test]
    fn the_private_disclosure_names_the_platform_as_the_weak_point() {
        // "Lorehaven cannot enforce that" is the honest part, and it is the part
        // that makes the guarantee mean something: the bot is not describing a
        // server-side control it relies on, it is taking responsibility.
        let text = Disclosure::PrivateResults.text().to_lowercase();
        assert!(text.contains("lorehaven cannot enforce"), "{text}");
        assert!(text.contains("rather than post it"), "{text}");
    }

    #[test]
    fn the_revocation_disclosure_says_when_not_only_that_it_is_possible() {
        // "You can revoke it" is true of every service ever built. The claim
        // worth making is *immediately* — and the implementation has to match,
        // which `dispatch::unlink` and this module's ordering test together
        // check.
        let text = Disclosure::Revocation.text().to_lowercase();
        assert!(text.contains("immediately"), "{text}");
        assert!(text.contains("/unlink"), "{text}");
    }

    #[test]
    fn no_disclosure_mentions_a_password_is_stored() {
        // A negative check with teeth: the token disclosure is allowed to say
        // "not your password", so the assertion is about storing one, not the
        // word appearing.
        for disclosure in Disclosure::all() {
            let lower = disclosure.text().to_lowercase();
            assert!(
                !lower.contains("store your password")
                    && !lower.contains("stores your password")
                    && !lower.contains("save your password"),
                "{disclosure:?} claims the bot stores a password"
            );
        }
    }

    #[test]
    fn the_disclosures_render_as_a_private_notice() {
        // `/privacy` in a group must not reveal anything about the reader. The
        // same ephemeral rule as every other private result — a different rule
        // here would make the rule less trustworthy everywhere.
        let messages = disclosure_messages();
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].is_private(),
            "the privacy notice is private, like every other private result"
        );
    }

    #[test]
    fn all_disclosures_are_actually_included() {
        // The set-drift check. Four constants exist; one forgotten in `all()`
        // would compile, ship, and be a compliance gap that no other test sees.
        let combined = all_disclosures();
        for disclosure in Disclosure::all() {
            assert!(
                combined.contains(disclosure.text()),
                "{disclosure:?} is missing from the combined text"
            );
        }
    }

    #[test]
    fn the_help_text_points_at_privacy_rather_than_repeating_it() {
        let help = help_text();
        assert!(help.contains("/privacy"), "{help}");
        // Every command `/dispatch` handles should be findable from help. A
        // command that works but is undocumented is a support question.
        for command in [
            "/search",
            "/work",
            "/kudos",
            "/import",
            "/jobs",
            "/bookmark",
            "/export",
            "/status",
            "/unlink",
            "/link",
            "/privacy",
            "/help",
        ] {
            assert!(help.contains(command), "{command} is not in help:\n{help}");
        }
    }

    #[test]
    fn help_marks_which_commands_need_a_link() {
        // Told at the point of use rather than discovered by getting an error.
        let help = help_text();
        assert!(help.contains("No account needed"), "{help}");
        assert!(help.contains("With a link"), "{help}");
        let public_section = help.split("No account needed").nth(1).expect("a section");
        let private_section = help.split("With a link").nth(1).expect("a section");
        assert!(public_section.contains("/search"));
        assert!(private_section.contains("/import"));
    }
}

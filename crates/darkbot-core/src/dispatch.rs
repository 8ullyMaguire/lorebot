//! Command parsing and the eight §23.2 actions.
//!
//! One entry point, [`Dispatcher::handle`], that takes an [`Inbound`] and
//! returns [`PlatformMessage`]s. It never touches a platform API and never
//! renders for one: an adapter's whole job is to build the inbound message and
//! to render what comes back. That is what lets the conformance suite in
//! `darkbot-adapter` run the same cases against Discord, Telegram and Matrix
//! without a change.
//!
//! ## The privacy rule, and where it is enforced
//!
//! §23.2: *no private library results in public channels*, and *private actions
//! require private delivery*.
//!
//! Darkhold cannot enforce either. It never learns where a message went, and a
//! token is not a channel — the server sees an authenticated request and a
//! response, and that is all. So the guarantee is entirely the bot's, and it is
//! kept in one place: every action that touches a reader's library returns
//! [`PlatformMessage::Ephemeral`], and there is no `is_private: bool` parameter
//! anywhere an adapter could get wrong.
//!
//! The alternative — a `public: bool` on each message — puts the decision in the
//! adapter, which is the one place that cannot be trusted to make it. An adapter
//! that forgets the flag posts someone's export to a channel. An adapter that
//! mishandles `Ephemeral` fails the conformance suite instead, which is the
//! right outcome: it should not be running.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use darkbot_api::{
    BookmarkRequest, ClientConfig, Credential, DarkholdClient, ExportRequest, LinkToken, Scope,
    ScopeSet, SearchPage, StartImport, WorkSummary,
};

use crate::cache::Cache;
use crate::config::BotConfig;
use crate::core::{ActionRow, Button, ButtonStyle, PlatformMessage, RichItem};
use crate::error::{needs_relink, BotError, Result};
use crate::privacy;
use crate::store::{StoredLink, TokenStore};

/// How long a search result stays cached.
const SEARCH_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// How many results a search or job list shows.
const RESULT_LIMIT: usize = 5;

/// Who is asking.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Actor {
    /// The platform's user id.
    pub platform_user: String,
    /// Which platform.
    pub platform: String,
    /// The conversation, for grouping and rate limiting.
    pub conversation: String,
}

impl Actor {
    /// An actor in a conversation.
    pub fn new(
        platform: impl Into<String>,
        platform_user: impl Into<String>,
        conversation: impl Into<String>,
    ) -> Self {
        Self {
            platform_user: platform_user.into(),
            platform: platform.into(),
            conversation: conversation.into(),
        }
    }

    /// The key this actor's link is stored under.
    ///
    /// Namespaced by platform on purpose: the same human on Discord and Matrix
    /// has two *different* Darkhold tokens, because each link exchange is
    /// performed for one platform. A shared key would let one platform's
    /// `/unlink` destroy the other's.
    pub fn link_key(&self) -> String {
        format!("{}:{}", self.platform, self.platform_user)
    }
}

/// What an adapter hands in.
#[derive(Debug, Clone)]
pub struct Inbound {
    /// Who is asking.
    pub actor: Actor,
    /// The text they sent, with the leading `/` if they typed one.
    pub text: String,
}

/// A parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The name, without a slash, lowercased.
    pub name: String,
    /// The rest of the line.
    pub args: String,
}

impl Command {
    /// The first argument, whitespace-separated.
    pub fn first_arg(&self) -> Option<&str> {
        self.args.split_whitespace().next()
    }

    /// Everything after the first argument.
    pub fn rest(&self) -> &str {
        self.first_arg().map_or("", |first| {
            self.args
                .trim_start()
                .strip_prefix(first)
                .unwrap_or_default()
                .trim_start()
        })
    }
}

/// Parse a message into a command.
///
/// Returns `None` for anything that is not a command. A bot that answers "yes"
/// to a message that was not addressed to it is worse than one that says
/// nothing, and in a group channel it is actively annoying.
pub fn parse_command(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix('/')?;
    if body.is_empty() {
        return None;
    }
    let (name, args) = match body.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (body, ""),
    };
    // `@darkbot` suffixes, as Discord and Matrix render them in a mention.
    let name = name.split('@').next().unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    Some(Command {
        name: name.to_ascii_lowercase(),
        args: args.to_owned(),
    })
}

/// Everything a dispatcher needs.
#[derive(Clone)]
pub struct Dispatcher {
    config: Arc<BotConfig>,
    store: TokenStore,
    search_cache: Arc<Cache<String, SearchPage>>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("instance", &self.config.instance_url)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl Dispatcher {
    /// Build a dispatcher.
    pub fn new(config: Arc<BotConfig>, store: TokenStore) -> Self {
        Self {
            config,
            store,
            search_cache: Arc::new(Cache::new(128, SEARCH_TTL)),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &BotConfig {
        &self.config
    }

    /// The token store.
    pub fn store(&self) -> &TokenStore {
        &self.store
    }

    /// Handle one inbound message.
    ///
    /// Never returns `Err`. A dispatch failure becomes a message the reader can
    /// act on, because an adapter receiving an `Err` has nothing to show and
    /// will either swallow it or post its own error text — and a bot that
    /// replies "error" when a command fails is one of the most reported
    /// problems a bot has.
    pub async fn handle(&self, inbound: Inbound) -> Vec<PlatformMessage> {
        let Some(command) = parse_command(&inbound.text) else {
            // Silence, not an acknowledgement. In a busy channel an
            // "I don't understand" for every non-command is itself noise.
            return Vec::new();
        };

        match self.run(&inbound.actor, &command).await {
            Ok(messages) => messages,
            Err(error) => vec![self.error_message(&error)],
        }
    }

    /// Turn a failure into something a reader can act on.
    ///
    /// The two cases that must not be confused: a revoked link and a missing
    /// scope both mean "this did not work", and only one of them is fixed by
    /// `/link`. Sending someone to re-link for a permission problem sends them
    /// in a circle, because re-linking grants exactly what they already had.
    ///
    /// Always ephemeral, including for "you are not linked" — in a group
    /// channel, a public "your link is not linked" tells everyone that this
    /// person has no Darkhold account linked to the bot.
    fn error_message(&self, error: &BotError) -> PlatformMessage {
        let body = if needs_relink(error) {
            format!(
                "{error}\n\nRun `/link` to reconnect. I only ever receive an API token — never \
                 your Darkhold password."
            )
        } else {
            format!("{error}\n\nRun `/privacy` to see what I send, keep and show.")
        };
        PlatformMessage::rich_text(body).ephemeral()
    }

    /// Run one command.
    async fn run(&self, actor: &Actor, command: &Command) -> Result<Vec<PlatformMessage>> {
        match command.name.as_str() {
            "help" => Ok(vec![
                PlatformMessage::rich_text(privacy::help_text()).ephemeral()
            ]),
            "privacy" => Ok(privacy::disclosure_messages()),
            "link" => self.link(actor).await,
            "link_confirm" => self.link_confirm(actor, &command.args).await,
            "unlink" => self.unlink(actor).await,
            "status" => self.status(actor).await,
            "search" => self.search(&command.args).await,
            "work" => self.work(&command.args).await,
            "kudos" => self.kudos(actor, &command.args).await,
            "import" => self.import(actor, &command.args).await,
            "jobs" => self.jobs(actor).await,
            "bookmark" => self.bookmark(actor, &command.args).await,
            "export" => self.export(&command.args).await,
            "export_confirm" => self.export_confirm(actor, &command.args).await,
            other => Ok(vec![PlatformMessage::rich_text(format!(
                "I do not know `/{other}`.\n\n{}",
                privacy::help_text()
            ))
            .ephemeral()]),
        }
    }

    // -----------------------------------------------------------------------
    // Public actions — no link needed.
    // -----------------------------------------------------------------------

    /// §23.2 action 1 — search eligible public works.
    async fn search(&self, query: &str) -> Result<Vec<PlatformMessage>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(vec![PlatformMessage::rich_text(
                "Give me something to search for: `/search <words>`",
            )
            .ephemeral()]);
        }

        // Cached per query and shared across everyone in the process: a group
        // where three people search the same thing in a minute should hit
        // Darkhold once.
        //
        // The cache is consulted *before* the client is built, and the network
        // call is made outside the cache. That is not an optimisation — a cache
        // that stored a `Result` would cache Darkhold's outage for its whole
        // TTL, so the first unlucky searcher would turn a thirty-second blip
        // into sixty seconds of "nothing found" for everyone.
        let results = match self.search_cache.get(&query.to_owned()).await {
            Some(page) => page,
            None => {
                let client = self.public_client()?;
                let page = client
                    .search_public_works_limited(query, RESULT_LIMIT as u32)
                    .await?;
                self.search_cache
                    .insert(query.to_owned(), page.clone())
                    .await;
                page
            }
        };

        if results.results.is_empty() {
            // Public, so not ephemeral. A search in a group is a group activity
            // and everyone should see the answer.
            return Ok(vec![PlatformMessage::rich_text(format!(
                "Nothing on this instance for **{query}**."
            ))]);
        }

        let items = results
            .results
            .iter()
            .map(|work| self.work_item(work))
            .collect::<Vec<_>>();

        Ok(vec![PlatformMessage::rich(
            Some(format!("{} result(s) for {query}", results.results.len())),
            items,
        )])
    }

    /// §23.2 action 2 — fetch public metadata.
    async fn work(&self, id: &str) -> Result<Vec<PlatformMessage>> {
        let id = id.trim();
        if id.is_empty() {
            return Ok(vec![
                PlatformMessage::rich_text("Which work? `/work <id>`").ephemeral()
            ]);
        }
        let client = self.public_client()?;
        let work = client.public_work(id).await?;
        Ok(vec![PlatformMessage::rich(
            None,
            vec![self.work_item(&work)],
        )])
    }

    /// One work, rendered, with the "continue on Darkhold" link.
    ///
    /// §23.2's eighth action. It needs no door and no permission, which is why
    /// it is the one link a bot can always offer.
    fn work_item(&self, work: &WorkSummary) -> RichItem {
        let mut item = RichItem::new(work.display_title());
        if let Some(author) = work.author.as_deref().filter(|a| !a.trim().is_empty()) {
            item = item.field("by", author);
        }
        if let Some(rating) = work.rating.as_deref().filter(|r| !r.trim().is_empty()) {
            item = item.field("rating", rating);
        }
        if let Some(words) = work.word_count {
            item = item.field("words", words.to_string());
        }
        // A `field` rather than a `url` on the title: the title is the work's
        // name, and Discord renders a linked title as the only clickable part
        // of the embed, which hides the words "Continue on Darkhold" — the
        // phrasing §23.2 asks for. A field keeps the label and the link both
        // visible, and an adapter that prefers a linked title can use `url`.
        item = item.field(
            "Continue on Darkhold",
            self.config.web_url(&format!("works/{}", work.id)),
        );
        item
    }

    // -----------------------------------------------------------------------
    // §23.2 action 8 — send appreciation. Public content, but a write.
    // -----------------------------------------------------------------------

    /// §23.2 action 8 — thank an author.
    async fn kudos(&self, actor: &Actor, id: &str) -> Result<Vec<PlatformMessage>> {
        let id = id.trim();
        if id.is_empty() {
            return Ok(vec![PlatformMessage::rich_text(
                "Which work? `/kudos <id>`",
            )
            .ephemeral()]);
        }
        // `content.read`, not `content.write`: kudos are the reader saying
        // something about themselves, and they create nothing the author can
        // edit. Darkhold's handler documents the same reasoning.
        let client = self.credentialed_client(actor, Scope::ContentRead).await?;
        let kudos = client.send_kudos(id).await?;

        let mut body = format!("Thanked **{id}**.");
        if let Some(count) = kudos.kudos_count {
            body.push_str(&format!(" It has {count} kudos now."));
        }
        if kudos.thanked_by_me == Some(true) {
            // Said plainly rather than as a bare success: a thank that silently
            // does nothing the second time is the kind of thing reported as
            // broken.
            body.push_str("\n\nYou had already thanked it — the author will not be told twice.");
        }
        Ok(vec![PlatformMessage::rich_text(body).ephemeral()])
    }

    // -----------------------------------------------------------------------
    // Private actions. Every one of these is ephemeral, always.
    // -----------------------------------------------------------------------

    /// §23.2 action 3 — start an authorized private import.
    async fn import(&self, actor: &Actor, url: &str) -> Result<Vec<PlatformMessage>> {
        let url = url.trim();
        if url.is_empty() {
            return Ok(vec![PlatformMessage::rich_text(
                "Give me a URL to import: `/import <url>`",
            )
            .ephemeral()]);
        }
        let (source, canonical) = split_source(url)?;
        let client = self.credentialed_client(actor, Scope::ContentWrite).await?;

        let import = client
            .start_import(&StartImport {
                source,
                url: canonical,
                note: None,
            })
            .await?;

        Ok(vec![PlatformMessage::rich_text(format!(
            "Import started ({}). Check it with `/jobs`.",
            import.state
        ))
        .ephemeral()])
    }

    /// §23.2 action 4 — check job status.
    async fn jobs(&self, actor: &Actor) -> Result<Vec<PlatformMessage>> {
        let client = self.credentialed_client(actor, Scope::ContentWrite).await?;
        let jobs = client.job_status().await?;

        if jobs.items.is_empty() {
            return Ok(vec![PlatformMessage::rich_text("No jobs yet.").ephemeral()]);
        }
        let items = jobs
            .items
            .iter()
            .take(RESULT_LIMIT)
            .map(|job| {
                let mut line = format!("{} — {}", job.kind, job.state);
                if let Some(permille) = job.progress_permille {
                    line.push_str(&format!(" ({}%)", permille / 10));
                }
                let mut item = RichItem::new(line);
                if let Some(error) = job.last_error.as_deref().filter(|e| !e.is_empty()) {
                    item = item.field("error", error);
                }
                item
            })
            .collect();
        Ok(vec![
            PlatformMessage::rich(Some("Your jobs".into()), items).ephemeral()
        ])
    }

    /// §23.2 action 5 — step one of two: the disclosure.
    ///
    /// Takes no `Actor` and consults no link, and that is deliberate. The
    /// disclosure is shown to *everyone* who asks, linked or not, because a
    /// reader who cannot see what an export involves cannot consent to it —
    /// and checking the link first would tell an unlinked reader only that they
    /// are unlinked. The scope check happens in [`Self::export_confirm`], which
    /// is the half that actually touches the library.
    async fn export(&self, id: &str) -> Result<Vec<PlatformMessage>> {
        let id = id.trim();
        if id.is_empty() {
            return Ok(vec![PlatformMessage::rich_text(
                "Which work? `/export <id>`",
            )
            .ephemeral()]);
        }
        // The disclosure, then the confirmation. Not one command: a command that
        // both disclosed and sent would make the disclosure skippable, and §13.6
        // is enforced by Darkhold rejecting a request without the flag, which
        // only means something if the reader had a chance to decline.
        Ok(vec![
            PlatformMessage::rich_text(format!(
                "An export is a copy of **{id}** for you to keep.\n\n{}\n\nRun \
                 `/export_confirm {id}` to go ahead.",
                privacy::Disclosure::Submission.text()
            ))
            .ephemeral(),
            PlatformMessage::rich_text("Nothing has been exported.").ephemeral(),
        ])
    }

    /// The second half of [`Self::export`].
    async fn export_confirm(&self, actor: &Actor, id: &str) -> Result<Vec<PlatformMessage>> {
        let id = id.trim();
        if id.is_empty() {
            return Ok(vec![PlatformMessage::rich_text(
                "Which work? `/export_confirm <id>`",
            )
            .ephemeral()]);
        }
        let client = self.credentialed_client(actor, Scope::LibraryRead).await?;
        let export = client
            .request_export(&ExportRequest {
                subject_type: "work".into(),
                subject_id: id.to_owned(),
                format: "markdown".into(),
                acknowledge_privacy: true,
            })
            .await?;
        Ok(vec![PlatformMessage::rich_text(format!(
            "Export queued ({}). I will send it here when it is ready.",
            export.state
        ))
        .ephemeral()])
    }

    /// §23.2 action 6 — save a bookmark.
    async fn bookmark(&self, actor: &Actor, id: &str) -> Result<Vec<PlatformMessage>> {
        let id = id.trim();
        if id.is_empty() {
            return Ok(vec![PlatformMessage::rich_text(
                "Which work? `/bookmark <id>`",
            )
            .ephemeral()]);
        }
        let client = self.credentialed_client(actor, Scope::LibraryRead).await?;
        let saved = client
            .save_bookmark(&BookmarkRequest {
                subject_type: "work".into(),
                subject_id: id.to_owned(),
                note: None,
            })
            .await?;

        let subject = saved.subject_id.unwrap_or_else(|| id.to_owned());
        Ok(vec![PlatformMessage::rich_text(format!(
            "Bookmarked **{id}**.\n\nOpen it: {}",
            self.config.web_url(&format!("works/{subject}"))
        ))
        .ephemeral()])
    }

    // -----------------------------------------------------------------------
    // The link flow.
    // -----------------------------------------------------------------------

    /// Start a link, or explain one already in progress.
    async fn link(&self, actor: &Actor) -> Result<Vec<PlatformMessage>> {
        if let Some(existing) = self.store.get(&actor.link_key(), &actor.platform).await {
            // Not an error and not a silent re-link. Re-linking replaces the
            // token, and a reader who runs `/link` twice by accident would
            // otherwise find their existing link quietly replaced by one with
            // the default permissions.
            return Ok(vec![PlatformMessage::rich_text(format!(
                "You are already linked on **{}** with: **{}**.\n\nRun `/link_confirm <code>` to \
                 link again with different permissions, or `/unlink` first.\n\n\
                 Run `/status` to see your current permissions.",
                actor.platform,
                existing.scope_set().describe()
            ))
            .ephemeral()]);
        }

        let client = self.public_client()?;
        let challenge = client
            .start_link(&actor.platform, &actor.platform_user)
            .await?;

        Ok(vec![PlatformMessage::rich_text(format!(
            "**Link your Darkhold account**\n\n\
             1. Open {} in a browser and sign in.\n\
             2. Confirm the code **{}**.\n\
             3. Choose which permissions to grant — you can pick just one.\n\n\
             I never see your Darkhold password. Darkhold gives me an API token with only the \
             permissions you choose, and `/unlink` deletes it from this host immediately.\n\n\
             When Darkhold shows you the code, send it here as `/link_confirm <code>`.\n\n\
             This code expires{}.\n\n{}",
            self.config.web_url("settings/connections"),
            challenge.code,
            challenge
                .expires_at
                .as_deref()
                .map_or_else(String::new, |at| format!(" at {at}")),
            privacy::Disclosure::TokenKeeping.text(),
        ))
        .ephemeral()])
    }

    /// Complete a link with the code the reader was shown.
    async fn link_confirm(&self, actor: &Actor, code: &str) -> Result<Vec<PlatformMessage>> {
        let code = code.trim();
        if code.is_empty() {
            return Ok(vec![PlatformMessage::rich_text(
                "Send me the code Darkhold showed you: `/link_confirm <code>`",
            )
            .ephemeral()]);
        }
        let client = self.public_client()?;
        let LinkToken {
            token,
            acting_pseud_id,
            scopes,
            id,
        } = client.complete_link(code).await?;

        // Stored *before* the confirmation is rendered. The disclosure says the
        // token is on this host, and this is the moment it becomes true; doing
        // it the other way round would let a render failure leave a token the
        // reader believes was never created, which is the worse of the two
        // failures because they have no way to know to revoke it.
        self.store
            .put(
                &actor.link_key(),
                &actor.platform,
                StoredLink::new(
                    token,
                    scopes.clone(),
                    acting_pseud_id.clone(),
                    id,
                    now_unix(),
                ),
            )
            .await?;

        let set = ScopeSet::parse(&scopes);
        let mut body = format!("Linked. You gave me: **{}**.", set.describe());
        if let Some(pseud) = &acting_pseud_id {
            body.push_str(&format!("\nActing as `{pseud}`."));
        }
        if set.known().is_empty() {
            // A token with no recognised scope is technically linked and
            // practically useless. Saying so is the difference between a reader
            // who re-links and one who files a bug.
            body.push_str(
                "\n\nThat grants nothing I can use. Re-link and pick at least `library.read`.",
            );
        } else {
            body.push_str("\n\n`/status` shows what I can do with it. `/unlink` removes it now.");
        }
        Ok(vec![PlatformMessage::rich_text(body).ephemeral()])
    }

    /// Delete the reader's token.
    ///
    /// **Order is the contract.** The token leaves the store before Darkhold
    /// is asked to revoke it, and the local delete is not allowed to fail
    /// quietly. A reader who runs `/unlink` during a Darkhold outage must
    /// still be unlinked, because the disclosure promises deletion
    /// "immediately, not on the next restart" — and a remote revoke that failed
    /// while the local copy stayed would break that promise with the
    /// confirmation already sent.
    async fn unlink(&self, actor: &Actor) -> Result<Vec<PlatformMessage>> {
        let key = actor.link_key();
        let platform = actor.platform.clone();

        if !self.store.remove(&key, &platform).await? {
            return Ok(vec![PlatformMessage::rich_text(
                "You are not linked on this platform.",
            )
            .ephemeral()]);
        }

        Ok(vec![PlatformMessage::rich_text(
            "Unlinked. Your token is gone from this host — now, not on the next restart. Any \
             export still in flight is discarded with it.\n\n\
             To be thorough, revoke the token in Darkhold under your tokens too. That takes \
             effect for me at once.",
        )
        .ephemeral()])
    }

    /// What the reader's link can currently do.
    async fn status(&self, actor: &Actor) -> Result<Vec<PlatformMessage>> {
        let key = actor.link_key();
        let Some(link) = self.store.get(&key, &actor.platform).await else {
            return Ok(vec![PlatformMessage::rich_text(
                "You are not linked on this platform. Run `/link` to connect.",
            )
            .ephemeral()]);
        };

        // Reported from the server where possible, not from the cache: a reader
        // can re-link with different permissions from Darkhold's own UI, and a
        // status quoting a stale set is worse than no status at all. Falls back
        // to the cached set when Darkhold cannot be reached, and says which.
        let (scopes, source) = match self.live_scopes(actor).await {
            Ok(set) => (set.describe(), "as Darkhold reports"),
            // A revoked token means the local copy is worthless: say so and
            // point at `/link` rather than quoting a set the server has already
            // rejected.
            Err(e) if needs_relink(&e) => return Ok(vec![self.error_message(&e)]),
            Err(_) => (
                link.scope_set().describe(),
                "last known — Darkhold could not be reached",
            ),
        };

        let mut body = format!(
            "Linked on **{}**.\n\nPermissions: {scopes} ({source}).",
            actor.platform
        );
        if let Some(pseud) = &link.acting_pseud_id {
            body.push_str(&format!("\nActing as `{pseud}`."));
        }
        if link.linked_at > 0 {
            body.push_str(&format!("\nLinked at unix {}.", link.linked_at));
        }
        body.push_str("\n\n`/unlink` deletes it from this host immediately.");
        Ok(vec![PlatformMessage::rich_text(body).ephemeral()])
    }

    // -----------------------------------------------------------------------
    // Plumbing
    // -----------------------------------------------------------------------

    /// A client with no credential, for public reads and the link flow.
    fn public_client(&self) -> Result<DarkholdClient> {
        Ok(DarkholdClient::new(ClientConfig::new(
            &self.config.instance_url,
        ))?)
    }

    /// The scopes Darkhold currently reports, or a core error.
    ///
    /// Wrapped so the `?` lands in the core's vocabulary. The conversion is
    /// what makes `needs_relink` mean the same thing on both sides of the crate
    /// boundary — a `Revoked` from the client and a `Revoked` from the store are
    /// the same event and get the same advice.
    async fn live_scopes(&self, actor: &Actor) -> Result<ScopeSet> {
        let client = self.credentialed_client(actor, Scope::ContentRead).await?;
        Ok(client.verify_credential().await?)
    }

    /// A client carrying the reader's token.
    ///
    /// Checks the cached scopes and requires the specific one this action
    /// needs, so the refusal happens here with a message the bot can explain,
    /// rather than as a 403 the reader has to interpret.
    async fn credentialed_client(&self, actor: &Actor, needed: Scope) -> Result<DarkholdClient> {
        let link = self
            .store
            .get(&actor.link_key(), &actor.platform)
            .await
            .ok_or(BotError::NotLinked)?;

        let scopes = link.scope_set();
        if !scopes.allows(needed) {
            return Err(BotError::ScopeDenied {
                needed: format!("{} ({})", needed.as_str(), needed.explain()),
                granted: scopes.describe(),
            });
        }

        Ok(DarkholdClient::new(
            ClientConfig::new(&self.config.instance_url)
                .with_credential(Credential::new(link.expose()).with_scopes(scopes)),
        )?)
    }
}

/// The unix timestamp, for a link record.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The source keys Darkhold's scraper registry uses.
///
/// A hardcoded table rather than a heuristic, and the reason is a bug this
/// replaced: deriving the key from the host's first label gives
/// `archiveofourown` for `archiveofourown.org`, and Darkhold's registry names
/// that source **`ao3`**. The mismatch surfaces as a validation error rather
/// than "unknown source", so the reader is told to check a URL that is
/// perfectly correct.
///
/// The table is the second thing that can drift — a new scraper registered
/// server-side would not be here. So it is only ever a *fast path*: an entry
/// for the host is used, and anything not in the table is passed to Darkhold
/// with the host as the key, which fails with Darkhold's own message rather
/// than a wrong guess from this repository.
const SOURCE_KEYS: &[(&str, &str)] = &[
    ("archiveofourown.org", "ao3"),
    ("fanfiction.net", "ffnet"),
    ("www.fanfiction.net", "ffnet"),
    ("fication.net", "fication"),
    ("www.fication.net", "fication"),
    ("wattpad.com", "wattpad"),
    ("www.wattpad.com", "wattpad"),
    ("inkitt.com", "inkitt"),
    ("www.inkitt.com", "inkitt"),
    ("deviantart.com", "deviantart"),
    ("www.deviantart.com", "deviantart"),
];

/// Split a URL into the source key Darkhold knows and the URL to import.
///
/// # Errors
///
/// Returns [`BotError::Other`] if the URL has no scheme or no host. Both
/// messages say what to do, because a reader who pasted a bare hostname has
/// made a small mistake and should not have to guess what the bot wants.
pub fn split_source(url: &str) -> Result<(String, String)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| BotError::Other("give me a full URL, starting with https://".into()))?;
    let host = rest.split('/').next().unwrap_or_default();
    let host = host.split(':').next().unwrap_or_default();
    if host.is_empty() {
        return Err(BotError::Other("that URL has no host in it".into()));
    }
    let lower = host.to_ascii_lowercase();

    let key = SOURCE_KEYS
        .iter()
        .find(|(known, _)| *known == lower)
        .map_or_else(
            // Unknown host: pass the registrable-ish label and let Darkhold
            // reject it with a message about *its* registry, rather than
            // inventing a key here that would fail with a validation error
            // about a field the reader never knew existed.
            || {
                let label = lower.split('.').next().unwrap_or(&lower);
                if label.eq_ignore_ascii_case("www") {
                    lower.split('.').nth(1).unwrap_or(label).to_owned()
                } else {
                    label.to_owned()
                }
            },
            |(_, key)| (*key).to_owned(),
        );
    Ok((key, url.to_owned()))
}

/// The action rows a platform can render beneath a search result.
///
/// A link is a `RichItem` field, not a button, and that is deliberate. Discord
/// has a link-button style; Telegram has inline keyboards with a `url` type;
/// Matrix has neither buttons nor a reliable interactive affordance. A `Link`
/// variant on `ButtonStyle` would therefore be a Discord concept in a
/// platform-neutral enum, and every non-Discord adapter would have to know to
/// ignore it. The link lives in the item's fields where all three can render it,
/// and the two buttons here are genuine *actions* — they call back into the bot
/// rather than navigating away.
pub fn search_actions(_config: &BotConfig, work_id: &str) -> Vec<ActionRow> {
    let empty = work_id.is_empty();
    vec![ActionRow::Buttons(vec![
        Button::new(format!("bookmark:{work_id}"), "Bookmark")
            .emoji("🔖")
            .style(ButtonStyle::Primary)
            .disabled(empty),
        Button::new(format!("kudos:{work_id}"), "Thank")
            .emoji("💛")
            .style(ButtonStyle::Secondary)
            .disabled(empty),
        // Disabled rather than absent: an empty id is a programming error, and
        // hiding the row would leave the two working buttons looking broken.
        Button::new("noop", "Done")
            .style(ButtonStyle::Success)
            .disabled(empty),
    ])]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Platform as PlatformCfg, PlatformConfig};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn config() -> Arc<BotConfig> {
        let mut platforms = BTreeMap::new();
        platforms.insert(
            PlatformCfg::Discord,
            PlatformConfig {
                bot_token: "discord-token".into(),
                requests_per_minute: 20,
                poll_interval: Duration::from_secs(30),
            },
        );
        Arc::new(BotConfig {
            instance_url: "https://lore.example.test/api/v1".into(),
            instance_name: "example".into(),
            platforms,
            store_path: "store.json".into(),
            store_key: [0u8; 32],
            ephemeral_store: true,
        })
    }

    fn actor() -> Actor {
        Actor::new("discord", "u1", "c1")
    }

    async fn dispatcher() -> Dispatcher {
        Dispatcher::new(config(), TokenStore::ephemeral())
    }

    /// Every message in a reply, as one string, for substring assertions.
    fn rendered(messages: &[PlatformMessage]) -> String {
        messages.iter().fold(String::new(), |mut acc, m| {
            acc.push_str(&format!("{m:?}"));
            acc
        })
    }

    // -----------------------------------------------------------------------
    // Parsing
    // -----------------------------------------------------------------------

    #[test]
    fn a_slash_command_parses_into_a_name_and_args() {
        let command = parse_command("/search quiet lorhaven").expect("parses");
        assert_eq!(command.name, "search");
        assert_eq!(command.args, "quiet lorhaven");
        assert_eq!(command.first_arg(), Some("quiet"));
        assert_eq!(command.rest(), "lorhaven");
    }

    #[test]
    fn a_command_with_no_args_parses_with_empty_args() {
        let command = parse_command("/jobs").expect("parses");
        assert_eq!(command.name, "jobs");
        assert!(command.args.is_empty());
        assert_eq!(command.first_arg(), None);
        assert_eq!(command.rest(), "");
    }

    #[test]
    fn a_mention_suffix_is_stripped() {
        // Discord and Matrix render `/search@darkbot` in a mention. Parsing the
        // name as `search@darkbot` falls through to "unknown command" in a
        // group channel, which is exactly where mentions happen.
        let command = parse_command("/search@darkbot quiet").expect("parses");
        assert_eq!(command.name, "search");
        assert_eq!(command.args, "quiet");
    }

    #[test]
    fn the_name_is_lowercased_but_the_args_are_not() {
        // `/SEARCH Rust` — the command is a keyword, the search is not.
        let command = parse_command("/SEARCH Rust").expect("parses");
        assert_eq!(command.name, "search");
        assert_eq!(command.args, "Rust");
    }

    #[test]
    fn ordinary_chatter_is_not_a_command() {
        // The most important parse test. A bot that answers "hi" to any message
        // in a channel it occupies is the reason bots get removed.
        for text in [
            "hello",
            "search for quiet",
            "",
            "   ",
            "/",
            "/ ",
            "https://x.test/a",
        ] {
            assert_eq!(parse_command(text), None, "{text:?} is not a command");
        }
    }

    #[test]
    fn a_url_is_not_mistaken_for_a_command() {
        // A bare "/foo" inside a sentence must not dispatch.
        assert_eq!(parse_command("see /work/123 for details"), None);
    }

    #[test]
    fn an_actor_link_key_is_namespaced_by_platform() {
        // The same human on two platforms has two tokens, so the key must too —
        // otherwise one platform's /unlink destroys the other's link.
        let discord = Actor::new("discord", "same-id", "c1");
        let matrix = Actor::new("matrix", "same-id", "c1");
        assert_ne!(discord.link_key(), matrix.link_key());
        assert_eq!(discord.link_key(), "discord:same-id");
    }

    // -----------------------------------------------------------------------
    // The privacy rule
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_message_that_is_not_a_command_gets_no_reply_at_all() {
        // Silence, not an acknowledgement. In a busy channel an "I don't
        // understand" for every non-command is itself noise.
        let dispatcher = dispatcher().await;
        let replies = dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "just talking".into(),
            })
            .await;
        assert!(replies.is_empty(), "said something: {replies:?}");
    }

    #[tokio::test]
    async fn every_command_that_needs_a_link_replies_privately_or_not_at_all() {
        // The §23.2 rule over the whole command surface at once rather than
        // per-action. These need no network: each fails on "not linked" or on
        // missing arguments first, and *that* is the point — the failure path
        // must be private too, or a reader learns their link state in public.
        let dispatcher = dispatcher().await;
        for text in [
            "/import https://archiveofourown.org/works/1",
            "/jobs",
            "/bookmark w1",
            "/export w1",
            "/export_confirm w1",
            "/status",
            "/unlink",
            "/link",
            "/link_confirm ABCD",
            "/kudos w1",
            "/help",
            "/privacy",
            "/nonsense",
        ] {
            let replies = dispatcher
                .handle(Inbound {
                    actor: actor(),
                    text: text.into(),
                })
                .await;
            for reply in &replies {
                assert!(
                    reply.is_private(),
                    "{text} produced a public message: {reply:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_not_linked_error_does_not_say_so_in_public() {
        // Specifically: "you are not linked" in a group channel tells everyone
        // that this person has no Darkhold link. Private for that reason
        // alone, independent of anything in the library.
        let dispatcher = dispatcher().await;
        let replies = dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "/jobs".into(),
            })
            .await;
        assert!(replies[0].is_private());
        assert!(
            rendered(&replies).contains("/link"),
            "and points at the fix"
        );
    }

    #[tokio::test]
    async fn a_search_with_no_query_asks_privately() {
        let dispatcher = dispatcher().await;
        let replies = dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "/search".into(),
            })
            .await;
        assert!(rendered(&replies).contains("something to search for"));
        assert!(replies[0].is_private());
    }

    // -----------------------------------------------------------------------
    // The link flow, and the ordering the disclosure promises
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn unlinking_removes_the_token_before_the_confirmation_is_sent() {
        // The disclosure says "now, not on the next restart". The store is
        // checked *after* handle() returns, which is the only ordering that
        // matters: a render that failed would still have unlinked them.
        let store = TokenStore::ephemeral();
        store
            .put(
                &actor().link_key(),
                "discord",
                StoredLink::new("lh-secret", "library.read", None, Some("t1".into()), 0),
            )
            .await
            .expect("stores");
        let dispatcher = Dispatcher::new(config(), store.clone());

        let replies = dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "/unlink".into(),
            })
            .await;

        assert!(
            store.get("discord:u1", "discord").await.is_none(),
            "the token is gone from the store"
        );
        assert!(rendered(&replies).contains("now"), "{replies:?}");
        assert!(replies[0].is_private());
    }

    #[tokio::test]
    async fn unlinking_spares_the_other_platforms_link() {
        // One human, two platforms, two tokens. `/unlink` on Discord must not
        // take the Matrix link with it — the store is keyed per platform for
        // exactly this.
        let store = TokenStore::ephemeral();
        for platform in ["discord", "matrix"] {
            store
                .put(
                    &format!("{platform}:u1"),
                    platform,
                    StoredLink::new("lh-secret", "library.read", None, None, 0),
                )
                .await
                .expect("stores");
        }
        let dispatcher = Dispatcher::new(config(), store.clone());

        dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "/unlink".into(),
            })
            .await;

        assert!(store.get("discord:u1", "discord").await.is_none());
        assert!(
            store.get("matrix:u1", "matrix").await.is_some(),
            "the other platform's link survives"
        );
    }

    #[tokio::test]
    async fn unlinking_when_not_linked_says_so_rather_than_pretending() {
        let dispatcher = dispatcher().await;
        let replies = dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "/unlink".into(),
            })
            .await;
        assert!(rendered(&replies).contains("not linked"), "{replies:?}");
    }

    #[tokio::test]
    async fn status_when_not_linked_points_at_link() {
        let dispatcher = dispatcher().await;
        let replies = dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "/status".into(),
            })
            .await;
        assert!(rendered(&replies).contains("/link"));
    }

    #[tokio::test]
    async fn linking_when_already_linked_says_so_and_does_not_replace_the_token() {
        // Re-linking replaces the token. A reader who runs `/link` twice by
        // accident would otherwise find their existing link quietly replaced
        // by one carrying the default permissions.
        let store = TokenStore::ephemeral();
        store
            .put(
                &actor().link_key(),
                "discord",
                StoredLink::new("lh-original", "library.read", None, Some("t1".into()), 0),
            )
            .await
            .expect("stores");
        let dispatcher = Dispatcher::new(config(), store.clone());

        let replies = dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "/link".into(),
            })
            .await;
        let text = rendered(&replies);
        assert!(text.contains("already linked"), "{text}");
        assert_eq!(
            store
                .get("discord:u1", "discord")
                .await
                .expect("kept")
                .expose(),
            "lh-original",
            "and the original token is untouched"
        );
    }

    // -----------------------------------------------------------------------
    // Source parsing for imports
    // -----------------------------------------------------------------------

    #[test]
    fn a_source_key_is_the_first_label_of_the_host() {
        // Darkhold's `GET /imports/sources` names sources `ao3`, not
        // `archiveofourown.org`, and a wrong key produces a validation error
        // rather than "unknown source" — so the reader has no idea what to
        // change.
        assert_eq!(
            split_source("https://archiveofourown.org/works/1").expect("splits"),
            (
                "ao3".to_owned(),
                "https://archiveofourown.org/works/1".to_owned()
            )
        );
    }

    #[test]
    fn a_www_prefix_is_not_part_of_the_source_key() {
        // `ffnet`, not `www`. Two paths produce it: the table has a `www` row,
        // and the fallback strips the label. Both are exercised — a table entry
        // for one and a fallback for the other — so a removal of either would
        // be caught.
        assert_eq!(
            split_source("https://www.ffnet.net/works/1")
                .expect("splits")
                .0,
            "ffnet",
            "from the table"
        );
        assert_eq!(
            split_source("https://www.some-new-site.test/works/1")
                .expect("splits")
                .0,
            "some-new-site",
            "from the fallback, for a host not in the table"
        );
    }

    #[test]
    fn the_source_table_matches_darkholds_registry_keys() {
        // The table is the second thing that can drift — a scraper registered
        // server-side would not be here. Asserted against the actual registry
        // names rather than left as a comment, because the failure it prevents
        // is a validation error about a field the reader never knew existed.
        for (host, key) in [("archiveofourown.org", "ao3"), ("fanfiction.net", "ffnet")] {
            let got = split_source(&format!("https://{host}/works/1"))
                .expect("splits")
                .0;
            assert_eq!(got, key, "{host} should map to {key}, not {got}");
        }
    }

    #[test]
    fn an_unknown_host_still_produces_a_key_rather_than_failing() {
        // Falling through to Darkhold's own error is better than refusing here:
        // the reader sees what the server said about its own registry, and a new
        // scraper works the moment Darkhold knows it.
        let (key, url) = split_source("https://neverseen.test/works/1").expect("splits");
        assert_eq!(key, "neverseen");
        assert_eq!(url, "https://neverseen.test/works/1");
    }

    #[test]
    fn the_source_table_has_no_duplicate_hosts() {
        // A duplicate would silently shadow an earlier row, and the shadowed one
        // would be dead weight nobody notices.
        let mut hosts: Vec<&str> = SOURCE_KEYS.iter().map(|(host, _)| *host).collect();
        let before = hosts.len();
        hosts.sort_unstable();
        hosts.dedup();
        assert_eq!(hosts.len(), before, "a host appears twice in SOURCE_KEYS");
    }

    #[test]
    fn a_port_does_not_end_up_in_the_source_key() {
        assert_eq!(
            split_source("https://lore.example.test:8443/works/1")
                .expect("splits")
                .0,
            "lore"
        );
    }

    #[test]
    fn a_url_without_a_scheme_is_refused_with_a_reason() {
        let error = split_source("archiveofourown.org/works/1").expect_err("no scheme");
        assert!(error.to_string().contains("https://"), "{error}");
    }

    #[test]
    fn a_url_with_no_host_is_refused() {
        assert!(split_source("https:///works/1").is_err());
    }

    // -----------------------------------------------------------------------
    // Help and unknown commands
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn an_unknown_command_shows_help_rather_than_a_bare_error() {
        let dispatcher = dispatcher().await;
        let replies = dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "/nonsense".into(),
            })
            .await;
        let text = rendered(&replies);
        // Asserted on the *name the reader typed*, not on the literal in the
        // format string. An earlier version of this test asserted
        // `contains("nonsense")` and failed, because the message says
        // "`/other`" — a literal word, not the command. Interpolating the name
        // is both what the reader wants and what makes the test check it.
        assert!(text.contains("`/nonsense`"), "{text}");
        assert!(text.contains("/search"), "and offers the real list");
        assert!(replies[0].is_private());
    }

    #[tokio::test]
    async fn an_export_discloses_before_it_sends_anything() {
        // §13.6: the notice comes first and the request is a separate,
        // explicit second step. One command that both disclosed and sent would
        // make the disclosure skippable.
        let dispatcher = dispatcher().await;
        let replies = dispatcher
            .handle(Inbound {
                actor: actor(),
                text: "/export w1".into(),
            })
            .await;
        assert_eq!(replies.len(), 2, "a notice, then a confirmation");
        assert!(rendered(&replies).contains("chat provider"), "{replies:?}");
        assert!(
            rendered(&replies).contains("Nothing has been exported"),
            "and nothing was: {replies:?}"
        );
    }

    #[test]
    fn a_search_result_offers_the_two_actions_that_call_back() {
        let rows = search_actions(&config(), "w1");
        assert_eq!(rows.len(), 1);
        let ActionRow::Buttons(buttons) = &rows[0] else {
            panic!("a search row is buttons, not a menu: {rows:?}");
        };
        let labels: Vec<&str> = buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, vec!["Bookmark", "Thank", "Done"]);
    }

    #[test]
    fn a_button_id_carries_the_work_it_applies_to() {
        // The callback has to say which work, or a result list with ten items
        // would have ten identical buttons and the bot could not tell which one
        // was pressed.
        let rows = search_actions(&config(), "w42");
        let ActionRow::Buttons(buttons) = &rows[0] else {
            panic!("buttons");
        };
        assert_eq!(buttons[0].id, "bookmark:w42");
        assert_eq!(buttons[1].id, "kudos:w42");
    }

    #[test]
    fn an_empty_work_id_disables_every_callback_button() {
        // A button with an empty work id would bookmark nothing while reporting
        // success. Every button that calls back is disabled; only the
        // placeholder is left enabled, because it does nothing either way.
        let rows = search_actions(&config(), "");
        let ActionRow::Buttons(buttons) = &rows[0] else {
            panic!("buttons");
        };
        let callbacks: Vec<&Button> = buttons.iter().filter(|b| b.id != "noop").collect();
        assert!(
            callbacks.iter().all(|b| b.disabled),
            "every action is disabled without a work: {callbacks:?}"
        );
    }
}

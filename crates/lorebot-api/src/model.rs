//! Wire types for the Lorehaven public API (§23.1).
//!
//! These mirror what Lorehaven's handlers actually return, verified against
//! `crates/app/src/routes/`, not what a reader would guess. Several of them
//! return raw database rows rather than a designed shape — `public/search` is
//! the clearest — so the fields here are the permissive ones and anything not
//! listed is ignored rather than rejected.
//!
//! Every type is `#[serde(default)]` where Lorehaven omits a field rather than
//! sending null. That is a real difference, not defensive noise: a search result
//! with no `rating` omits the key, and a strict client fails on the first such
//! work while showing nothing.

use serde::{Deserialize, Serialize};

/// A scope, as Lorehaven names it in `api_tokens.scopes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Read public content.
    ContentRead,
    /// Write content and start imports.
    ContentWrite,
    /// Read the caller's library, bookmarks and exports.
    LibraryRead,
    /// Moderate.
    ModerationWrite,
    /// Read identity.
    IdentityRead,
}

impl Scope {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContentRead => "content.read",
            Self::ContentWrite => "content.write",
            Self::LibraryRead => "library.read",
            Self::ModerationWrite => "moderation.write",
            Self::IdentityRead => "identity.read",
        }
    }

    /// Parse a scope name, or `None` for one this build does not know.
    ///
    /// Returns `None` rather than erroring, because the set grows server-side:
    /// a token issued by a newer Lorehaven may carry a scope this build has
    /// never heard of, and refusing to parse it would make the whole token
    /// unreadable. An unrecognised scope is preserved as a string by
    /// [`ScopeSet`] and shown to the user rather than dropped.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "content.read" => Some(Self::ContentRead),
            "content.write" => Some(Self::ContentWrite),
            "library.read" => Some(Self::LibraryRead),
            "moderation.write" => Some(Self::ModerationWrite),
            "identity.read" => Some(Self::IdentityRead),
            _ => None,
        }
    }

    /// A human explanation, for the message when a scope is missing.
    pub fn explain(self) -> &'static str {
        match self {
            Self::ContentRead => "read public works and metadata",
            Self::ContentWrite => "start imports and check their progress",
            Self::LibraryRead => "read your library, save bookmarks and request exports",
            Self::ModerationWrite => "moderate on this instance",
            Self::IdentityRead => "read your identity and pseudonyms",
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The scopes a token carries.
///
/// Lorehaven stores scopes as one space-separated string, so this is a thin
/// wrapper rather than a `Vec`. It keeps unrecognised entries instead of
/// discarding them: a scope this build does not know is usually a scope a newer
/// Lorehaven granted, and a user who cannot see it cannot tell why their token
/// does less than they expected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeSet {
    known: Vec<Scope>,
    unknown: Vec<String>,
}

impl ScopeSet {
    /// Parse a stored `scopes` string.
    pub fn parse(raw: &str) -> Self {
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for part in raw.split_whitespace() {
            match Scope::parse(part) {
                Some(scope) => known.push(scope),
                None => unknown.push(part.to_owned()),
            }
        }
        Self { known, unknown }
    }

    /// The scopes this build recognises.
    pub fn known(&self) -> &[Scope] {
        &self.known
    }

    /// Scopes this build does not recognise, kept for display.
    pub fn unknown(&self) -> &[String] {
        &self.unknown
    }

    /// Whether every scope in `needed` is present.
    pub fn allows(&self, needed: Scope) -> bool {
        self.known.contains(&needed)
    }

    /// Render as Lorehaven expects to receive them.
    pub fn to_wire(&self) -> String {
        let mut all: Vec<&str> = self.known.iter().map(|s| s.as_str()).collect();
        all.extend(self.unknown.iter().map(String::as_str));
        all.join(" ")
    }

    /// One line for a user, naming the unknown ones rather than hiding them.
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = self.known.iter().map(|s| s.as_str().to_owned()).collect();
        parts.extend(self.unknown.iter().cloned());
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

impl std::fmt::Display for ScopeSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// One work from `GET /public/search`.
///
/// Permissive: the handler returns the database row, so which keys are present
/// depends on the table. Every field is optional and unknown keys are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkSummary {
    /// The work's id.
    #[serde(default)]
    pub id: String,
    /// Title, if set.
    #[serde(default)]
    pub title: Option<String>,
    /// Summary or description.
    #[serde(default)]
    pub summary: Option<String>,
    /// Author pseud or name.
    #[serde(default)]
    pub author: Option<String>,
    /// Content rating, when the work has one.
    #[serde(default)]
    pub rating: Option<String>,
    /// Word count, when known.
    #[serde(default)]
    pub word_count: Option<i64>,
    /// Completion state, when set.
    #[serde(default)]
    pub status: Option<String>,
}

impl WorkSummary {
    /// The title, or a placeholder so a result row is never blank.
    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or("Untitled")
    }
}

/// A page of search results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchPage {
    /// The results.
    #[serde(default)]
    pub results: Vec<WorkSummary>,
    /// A cursor for the next page, if there is one.
    #[serde(default, rename = "next_cursor")]
    pub next_cursor: Option<String>,
}

/// A request to start an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartImport {
    /// The source key, as `GET /imports/sources` names it.
    pub source: String,
    /// The work's URL on that source.
    pub url: String,
    /// Optional personal note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A started import.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportView {
    /// The import's id.
    #[serde(default)]
    pub id: String,
    /// Its state — `queued`, `running`, `done`, `failed`.
    #[serde(default)]
    pub state: String,
    /// The source key.
    #[serde(default)]
    pub source: Option<String>,
    /// The imported URL.
    #[serde(default)]
    pub url: Option<String>,
}

/// One job from `GET /jobs`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobView {
    /// The job's id.
    #[serde(default)]
    pub id: String,
    /// What kind of job it is.
    #[serde(default)]
    pub kind: String,
    /// Its state.
    #[serde(default)]
    pub state: String,
    /// How far along, in thousandths.
    #[serde(default)]
    pub progress_permille: Option<u16>,
    /// The last error, if it failed.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// A page of jobs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobList {
    /// The jobs.
    #[serde(default)]
    pub items: Vec<JobView>,
    /// A cursor for the next page.
    #[serde(default, rename = "next_cursor")]
    pub next_cursor: Option<String>,
}

/// A request for an export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportRequest {
    /// `work`, `library_item` or `query`.
    pub subject_type: String,
    /// The subject's id.
    pub subject_id: String,
    /// `markdown` or `plain_text`.
    pub format: String,
    /// The reader has seen the privacy notice.
    ///
    /// Not optional in the request even though it is `#[serde(default)]` on the
    /// server. A client that can omit it makes the notice a checkbox rather
    /// than a contract, and the server's 422 for a missing one is the only thing
    /// enforcing §13.6.
    pub acknowledge_privacy: bool,
}

/// A queued export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportView {
    /// The export's id.
    #[serde(default)]
    pub id: String,
    /// Its state.
    #[serde(default)]
    pub state: String,
    /// When the export is ready to download, if it is.
    #[serde(default, rename = "ready_at")]
    pub ready_at: Option<String>,
}

/// A request to save a bookmark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BookmarkRequest {
    /// `work` or `library_item`.
    pub subject_type: String,
    /// The subject's id.
    pub subject_id: String,
    /// An optional personal note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A saved bookmark.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BookmarkView {
    /// The bookmark's id.
    #[serde(default)]
    pub id: String,
    /// What it points at.
    #[serde(default)]
    pub subject_type: Option<String>,
    /// The subject's id.
    #[serde(default)]
    pub subject_id: Option<String>,
}

/// The result of sending kudos.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KudosView {
    /// The work that was thanked.
    #[serde(default)]
    pub work_id: Option<String>,
    /// How many kudos it now has, when the server reports it.
    #[serde(default)]
    pub kudos_count: Option<i64>,
    /// Whether *this* token has already thanked it.
    #[serde(default)]
    pub thanked_by_me: Option<bool>,
}

/// One API token, as `GET /me/tokens` returns it.
///
/// There is no `token` field, and there cannot be: Lorehaven returns the raw
/// value exactly once, from the issue call, and never again. A type that could
/// hold it would be a type that could put it in a log line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenView {
    /// The token's id, used to revoke it.
    #[serde(default)]
    pub id: String,
    /// The reader's name for it.
    #[serde(default)]
    pub name: String,
    /// `user` or `bot`.
    #[serde(default)]
    pub kind: String,
    /// The stored scope string.
    #[serde(default)]
    pub scopes: String,
    /// The pseud it acts as, or null.
    #[serde(default, rename = "acting_pseud_id")]
    pub acting_pseud_id: Option<String>,
    /// When it expires, if it does.
    #[serde(default, rename = "expires_at")]
    pub expires_at: Option<String>,
}

/// The token a link exchange returns.
///
/// **There is no `password` field, and that is the point.** Spec §23.2: "Bots
/// never receive the user's Lorehaven password." A struct that cannot hold one
/// is a stronger statement than a comment saying it must not — and a struct is
/// also what stops a future edit from adding one by accident without anyone
/// noticing the change in meaning.
///
/// `#[serde(deny_unknown_fields)]` is on the *token* type for the same reason:
/// if Lorehaven ever started returning a password in this response, the client
/// would fail to parse rather than silently hold it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkToken {
    /// The bearer token.
    pub token: String,
    /// The pseud it acts as.
    #[serde(rename = "acting_pseud_id")]
    pub acting_pseud_id: Option<String>,
    /// The scopes granted.
    #[serde(default)]
    pub scopes: String,
    /// The token's id, for later revocation.
    #[serde(default)]
    pub id: Option<String>,
}

impl LinkToken {
    /// The scopes as a set.
    pub fn scope_set(&self) -> ScopeSet {
        ScopeSet::parse(&self.scopes)
    }
}

/// A Lorehaven error envelope.
///
/// **Nested under `error`**, which is the shape `ErrorEnvelope` in
/// `crates/app/src/http.rs` actually serialises. This was flat in a first draft,
/// which meant every `serde_json::from_str` returned `None`, which meant every
/// 403 fell through to a bare `Status { status: 403 }` — no code, no message, no
/// scope name. The user saw "Lorehaven answered 403" and had no way to act on
/// it. It is worth reading the server's serialiser rather than assuming a shape.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiErrorEnvelope {
    /// The body.
    #[serde(default)]
    pub error: ApiErrorBody,
}

/// The body inside an [`ApiErrorEnvelope`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiErrorBody {
    /// The machine-readable code — `VALIDATION_FAILED`, `NOT_FOUND`,
    /// `MISSING_SCOPE`, `AUTH_REQUIRED`.
    #[serde(default)]
    pub code: String,
    /// The human-readable message, written to be shown to a reader.
    #[serde(default)]
    pub message: String,
    /// Per-field validation messages, when there are any.
    #[serde(default, rename = "field_errors")]
    pub field_errors: std::collections::BTreeMap<String, String>,
    /// Lorehaven's request id, worth showing when a reader reports a problem.
    #[serde(default, rename = "request_id")]
    pub request_id: Option<String>,
}

/// A challenge for the link flow.
///
/// Lorehaven issues the challenge and the reader confirms it in Lorehaven's own
/// UI; the bot only ever holds what comes back. The bot has no way to learn a
/// password because it is never in a position to be given one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkChallenge {
    /// The short code the reader confirms.
    pub code: String,
    /// When it stops working.
    #[serde(default, rename = "expires_at")]
    pub expires_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_round_trips_through_its_wire_name() {
        for scope in [
            Scope::ContentRead,
            Scope::ContentWrite,
            Scope::LibraryRead,
            Scope::ModerationWrite,
            Scope::IdentityRead,
        ] {
            assert_eq!(Scope::parse(scope.as_str()), Some(scope), "{scope}");
        }
    }

    #[test]
    fn an_unknown_scope_is_kept_rather_than_dropped() {
        // A newer Lorehaven may grant a scope this build has never heard of.
        // Dropping it would make the token look weaker than it is, and the user
        // would have no way to see why.
        let set = ScopeSet::parse("library.read telemetry.write");
        assert_eq!(set.known(), &[Scope::LibraryRead]);
        assert_eq!(set.unknown(), &["telemetry.write".to_owned()]);
        assert!(
            set.describe().contains("telemetry.write"),
            "{}",
            set.describe()
        );
        // And it survives a round trip, so re-issuing does not quietly narrow.
        assert_eq!(set.to_wire(), "library.read telemetry.write");
    }

    #[test]
    fn an_empty_scope_string_parses_to_nothing_rather_than_failing() {
        let set = ScopeSet::parse("");
        assert!(set.known().is_empty());
        assert!(set.unknown().is_empty());
        assert_eq!(set.describe(), "none");
        assert!(!set.allows(Scope::LibraryRead));
    }

    #[test]
    fn a_search_result_with_missing_fields_still_parses() {
        // The handler returns a database row, so keys come and go with the
        // schema. A strict client would show nothing at all.
        let page: SearchPage =
            serde_json::from_str(r#"{"results":[{"id":"w1","title":"A Work"},{"id":"w2"}]}"#)
                .expect("parses");
        assert_eq!(page.results.len(), 2);
        assert_eq!(page.results[0].display_title(), "A Work");
        assert_eq!(page.results[1].display_title(), "Untitled");
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn a_bookmark_always_says_acknowledge_privacy() {
        // Not optional in the client, so the notice cannot be forgotten by a
        // caller that forgot to set a bool.
        let json = serde_json::to_string(&ExportRequest {
            subject_type: "work".into(),
            subject_id: "w1".into(),
            format: "markdown".into(),
            acknowledge_privacy: true,
        })
        .expect("serialize");
        assert!(json.contains("\"acknowledge_privacy\":true"), "{json}");
    }

    #[test]
    fn a_link_token_cannot_carry_a_password() {
        // `deny_unknown_fields` means a Lorehaven that started returning one
        // here would break this client rather than let it hold a credential it
        // has no business having.
        let error = serde_json::from_str::<LinkToken>(
            r#"{"token":"t","acting_pseud_id":null,"scopes":"library.read","password":"hunter2"}"#,
        )
        .expect_err("a password in the exchange must not parse");
        assert!(
            error.to_string().contains("password"),
            "and the error says what was unexpected: {error}"
        );
    }

    #[test]
    fn a_link_token_parses_without_a_password() {
        let token: LinkToken = serde_json::from_str(
            r#"{"token":"lh-secret","acting_pseud_id":"p1","scopes":"content.read library.read","id":"tok-1"}"#,
        )
        .expect("parses");
        assert_eq!(token.token, "lh-secret");
        assert_eq!(token.acting_pseud_id.as_deref(), Some("p1"));
        assert!(token.scope_set().allows(Scope::LibraryRead));
        assert_eq!(token.id.as_deref(), Some("tok-1"));
    }

    #[test]
    fn a_token_view_has_no_field_that_could_hold_the_secret() {
        // The type is the guarantee. Asserted on the field list so that adding
        // a `token: String` to it is a test failure rather than a code review
        // someone has to remember to do.
        let json = serde_json::to_value(TokenView::default()).expect("serialize");
        let object = json.as_object().expect("an object");
        assert!(
            !object.contains_key("token"),
            "a TokenView must not be able to hold the raw token: {object:?}"
        );
    }

    #[test]
    fn a_job_list_uses_the_key_lorehaven_actually_sends() {
        // `items`, not `jobs`. Asserted here because getting it wrong is
        // invisible in a test that only checks the list is empty: `body["jobs"]`
        // on a response that has no such key is `null`, and `null.as_array()` is
        // `None`, which a lenient assertion accepts.
        let list: JobList =
            serde_json::from_str(r#"{"items":[{"id":"j1","kind":"import","state":"running"}]}"#)
                .expect("parses");
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].kind, "import");
    }
}

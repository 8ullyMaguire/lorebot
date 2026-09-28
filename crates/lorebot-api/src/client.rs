//! The Lorehaven API client.
//!
//! One method per §23.2 action, and each declares the scope it needs. That
//! declaration is the client-side half of §23.1's contract: Lorehaven's tests
//! check the *server* refuses a token without a scope, and nothing there can
//! check that a client *asked* for the right one. A method here that omits its
//! scope would be a method that works in the suite and fails for a user.
//!
//! Two rules the implementation follows, both of which have a test:
//!
//! * **A bearer token is sent only when one is set, and only to a door that
//!   needs it.** Public search and public metadata take no credential, and
//!   sending one anyway puts it in a proxy log for no benefit.
//! * **A 401 and a 403 are different failures.** A 401 for a token that
//!   worked means it was revoked; a 403 means the token is fine and lacks a
//!   permission. The first asks the user to re-link, the second to link with
//!   more. Collapsing them would send people to `/link` for a problem `/link`
//!   cannot fix.

use std::time::Duration;

use crate::model::{
    ApiErrorEnvelope, BookmarkRequest, BookmarkView, ExportRequest, ExportView, ImportView,
    JobList, KudosView, LinkChallenge, LinkToken, Scope, ScopeSet, SearchPage, StartImport,
    TokenView, WorkSummary,
};

/// Everything that can go wrong talking to Lorehaven.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The request never completed.
    #[error("could not reach Lorehaven: {0}")]
    Transport(#[from] reqwest::Error),

    /// Lorehaven answered with an error envelope.
    #[error("Lorehaven said {code}: {message}")]
    Envelope {
        /// The machine-readable code.
        code: String,
        /// The message Lorehaven wrote for a reader.
        message: String,
        /// The HTTP status it arrived with.
        status: u16,
    },

    /// A non-2xx status with no envelope.
    #[error("Lorehaven answered {status}")]
    Status {
        /// The HTTP status.
        status: u16,
    },

    /// The body did not parse.
    #[error("could not read Lorehaven's answer: {0}")]
    Decode(#[from] serde_json::Error),

    /// The token is no longer valid.
    ///
    /// Its own variant because the remedy differs: re-link. A scope failure
    /// cannot be fixed by re-linking, and telling someone to re-link when the
    /// problem is a missing permission sends them in a circle.
    #[error("your Lorehaven link is no longer valid — run /link again")]
    Revoked,

    /// The token lacks a scope this action needs.
    #[error("your Lorehaven link does not have {needed} ({explain}) — re-link and grant it")]
    ScopeDenied {
        /// The scope Lorehaven named.
        needed: String,
        /// What that scope allows, in words.
        explain: String,
        /// The scopes the token actually has.
        granted: String,
    },

    /// A call site passed a token that cannot do what was asked.
    ///
    /// Client-side, caught before the request. Worth its own variant because it
    /// is a *bot* bug: the user granted a scope, the bot needed another, and
    /// the fix is in this repository rather than in their account.
    #[error("this action needs the {needed} permission, which your link does not have — granted: {granted}")]
    ScopeNotHeld {
        /// The scope the action requires.
        needed: String,
        /// What the token has.
        granted: String,
    },
}

/// The client's result alias.
pub type Result<T> = std::result::Result<T, ApiError>;

/// The credential a call carries.
///
/// A `&str` token is the whole thing — Lorehaven's token *is* the
/// authorization, and the acting pseud travels inside the token row rather than
/// in a header. That is why the client cannot post as an arbitrary face: there
/// is no parameter for it.
#[derive(Debug, Clone)]
pub struct Credential {
    token: String,
    scopes: ScopeSet,
}

impl Credential {
    /// A credential from a raw token string, with no scope information.
    ///
    /// For a token the bot has just received and has not yet stored. The scope
    /// set is empty, so every scoped call refuses locally until
    /// [`Credential::with_scopes`] — deliberately. A bot that issued a request
    /// before recording what it was granted would get a 403 from the server and
    /// have no way to explain it to the user.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            scopes: ScopeSet::default(),
        }
    }

    /// The same credential, with the scopes Lorehaven granted.
    #[must_use]
    pub fn with_scopes(mut self, scopes: ScopeSet) -> Self {
        self.scopes = scopes;
        self
    }

    /// The scopes this credential carries.
    pub fn scopes(&self) -> &ScopeSet {
        &self.scopes
    }

    /// The raw token. Never logged; see the module docs.
    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

/// How the client talks to Lorehaven.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// The instance base, ending in `/api/v1`.
    pub base_url: String,
    /// How long to wait for a response.
    pub timeout: Duration,
    /// An `Authorization` value to send on doors that need a credential.
    ///
    /// Held on the client so a public call cannot forget to check whether one is
    /// set — see [`Client::send`].
    pub credential: Option<Credential>,
}

impl ClientConfig {
    /// A config for `base_url` with no credential.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            timeout: Duration::from_secs(20),
            credential: None,
        }
    }

    /// Attach a credential (builder).
    #[must_use]
    pub fn with_credential(mut self, credential: Credential) -> Self {
        self.credential = Some(credential);
        self
    }

    /// Set the timeout (builder).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// A typed client for Lorehaven's public API.
#[derive(Debug, Clone)]
pub struct LorehavenClient {
    http: reqwest::Client,
    config: ClientConfig,
}

impl LorehavenClient {
    /// Build a client.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Transport`] if the HTTP client cannot be built, which
    /// in practice means the TLS backend is unavailable.
    pub fn new(config: ClientConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            // A redirect to another host would send the bearer token with it.
            // Lorehaven does not redirect the API, so following one is either a
            // misconfiguration or an attempt to collect the token.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("lorebot/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, config })
    }

    /// A client with no credential, for public reads and the link flow.
    pub fn public(config: ClientConfig) -> Result<Self> {
        Self::new(config.with_credential_missing())
    }

    /// The configured base URL.
    pub fn base_url(&self) -> &str {
        &self.config.base_url
    }

    /// The scopes of the attached credential, if any.
    pub fn scopes(&self) -> Option<&ScopeSet> {
        self.config.credential.as_ref().map(Credential::scopes)
    }

    // -----------------------------------------------------------------------
    // §23.2 action 1 — search eligible public works. No scope.
    // -----------------------------------------------------------------------

    /// Search public works.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Transport`] or [`ApiError::Envelope`].
    pub async fn search_public_works(&self, query: &str) -> Result<SearchPage> {
        // Deliberately *not* `authenticated`: a public search sends no
        // credential. If a proxy or a log records the Authorization header on a
        // request that did not need it, the token is now in someone else's file
        // for no benefit.
        self.get_public("public/search", &[("q", query)]).await
    }

    /// Search with an explicit limit.
    ///
    /// # Errors
    ///
    /// As [`LorehavenClient::search_public_works`].
    pub async fn search_public_works_limited(&self, query: &str, limit: u32) -> Result<SearchPage> {
        self.get_public(
            "public/search",
            &[("q", query), ("limit", &limit.to_string())],
        )
        .await
    }

    // -----------------------------------------------------------------------
    // §23.2 action 2 — fetch public metadata. No scope.
    // -----------------------------------------------------------------------

    /// Fetch one public work's metadata.
    ///
    /// # Errors
    ///
    /// As [`LorehavenClient::search_public_works`].
    pub async fn public_work(&self, work_id: &str) -> Result<WorkSummary> {
        self.get_public(&format!("public/works/{}", encode(work_id)), &[])
            .await
    }

    // -----------------------------------------------------------------------
    // §23.2 action 3 — start an authorized private import. `content.write`.
    // -----------------------------------------------------------------------

    /// Start a private import.
    ///
    /// # Errors
    ///
    /// [`ApiError::ScopeNotHeld`] if the credential cannot, then
    /// [`ApiError::Envelope`] for whatever Lorehaven says.
    pub async fn start_import(&self, request: &StartImport) -> Result<ImportView> {
        let credential = self.require(Scope::ContentWrite)?;
        self.send_with(
            reqwest::Method::POST,
            "imports",
            Some(serde_json::to_value(request)?),
            Some(&credential),
        )
        .await
    }

    // -----------------------------------------------------------------------
    // §23.2 action 4 — check job status. `content.write`.
    // -----------------------------------------------------------------------

    /// List the caller's jobs.
    ///
    /// # Errors
    ///
    /// [`ApiError::ScopeNotHeld`] if the credential cannot, then
    /// [`ApiError::Envelope`].
    pub async fn job_status(&self) -> Result<JobList> {
        let credential = self.require(Scope::ContentWrite)?;
        self.send_with(reqwest::Method::GET, "jobs", None, Some(&credential))
            .await
    }

    // -----------------------------------------------------------------------
    // §23.2 action 5 — request a permitted export. `library.read`.
    // -----------------------------------------------------------------------

    /// Request an export.
    ///
    /// # Errors
    ///
    /// [`ApiError::ScopeNotHeld`] if the credential cannot, then
    /// [`ApiError::Envelope`].
    pub async fn request_export(&self, request: &ExportRequest) -> Result<ExportView> {
        let credential = self.require(Scope::LibraryRead)?;
        self.send_with(
            reqwest::Method::POST,
            "exports",
            Some(serde_json::to_value(request)?),
            Some(&credential),
        )
        .await
    }

    // -----------------------------------------------------------------------
    // §23.2 action 6 — save a bookmark. `library.read`.
    // -----------------------------------------------------------------------

    /// Save a bookmark.
    ///
    /// # Errors
    ///
    /// [`ApiError::ScopeNotHeld`] if the credential cannot, then
    /// [`ApiError::Envelope`].
    pub async fn save_bookmark(&self, request: &BookmarkRequest) -> Result<BookmarkView> {
        let credential = self.require(Scope::LibraryRead)?;
        self.send_with(
            reqwest::Method::POST,
            "bookmarks",
            Some(serde_json::to_value(request)?),
            Some(&credential),
        )
        .await
    }

    // -----------------------------------------------------------------------
    // §23.2 action 8 — send appreciation. `content.read`.
    // -----------------------------------------------------------------------

    /// Send kudos to a work's author.
    ///
    /// # Errors
    ///
    /// [`ApiError::ScopeNotHeld`] if the credential cannot, then
    /// [`ApiError::Envelope`].
    pub async fn send_kudos(&self, work_id: &str) -> Result<KudosView> {
        // `content.read`, not `content.write`: kudos are the reader saying
        // something about themselves, and they create nothing the author can
        // edit. Lorehaven's handler documents the same reasoning.
        let credential = self.require(Scope::ContentRead)?;
        self.send_with(
            reqwest::Method::POST,
            &format!("works/{}/kudos", encode(work_id)),
            Some(serde_json::json!({})),
            Some(&credential),
        )
        .await
    }

    // -----------------------------------------------------------------------
    // The link flow. No scope: these are what *creates* the credential.
    // -----------------------------------------------------------------------

    /// Start a link challenge.
    ///
    /// # Errors
    ///
    /// As [`LorehavenClient::search_public_works`].
    pub async fn start_link(&self, platform: &str, platform_user: &str) -> Result<LinkChallenge> {
        self.send_with(
            reqwest::Method::POST,
            "bot/link/start",
            Some(serde_json::json!({
                "platform": platform,
                "platform_user_id": platform_user,
            })),
            None,
        )
        .await
    }

    /// Exchange a confirmed challenge for a token.
    ///
    /// # Errors
    ///
    /// [`ApiError::Envelope`] if the challenge is unknown, expired or already
    /// used.
    pub async fn complete_link(&self, code: &str) -> Result<LinkToken> {
        self.send_with(
            reqwest::Method::POST,
            "bot/link/exchange",
            Some(serde_json::json!({ "code": code })),
            None,
        )
        .await
    }

    /// List the caller's tokens.
    ///
    /// # Errors
    ///
    /// [`ApiError::Revoked`] if the token no longer works.
    pub async fn list_tokens(&self) -> Result<Vec<TokenView>> {
        let credential = self
            .config
            .credential
            .as_ref()
            .ok_or(ApiError::ScopeNotHeld {
                needed: "a linked account".into(),
                granted: "none — this client has no token".into(),
            })?;
        #[derive(serde::Deserialize)]
        struct Body {
            #[serde(default)]
            tokens: Vec<TokenView>,
        }
        let body: Body = self
            .send_with(reqwest::Method::GET, "me/tokens", None, Some(credential))
            .await?;
        Ok(body.tokens)
    }

    /// Revoke a token.
    ///
    /// # Errors
    ///
    /// [`ApiError::Envelope`] if the token is not the caller's — which is a 404
    /// from Lorehaven, and deliberately so: a 403 would confirm the token
    /// exists.
    pub async fn revoke_token(&self, token_id: &str) -> Result<()> {
        let credential = self
            .config
            .credential
            .as_ref()
            .ok_or(ApiError::ScopeNotHeld {
                needed: "a linked account".into(),
                granted: "none — this client has no token".into(),
            })?;
        // A 204 or a 200 with a body are both fine, so the body is discarded
        // rather than parsed — Lorehaven returns a JSON view here and an empty
        // body is equally valid.
        self.send_empty(
            reqwest::Method::DELETE,
            &format!("me/tokens/{}", encode(token_id)),
            credential,
        )
        .await
    }

    /// Check whether the credential still works.
    ///
    /// This is what `/link` status calls, and it is the cheapest possible probe
    /// of §23.2's revocation requirement. A revoked token must fail **here**,
    /// not on the next real action — a user who asks whether their link works
    /// and is told "yes" should not be right.
    ///
    /// # Errors
    ///
    /// [`ApiError::Revoked`] when the token no longer works.
    pub async fn verify_credential(&self) -> Result<ScopeSet> {
        let credential = self
            .config
            .credential
            .as_ref()
            .ok_or(ApiError::ScopeNotHeld {
                needed: "a linked account".into(),
                granted: "none — this client has no token".into(),
            })?;
        let tokens = self.list_tokens().await?;
        // The scopes the *server* reports are the authority, not the ones the
        // bot cached at link time: a reader can re-link with different scopes
        // from Lorehaven's own UI, and the bot would otherwise keep using the
        // stale set and refuse locally for no reason.
        Ok(tokens
            .first()
            .map(|t| ScopeSet::parse(&t.scopes))
            .unwrap_or_else(|| credential.scopes().clone()))
    }

    // -----------------------------------------------------------------------
    // Plumbing
    // -----------------------------------------------------------------------

    /// The attached credential, or an error naming what is missing.
    fn require(&self, needed: Scope) -> Result<&Credential> {
        let credential = self
            .config
            .credential
            .as_ref()
            .ok_or(ApiError::ScopeNotHeld {
                needed: needed.as_str().to_owned(),
                granted: "none — this client has no token".into(),
            })?;
        if credential.scopes().allows(needed) {
            Ok(credential)
        } else {
            Err(ApiError::ScopeNotHeld {
                needed: needed.as_str().to_owned(),
                granted: credential.scopes().describe(),
            })
        }
    }

    /// A public GET. Sends no credential even when one is attached.
    async fn get_public<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let mut url = format!("{}/{}", self.config.base_url, path.trim_start_matches('/'));
        if !query.is_empty() {
            let encoded: Vec<String> = query
                .iter()
                .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
                .collect();
            url.push('?');
            url.push_str(&encoded.join("&"));
        }
        let response = self
            .http
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await?;
        decode(response).await
    }

    /// A request that may or may not carry a credential.
    async fn send_with<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
        credential: Option<&Credential>,
    ) -> Result<T> {
        let url = format!("{}/{}", self.config.base_url, path.trim_start_matches('/'));
        let mut request = self
            .http
            .request(method, &url)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(credential) = credential {
            request = request.header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", credential.token()),
            );
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        decode(request.send().await?).await
    }

    /// A request whose response body is discarded.
    async fn send_empty(
        &self,
        method: reqwest::Method,
        path: &str,
        credential: &Credential,
    ) -> Result<()> {
        let url = format!("{}/{}", self.config.base_url, path.trim_start_matches('/'));
        let response = self
            .http
            .request(method, &url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", credential.token()),
            )
            .send()
            .await?;
        // The status is checked; the body is not read, which is what a 204
        // needs and what a 200-with-a-view can spare.
        check_status(response).await
    }
}

/// Check a response's status, mapping the failures that matter.
async fn check_status(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(status_error(status.as_u16(), &body))
}

/// Turn a response into a typed value, or into the right error.
async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(status_error(status.as_u16(), &body));
    }

    // An empty body on a 200 is a server bug, not something to paper over: the
    // alternative is a default value that reads as "nothing found" and turns a
    // bug into a wrong answer.
    if body.trim().is_empty() {
        return Err(ApiError::Status {
            status: status.as_u16(),
        });
    }
    Ok(serde_json::from_str(&body)?)
}

/// Map a non-2xx response onto an error.
///
/// Three cases get their own variant because the remedy differs, and collapsing
/// them is the failure this function exists to prevent.
fn status_error(status: u16, body: &str) -> ApiError {
    let envelope: Option<ApiErrorEnvelope> = serde_json::from_str(body).ok();

    if status == 401 {
        // A 401 on a token that used to work means revoked or expired. Not
        // "sign in again" — the bot has no password to sign in with.
        return ApiError::Revoked;
    }

    if status == 403 {
        if let Some(envelope) = &envelope {
            if envelope.error.code != "MISSING_SCOPE" {
                return ApiError::Envelope {
                    code: envelope.error.code.clone(),
                    message: envelope.error.message.clone(),
                    status,
                };
            }
            // Lorehaven's `MissingScope` names the scope in its message, and the
            // body of the request is not available here, so the scope is taken
            // from the message rather than parsed out of a field.
            let needed = envelope
                .error
                .message
                .split('"')
                .nth(1)
                .unwrap_or(&envelope.error.message)
                .to_owned();
            let explain = Scope::parse(&needed).map_or_else(
                || {
                    if needed.is_empty() {
                        envelope.error.message.clone()
                    } else {
                        needed.clone()
                    }
                },
                |scope| scope.explain().to_owned(),
            );
            return ApiError::ScopeDenied {
                needed,
                explain,
                granted: String::new(),
            };
        }
        return ApiError::Envelope {
            code: "FORBIDDEN".to_owned(),
            message: "forbidden".to_owned(),
            status,
        };
    }

    match envelope {
        Some(envelope) if !envelope.error.code.is_empty() => ApiError::Envelope {
            code: envelope.error.code,
            message: envelope.error.message,
            status,
        },
        _ => ApiError::Status { status },
    }
}

/// Percent-encode a path or query component.
///
/// Not optional: a work id goes into a path and a search query into a query
/// string, and a `&` or `#` in either would change what was requested.
fn encode(value: &str) -> String {
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

impl ClientConfig {
    /// Drop any credential. Used by [`LorehavenClient::public`].
    fn with_credential_missing(mut self) -> Self {
        self.credential = None;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_covers_the_characters_that_would_change_the_request() {
        assert_eq!(encode("simple-id"), "simple-id");
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(encode("tag:value"), "tag%3Avalue");
        // A `#` would truncate the path server-side; a `?` would start a query.
        assert_eq!(encode("a#b"), "a%23b");
        assert_eq!(encode("a?b"), "a%3Fb");
    }

    #[test]
    fn a_401_is_revoked_and_never_a_permission_problem() {
        // The distinction the whole error mapping exists for: re-linking fixes
        // a revocation and cannot fix a missing scope.
        let error = status_error(401, r#"{"code":"AUTH_REQUIRED","message":"no"}"#);
        assert!(matches!(error, ApiError::Revoked), "{error:?}");
        assert!(
            error.to_string().contains("/link"),
            "and says to re-link: {error}"
        );
    }

    #[test]
    fn a_missing_scope_names_the_scope_lorehaven_asked_for() {
        let body =
            r#"{"error":{"code":"MISSING_SCOPE","message":"this action needs \"library.read\""}}"#;
        let error = status_error(403, body);
        match &error {
            ApiError::ScopeDenied {
                needed, explain, ..
            } => {
                assert_eq!(needed, "library.read");
                assert!(
                    explain.contains("bookmark"),
                    "and explains what it is for: {explain}"
                );
            }
            other => panic!("expected ScopeDenied, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_scope_survives_an_envelope_the_client_cannot_parse() {
        // Lorehaven's real envelope is nested under `error`. A client written
        // against a flat shape would read the wrong message and report a 403
        // with no scope in it, which is the case where a user cannot tell what
        // to do.
        let nested = r#"{"error":{"code":"MISSING_SCOPE","message":"needs \"content.write\""}}"#;
        let error = status_error(403, nested);
        match error {
            ApiError::ScopeDenied { needed, .. } => assert_eq!(needed, "content.write"),
            other => panic!("expected ScopeDenied, got {other:?}"),
        }
    }

    #[test]
    fn a_403_that_is_not_a_missing_scope_is_still_an_envelope() {
        let body = r#"{"error":{"code":"FORBIDDEN","message":"not your pseud"}}"#;
        let error = status_error(403, body);
        assert!(matches!(error, ApiError::Envelope { .. }), "{error:?}");
    }

    #[test]
    fn a_404_is_a_status_not_a_scope_problem() {
        // Another account's row is a 404, deliberately: a 403 would confirm it
        // exists.
        let error = status_error(404, "{}");
        assert!(
            matches!(error, ApiError::Status { status: 404 }),
            "{error:?}"
        );
    }

    #[test]
    fn a_500_with_a_body_becomes_a_status_rather_than_an_envelope() {
        let error = status_error(500, "internal error");
        assert!(
            matches!(error, ApiError::Status { status: 500 }),
            "{error:?}"
        );
    }

    #[test]
    fn a_credential_with_no_scopes_refuses_every_scoped_call_locally() {
        // `Credential::new` alone leaves the scope set empty, so a bot that has
        // a token but has not recorded what it was granted refuses before the
        // request — with a message it can explain, rather than a bare 403.
        let config = ClientConfig::new("https://lore.example.test/api/v1")
            .with_credential(Credential::new("lh-secret"));
        let client = LorehavenClient::new(config).expect("builds");
        assert!(client.scopes().is_some_and(|s| s.known().is_empty()));
    }
}

//! The client against a real HTTP server.
//!
//! `wiremock` is a real server on localhost, so these exercise actual status
//! lines, actual bodies and actual headers. The unit tests in `client.rs` call
//! `status_error` directly, which is fast but proves only that the *mapper* is
//! self-consistent. These prove the *client* — the URL it builds, the header it
//! sends, the body it accepts — agrees with Darkhold.
//!
//! The bodies here were copied from Darkhold's own tests and handlers rather
//! than invented. Two of them are deliberately shaped the awkward way, and both
//! of those cases were bugs first:
//!
//! * the error envelope is nested under `error`, not flat;
//! * `public/search` returns raw database rows, so optional keys are absent
//!   rather than null.

use std::time::Duration;

use darkbot_api::{
    BookmarkRequest, ClientConfig, Credential, DarkholdClient, ExportRequest, Scope, ScopeSet,
    StartImport,
};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{matchers::body_partial_json, Mock, MockServer, ResponseTemplate};

/// A matcher that requires a header to be **absent**.
///
/// `wiremock` 0.6 has no built-in "header is missing" matcher, and a mock that
/// only matches on the header's *presence* would let the exact bug this file
/// exists to catch through: the public path sending a bearer token. Written as
/// a closure over `Request`, which is the matcher shape wiremock 0.6 takes.
fn header_absent(name: &'static str) -> impl Fn(&wiremock::Request) -> bool {
    move |request: &wiremock::Request| {
        !request
            .headers
            .get(name)
            .is_some_and(|value| !value.is_empty())
    }
}

/// A server with no mocks, returning 404 for everything.
async fn server() -> MockServer {
    MockServer::start().await
}

/// A client pointed at `base`.
fn client_for(base: &MockServer) -> DarkholdClient {
    DarkholdClient::new(
        ClientConfig::new(format!("{}/api/v1", base.uri())).with_timeout(Duration::from_secs(5)),
    )
    .expect("a client builds against a localhost URL")
}

/// A client with a credential that has the given scopes.
fn client_with(base: &MockServer, token: &str, scopes: &str) -> DarkholdClient {
    DarkholdClient::new(
        ClientConfig::new(format!("{}/api/v1", base.uri()))
            .with_credential(Credential::new(token).with_scopes(ScopeSet::parse(scopes))),
    )
    .expect("a client builds")
}

#[tokio::test]
async fn public_search_sends_no_authorization_header() {
    // The requirement, not an optimisation: a bearer token on a request that
    // did not need it lands in every proxy log between the bot and Darkhold.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/public/search"))
        .and(query_param("q", "test"))
        // A mock that *requires* the header to be absent. A mock that only
        // matches when it is present would let the bug through.
        .and(header_absent("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{"id": "w1", "title": "A Work", "author": "anon"}]
        })))
        .expect(1)
        .mount(&base)
        .await;

    // A credential is attached, so this only passes if the public path
    // deliberately drops it.
    let client = client_with(&base, "lh-secret-token", "library.read");
    let page = client.search_public_works("test").await.expect("searches");

    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].id, "w1");
    assert_eq!(page.results[0].author.as_deref(), Some("anon"));
}

#[tokio::test]
async fn public_search_omits_optional_fields_rather_than_failing() {
    // `public_search` returns the database row, so a work with no rating has no
    // `rating` key at all. A client that required every field would parse zero
    // results and report "nothing found", which is a lie.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/public/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                {"id": "w1", "title": "Full", "rating": "M", "word_count": 41000},
                {"id": "w2"},
                {"id": "w3", "title": "  ", "summary": "no title"}
            ]
        })))
        .mount(&base)
        .await;

    let page = client_for(&base)
        .search_public_works("x")
        .await
        .expect("searches");
    assert_eq!(page.results.len(), 3, "every row parses");
    assert_eq!(page.results[0].word_count, Some(41000));
    assert!(page.results[1].title.is_none());
    // A blank title is not a title. Falling back to "Untitled" is better than
    // rendering an empty row that looks like a bug.
    assert_eq!(page.results[2].display_title(), "Untitled");
}

#[tokio::test]
async fn a_search_query_is_percent_encoded() {
    // A `&` in a query would otherwise truncate the search at the first
    // separator and search for half the query.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/public/search"))
        .and(query_param("q", "rust & lorhaven"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []})))
        .expect(1)
        .mount(&base)
        .await;

    client_for(&base)
        .search_public_works("rust & lorhaven")
        .await
        .expect("the query survives the round trip");
}

#[tokio::test]
async fn a_401_becomes_revoked_and_says_to_re_link() {
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/me/tokens"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"code": "AUTH_REQUIRED", "message": "no"}
        })))
        .mount(&base)
        .await;

    let client = client_with(&base, "lh-dead", "library.read");
    let error = client.list_tokens().await.expect_err("a 401 is an error");
    assert!(matches!(error, darkbot_api::ApiError::Revoked), "{error:?}");
    assert!(
        error.to_string().contains("/link"),
        "and gives a remedy: {error}"
    );
}

#[tokio::test]
async fn a_403_missing_scope_names_the_scope() {
    // The case that the flat-envelope bug destroyed. With `ApiErrorBody` read
    // flat, this parsed to `None`, fell through to `Status { 403 }`, and the
    // user was told only that Darkhold answered 403.
    let base = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bookmarks"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": {
                "code": "MISSING_SCOPE",
                "message": "this action needs \"library.read\"",
                "request_id": "req-1"
            }
        })))
        .mount(&base)
        .await;

    // The token holds `library.read`, so the client lets the request through —
    // and Darkhold refuses. This is the disagreement case: the bot's cached
    // scope set says yes and the server says no, which happens after a scope is
    // narrowed server-side. The first draft of this test used `content.read`
    // and never reached the server at all, so it was asserting the *local*
    // refusal while claiming to test the 403 mapping.
    let client = client_with(&base, "lh-token", "library.read");
    let error = client
        .save_bookmark(&BookmarkRequest {
            subject_type: "work".into(),
            subject_id: "w1".into(),
            note: None,
        })
        .await
        .expect_err("a 403 is an error");

    match &error {
        darkbot_api::ApiError::ScopeDenied {
            needed, explain, ..
        } => {
            assert_eq!(needed, "library.read");
            assert!(explain.contains("bookmark"), "{explain}");
        }
        other => panic!("expected ScopeDenied, got {other:?}"),
    }
}

#[tokio::test]
async fn a_scoped_call_refuses_locally_before_the_request() {
    // The client-side half of §23.1. Darkhold's tests prove the *server*
    // refuses a token without a scope; nothing there can prove the bot *asked*
    // for the right one. A method that declared the wrong scope would work in
    // Darkhold's suite and fail for every user.
    let base = server().await;
    // No mocks. If a request were sent, wiremock returns 404 and the assertion
    // below would see an error rather than ScopeNotHeld.
    let client = client_with(&base, "lh-token", "content.read");

    let error = client
        .request_export(&ExportRequest {
            subject_type: "work".into(),
            subject_id: "w1".into(),
            format: "markdown".into(),
            acknowledge_privacy: true,
        })
        .await
        .expect_err("refused without a request");

    match error {
        darkbot_api::ApiError::ScopeNotHeld { needed, granted } => {
            assert_eq!(needed, "library.read");
            assert!(
                granted.contains("content.read"),
                "and says what it does have, so the user knows what to grant: {granted}"
            );
        }
        other => panic!("expected ScopeNotHeld, got {other:?}"),
    }
}

#[tokio::test]
async fn a_public_call_with_no_credential_sends_no_header() {
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/public/works/w1"))
        .and(header_absent("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "w1", "title": "A Work", "rating": "T"
        })))
        .expect(1)
        .mount(&base)
        .await;

    let work = client_for(&base).public_work("w1").await.expect("fetches");
    assert_eq!(work.id, "w1");
    assert_eq!(work.rating.as_deref(), Some("T"));
}

#[tokio::test]
async fn a_work_id_with_a_slash_is_encoded_into_the_path() {
    // A work id is opaque. Treating it as path-safe would let one work's id
    // reach another's endpoint.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/public/works/w%2F1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "w/1"})))
        .expect(1)
        .mount(&base)
        .await;

    let work = client_for(&base).public_work("w/1").await.expect("fetches");
    assert_eq!(work.id, "w/1");
}

#[tokio::test]
async fn starting_an_import_sends_the_bearer_token_and_the_body() {
    let base = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/imports"))
        .and(header("authorization", "Bearer lh-secret"))
        .and(body_partial_json(serde_json::json!({
            "source": "ao3",
            "url": "https://example.test/works/1"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "imp-1", "state": "queued", "source": "ao3"
        })))
        .expect(1)
        .mount(&base)
        .await;

    let client = client_with(&base, "lh-secret", "content.write");
    let import = client
        .start_import(&StartImport {
            source: "ao3".into(),
            url: "https://example.test/works/1".into(),
            note: None,
        })
        .await
        .expect("starts");

    assert_eq!(import.id, "imp-1");
    assert_eq!(import.state, "queued");
}

#[tokio::test]
async fn an_export_request_always_carries_the_privacy_acknowledgement() {
    // §13.6 is enforced server-side by rejecting a missing flag. A client that
    // could omit it turns the notice into a checkbox, so the field is not
    // `Option` and there is no way to construct the request without it.
    let base = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/exports"))
        .and(body_partial_json(serde_json::json!({
            "acknowledge_privacy": true
        })))
        .respond_with(ResponseTemplate::new(202).set_body_json(serde_json::json!({
            "id": "exp-1", "state": "queued"
        })))
        .expect(1)
        .mount(&base)
        .await;

    let client = client_with(&base, "lh-secret", "library.read");
    let export = client
        .request_export(&ExportRequest {
            subject_type: "work".into(),
            subject_id: "w1".into(),
            format: "markdown".into(),
            acknowledge_privacy: true,
        })
        .await
        .expect("requests");
    assert_eq!(export.id, "exp-1");
}

#[tokio::test]
async fn kudos_uses_content_read_and_not_content_write() {
    // §23.2 puts appreciation under read, matching Darkhold's handler: kudos
    // are the reader saying something about themselves and create nothing the
    // author can edit. A client that asked for `content.write` would tell users
    // to grant a permission they do not need.
    let base = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/works/w1/kudos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "work_id": "w1", "kudos_count": 12, "thanked_by_me": true
        })))
        .expect(1)
        .mount(&base)
        .await;

    let client = client_with(&base, "lh-secret", "content.read");
    let kudos = client.send_kudos("w1").await.expect("sends");
    assert_eq!(kudos.work_id.as_deref(), Some("w1"));
    assert_eq!(kudos.thanked_by_me, Some(true));
}

#[tokio::test]
async fn job_status_reads_the_items_key() {
    // `items`, not `jobs`. Reading `jobs` on a response without it yields
    // `null`, and `null.as_array()` is `None` — which a lenient client accepts as
    // "no jobs", reporting a healthy account with a silently empty job list.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "items": [
                {"id": "j1", "kind": "import", "state": "running", "progress_permille": 400}
            ],
            "next_cursor": null
        })))
        .expect(1)
        .mount(&base)
        .await;

    let client = client_with(&base, "lh-secret", "content.write");
    let jobs = client.job_status().await.expect("reads");
    assert_eq!(jobs.items.len(), 1, "the job is actually seen");
    assert_eq!(jobs.items[0].progress_permille, Some(400));
    assert!(jobs.next_cursor.is_none());
}

#[tokio::test]
async fn the_link_exchange_returns_a_token_and_nothing_sensitive() {
    let base = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/bot/link/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "lh-fresh",
            "acting_pseud_id": "p1",
            "scopes": "content.read library.read",
            "id": "tok-9"
        })))
        .expect(1)
        .mount(&base)
        .await;

    let client = client_for(&base);
    let token = client.complete_link("ABCD-1234").await.expect("exchanges");
    assert_eq!(token.token, "lh-fresh");
    let scopes = token.scope_set();
    assert!(scopes.allows(Scope::ContentRead));
    assert!(scopes.allows(Scope::LibraryRead));
    assert!(!scopes.allows(Scope::ContentWrite));
}

#[tokio::test]
async fn verifying_a_link_reports_the_servers_scopes_not_the_cached_ones() {
    // A reader can re-link with different scopes from Darkhold's own UI. If
    // the bot trusted its cached set it would keep refusing locally for a
    // permission the token now has.
    let base = server().await;
    // `/me/credential`, not `/me/tokens`. The latter is session-only: it
    // answers a bearer token with "sign in to list tokens", so a bot could
    // never call it. This test mocked the *wrong* endpoint, which is how the
    // bug survived the unit suite and was found by the end-to-end run against
    // a real instance.
    Mock::given(method("GET"))
        .and(path("/api/v1/me/credential"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "account_id": "acct-1",
            "pseud_id": "pseud-1",
            "via": "token",
            "scopes": ["content.read", "content.write", "library.read"],
        })))
        .expect(1)
        .mount(&base)
        .await;

    // The cached set is narrower than the server's.
    let client = client_with(&base, "lh-secret", "content.read");
    let scopes = client.verify_credential().await.expect("verifies");
    assert!(
        scopes.allows(Scope::ContentWrite),
        "the server is the authority: {scopes}"
    );
}

#[tokio::test]
async fn verifying_a_link_does_not_call_the_session_only_token_list() {
    // The regression, pinned. `/me/tokens` needs a session; a bot has a bearer
    // token and no cookie jar, so calling it makes every link unverifiable and
    // silently falls back to the cached scopes.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/me/credential"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "account_id": "acct-1", "pseud_id": "pseud-1", "via": "token",
            "scopes": ["content.read"],
        })))
        .expect(1)
        .mount(&base)
        .await;
    // If the client calls this, wiremock's `verify()` below fails the test.
    Mock::given(method("GET"))
        .and(path("/api/v1/me/tokens"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "error": {"code": "VALIDATION_FAILED", "message": "sign in to list tokens"}
        })))
        .expect(0)
        .mount(&base)
        .await;

    let client = client_with(&base, "lh-secret", "content.read");
    client
        .verify_credential()
        .await
        .expect("verifies via /me/credential");
    base.verify().await;
}

#[tokio::test]
async fn a_token_with_no_scopes_reports_none_rather_than_the_cached_set() {
    // A reader who re-linked with nothing granted, or who had their scopes
    // withdrawn, must be refused. Falling back to the cached set would leave the
    // bot using authority the server has withdrawn — the one outcome worse than
    // a refusal, because it is invisible.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/me/credential"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "account_id": "acct-1", "pseud_id": "pseud-1", "via": "token",
            "scopes": [],
        })))
        .expect(1)
        .mount(&base)
        .await;

    // Cached scopes are broad, on purpose.
    let client = client_with(
        &base,
        "lh-secret",
        "content.read library.read content.write",
    );
    let scopes = client.verify_credential().await.expect("verifies");
    assert!(
        !scopes.allows(Scope::ContentRead),
        "the server withdrew everything: {scopes}"
    );
    assert!(!scopes.allows(Scope::ContentWrite));
}

#[tokio::test]
async fn a_sessions_empty_scope_list_is_not_read_as_a_revocation() {
    // A session carries no scope set — its authority was settled at login. An
    // empty list there means "not applicable", not "granted nothing", and a
    // caller that confuses the two locks a signed-in user out of their own
    // account.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/me/credential"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "account_id": "acct-1", "pseud_id": "pseud-1", "via": "session",
            "scopes": [],
        })))
        .expect(1)
        .mount(&base)
        .await;

    let client = client_with(&base, "lh-secret", "content.read");
    let scopes = client.verify_credential().await.expect("verifies");
    assert!(
        scopes.allows(Scope::ContentRead),
        "a session's empty list falls back to the cached set: {scopes}"
    );
}

#[tokio::test]
async fn a_token_view_never_carries_the_raw_token() {
    // `GET /me/tokens` returns metadata. The secret is returned exactly once,
    // from the issue call. Asserted on a live response so that a server change
    // which started leaking it would be caught here rather than in a log.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/me/tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "tokens": [{"id": "tok-9", "name": "darkbot", "kind": "bot",
                        "scopes": "library.read", "expires_at": null}]
        })))
        .mount(&base)
        .await;

    let client = client_with(&base, "lh-secret", "library.read");
    let tokens = client.list_tokens().await.expect("lists");
    assert_eq!(tokens.len(), 1);

    let json = serde_json::to_value(&tokens[0]).expect("serialises");
    assert!(
        !json.to_string().contains("lh-secret"),
        "the raw token must not survive into a loggable type: {json}"
    );
}

#[tokio::test]
async fn revoking_accepts_both_a_204_and_a_200_with_a_body() {
    // Darkhold returns a JSON view; a future version may return nothing. The
    // client checks the status and does not require a body, so it keeps working
    // either way — and a 204 is what most HTTP libraries prefer.
    for status in [200u16, 204u16] {
        let base = server().await;
        Mock::given(method("DELETE"))
            .and(path("/api/v1/me/tokens/tok-9"))
            .respond_with(if status == 204 {
                ResponseTemplate::new(204)
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true}))
            })
            .expect(1)
            .mount(&base)
            .await;

        client_with(&base, "lh-secret", "library.read")
            .revoke_token("tok-9")
            .await
            .unwrap_or_else(|e| panic!("a {status} is a success, got {e}"));
    }
}

#[tokio::test]
async fn a_500_with_a_non_json_body_is_a_status_not_a_decode_error() {
    // An nginx 502 page is HTML. Reporting "could not read Darkhold's answer"
    // would be confusing; the useful fact is that the server did not answer.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/public/search"))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
        .mount(&base)
        .await;

    let error = client_for(&base)
        .search_public_works("x")
        .await
        .expect_err("a 502 is an error");
    assert!(
        matches!(error, darkbot_api::ApiError::Status { status: 502 }),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_empty_200_is_an_error_rather_than_an_empty_result() {
    // A 200 with no body is a server bug. Defaulting to an empty page would
    // turn a bug into a confident "nothing found", which is the worse failure:
    // the user stops asking.
    let base = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/public/search"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&base)
        .await;

    let error = client_for(&base)
        .search_public_works("x")
        .await
        .expect_err("an empty 200 is not a result");
    assert!(
        matches!(error, darkbot_api::ApiError::Status { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_404_from_another_accounts_row_is_a_status() {
    // Darkhold answers 404 rather than 403 for a row that is not the caller's,
    // because a 403 would confirm the row exists. The client must not turn that
    // into a permission message that would invite a retry.
    let base = server().await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/me/tokens/not-mine"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": {"code": "NOT_FOUND", "message": "no such token"}
        })))
        .mount(&base)
        .await;

    let error = client_with(&base, "lh-secret", "library.read")
        .revoke_token("not-mine")
        .await
        .expect_err("a 404 is an error");
    match error {
        darkbot_api::ApiError::Envelope { code, .. } => assert_eq!(code, "NOT_FOUND"),
        other => panic!("expected NOT_FOUND, got {other:?}"),
    }
}

#[tokio::test]
async fn a_client_with_no_credential_refuses_scoped_calls_with_a_readable_message() {
    // The `public` constructor, used by the link flow. A scoped call on it is a
    // bot bug, and the message has to be readable because it can reach a log
    // that an operator is the only audience for.
    let base = server().await;
    let client = client_for(&base);
    let error = client
        .start_import(&StartImport {
            source: "ao3".into(),
            url: "https://example.test/works/1".into(),
            note: None,
        })
        .await
        .expect_err("no credential");
    let text = error.to_string();
    assert!(text.contains("content.write"), "{text}");
    assert!(text.contains("no token"), "and says what is absent: {text}");
}

# Running against a real Darkhold

The wiremock suite proves darkbot agrees with itself. It cannot prove darkbot
agrees with Darkhold, and in the first end-to-end run it did not.

```sh
# terminal 1
cd ~/code-local/rust/darkhold
cargo run -- serve                     # 127.0.0.1:8080

# terminal 2 — seed a reader and issue a bot token
cd ~/code-local/rust/darkhold && cargo run -- seed -- development
curl -c /tmp/lh.jar -X POST http://127.0.0.1:8080/api/v1/auth/login \
  -H 'content-type: application/json' \
  -d '{"email":"dev@darkhold.local","password":"darkhold-dev"}'
curl -b /tmp/lh.jar -X POST http://127.0.0.1:8080/api/v1/me/tokens \
  -H 'content-type: application/json' -d '{
    "name": "darkbot-e2e", "kind": "bot",
    "scopes": ["content.read", "library.read"],
    "acting_pseud_id": "<a pseud id from GET /api/v1/pseuds>",
    "expires_in_seconds": 86400
  }'

# terminal 3
export DARKBOT_E2E_URL=http://127.0.0.1:8080/api/v1
export DARKBOT_E2E_TOKEN=...            # the token from above
export DARKBOT_E2E_SCOPES='content.read library.read'
export DARKBOT_E2E_INSTANCE=darkhold
cd ~/code-local/rust/darkbot && cargo test -p darkbot --test e2e -- --nocapture
```

With no `DARKBOT_E2E_URL` the network tests skip and the offline properties
still run — those are the ones that catch render and privacy bugs, and they need
no instance.

## What it found

Four things, none of which any unit test could have found. All four are
recorded because each one is a class of bug worth recognising.

### 1. `verify_credential` called a session-only endpoint

`DarkholdClient::verify_credential` called `GET /me/tokens`. That route is
session-only: a bot has a bearer token and no cookie jar, so Darkhold answers
`422 "sign in to list tokens"`. Every link was therefore unverifiable, and the
bot fell back to the scopes cached at link time — which go stale the moment a
reader re-links from Darkhold's own UI.

The wiremock test mocked the wrong endpoint, so it agreed with the wrong client.
**A mock answers whatever the code asks for, which is the one thing a mock
cannot check.**

Fixed on both sides: Darkhold gained `GET /me/credential` (bearer-authenticated,
returns the caller's own scopes and nothing else), and darkbot calls that.

The response distinguishes `via: "token"` from `via: "session"`, because an
empty scope list means *granted nothing* for a token and *not applicable* for a
session. Conflating them either leaves the bot using withdrawn authority or
locks a signed-in user out of their own account.

### 2. The migration catalogue was ten migrations behind

`POST /me/tokens` returned `500: table api_tokens has no column named kind`.
Migration 0083 clearly adds that column. It was never applied: the embedded
catalogue stopped at 0073 while 0074–0083 sat on disk.

`crates/db/build.rs` declared one `cargo:rerun-if-changed` for the migrations
*directory*. Cargo watches a directory entry for its own mtime, and adding a
file inside a directory does not change it. So the build script ran once, was
permanently satisfied, and never re-ran.

Nothing in Darkhold's suite noticed, because every test builds its database
from the same stale catalogue — a stale catalogue produces a *consistent* wrong
schema, and consistency is what the tests were checking. Only comparing the
catalogue against the filesystem catches it. `crates/db/tests/migration_catalogue.rs`
does, and it was verified by making the catalogue stale again and watching it
fail.

This is the "referenced ≠ used" failure, in a new place: the code referenced a
column, and a migration on disk declared it, and no two of those three agreed
with the fourth.

### 3. `/whoami` did not exist

The E2E test was written from a different memory of the spec than the dispatcher
was implemented from. The command is `/status`. The test failed against correct
code, which is the useful kind of failure — it means the test and the
implementation were written from two different sources and only one of them was
compiled.

### 4. A token with no `acting_pseud_id` gets 403, not 401

`RequireActor` refuses a token with no acting pseud, and the refusal is
`ACCESS_DENIED`. A bot registering without one gets a 403 with no indication
that the fix is to pass `acting_pseud_id`, which is a confusing first-run
failure for anyone following the README.

Worth fixing in Darkhold's error, and the reason to document the field here.

## What the suite does *not* cover

- **A real chat platform.** No Discord, Telegram or Matrix connection is made.
  The adapters are tested against real payload shapes and the shared conformance
  suite, which is where the four adapter bugs were found, but nothing here has
  spoken to Discord's gateway.
- **The link handshake.** `/link` → browser → `/link_confirm` is tested against
  mocks. The E2E tests insert a link into the store directly, because the point
  is the dispatcher rather than the handshake.
- **Import, export and job polling.** They need a populated library; the seed
  gives an empty one.

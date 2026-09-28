# lorebot

A thin REST client for your Lorehaven instance, on Discord, Telegram and Matrix.

It is deliberately thin. It holds no database, no reader's works, no vector
index, and no copy of anything Lorehaven already stores. It reads a command,
calls one API endpoint, renders the answer, and puts it in a chat.

```
Discord / Telegram / Matrix
        │  a command, a platform user id
        ▼
   lorebot ──POST /me/chat/actions──▶  your Lorehaven
        │                              (scoped token, no db access here)
        │◀──  typed reply
        ▼
   a chat, private where the platform can
```

## What it will not do

These are enforced by tests, not by convention. A change that breaks one of them
fails the build.

- **No database driver, anywhere.** Not sqlx, not diesel, not a Postgres URI.
  `lorebot-core` has an assertion that fails the test if a source file imports
  one, and `scripts/prove-db-assertion.sh` proves the assertion itself works by
  adding a driver and watching it fail.
- **No outbound webhook.** Lorehaven is never told where a message went. The
  API takes the platform actor as a string and returns no delivery information,
  so there is nowhere to report a destination to. See `docs/PRIVACY.md`.
- **No claims, no scoring, no writing.** Eight read-only actions, in
  `lorebot-core/src/dispatch.rs`.
- **No plaintext tokens on disk.** Reader tokens are sealed with AES-256-GCM
  under a key you provide, and the key is rejected if it is too short rather
  than padded.

## Build and test

```sh
cargo test --workspace     # 240 tests
cargo clippy --all-targets
cargo fmt --all --check
./scripts/prove-db-assertion.sh
```

`prove-db-assertion.sh` is the one worth reading before trusting the first line.
It has three cases: adding a database driver must fail the suite, a
hand-rolled connection string must fail the source scan, and the clean tree must
pass. A test that cannot fail is not a test.

## Configure

Everything is environment variables. There is no config file, because a config
file is somewhere for a token to be forgotten.

| Variable | Required | Meaning |
|---|---|---|
| `LOREBOT_LOREHAVEN_URL` | yes | Your instance's API root, e.g. `https://lore.example.org/api/v1` |
| `LOREBOT_STORE_KEY` | yes | 32 bytes, base64. `openssl rand -base64 32` |
| `LOREBOT_DISCORD_TOKEN` | one of | Discord bot token |
| `LOREBOT_TELEGRAM_TOKEN` | one of | Telegram bot token from @BotFather |
| `LOREBOT_MATRIX_TOKEN` | one of | `homeserver\|access_token\|@bot:server` |
| `LOREBOT_STORE_PATH` | no | Defaults to `~/.local/share/lorebot/store.bin` |
| `LOREBOT_EPHEMERAL_STORE` | no | `1` keeps links in memory only. Test deployments. |
| `RUST_LOG` | no | Defaults to `info` |

Startup validates all of it and fails with a message you can act on:

```
$ lorebot
lorebot: configuration error: LOREBOT_STORE_KEY must be 32 bytes, got 5.
Generate one with `openssl rand -base64 32`. A shorter key would have to be
padded or truncated, and truncation means two different configured keys can
open the same store.
```

The default log level is `info` and not `debug` on purpose: `debug` on an HTTP
client logs request URLs, and a Lorehaven URL can carry a credential.

## Link a reader

```
/link
```

The bot posts a link challenge; the reader opens it in a browser and confirms.
Their Lorehaven token is then sealed into the store. `/unlink` removes it and
zeroizes it. Nothing else about them is kept — see `docs/PRIVACY.md`.

## Commands

| | |
|---|---|
| `/link`, `/unlink`, `/whoami` | account, and the instance you are linked to |
| `/search <query>` | full-text search across your works |
| `/recent [n]` | what you added lately |
| `/similar <work>` | works like it |
| `/stats` | counts |
| `/help` | this list, privately |

A result that is private to you is sent privately where the platform can. On
Matrix it cannot be — see below.

## Platform notes

`docs/ADAPTERS.md` has the full table and the reasoning. The short version:

- **Discord** is the only platform where an ephemeral reply is genuinely
  ephemeral: `flags: 64`, not in the channel, no message-create event fired.
- **Telegram** uses long polling, so it works on a laptop with no public URL.
- **Matrix** cannot do private messages. An event sent to a room is in that
  room, in every client's history, in the homeserver's database, forever. So
  the adapter refuses a private message and tells the reader why, rather than
  posting a private library result to a room. A Matrix deployment that needs
  this needs a DM room, and the bot will point at it.

  This one was wrong in the first draft: `MATRIX.ephemeral` was `true`, which
  did not make Matrix private — it made the adapter post a private result to a
  room while the capability table claimed it was safe. An end-to-end test caught
  it, and four tests that encoded the same wrong assumption had to be rewritten
  to assert the refusal instead.

## Layout

```
crates/
  lorebot-core/     IR, privacy rule, config, encrypted token store, dispatcher, cache
  lorebot-api/      typed client for Lorehaven's public API, wiremock-tested
  lorebot-adapter/  the Adapter trait and the conformance suite
  lorebot-discord/  Discord: gateway events, ephemeral sends, components
  lorebot-telegram/ Telegram: long polling, inline keyboards, 1:1 privacy
  lorebot-matrix/   Matrix: /sync, m.new_content, HTML escaping
  lorebot/          the binary: config -> store -> dispatcher -> adapters
docs/
  PRIVACY.md        what Lorehaven knows, and what it does not
  ADAPTERS.md       per-platform capabilities and the reasoning
  SPEC.md           section 23.2
```

The conformance suite in `lorebot-adapter` is the reason to trust the adapters.
Every adapter must pass it, and it earns its keep: it caught Telegram silently
discarding every action row while advertising `buttons: true`, Matrix emitting
buttons it had declared it did not support, and a Discord mention stripper that
deleted *other people's* mentions from a reader's sentence.

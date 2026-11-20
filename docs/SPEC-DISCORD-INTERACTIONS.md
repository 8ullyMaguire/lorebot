# Discord interactions redesign for darkbot

Status: decided, not implemented. Owner decisions recorded 2026-11-20.

## Problem

darkbot currently runs a Discord gateway, reads plain chat messages and parses leading `/help`, `/search` and friends. That requires Discord's privileged MESSAGE_CONTENT intent and makes the bot appear silent when the operator invites it but never enables the portal switch. Ascend solved the same class of problem with a signed HTTP interactions endpoint: Discord sends a command directly to a URL; no gateway, no message-content review, and real slash commands appear in Discord's `/` picker.

Census (commands run 2026-11-20):
- `wc -l` of `ascend/backend/crates/ascend-api/src/routes/discord.rs` = 818 lines. That route owns signature verification, PING, dispatch, account links and its own fics/session state.
- darkbot occupancy from its own roadmap note = 12,489 lines across `darkbot-core`, `darkbot-adapter`, `darkbot-api`, and Discord/Matrix/Telegram adapters. It owns the dispatcher, account-link store and platform-neutral `PlatformMessage` IR.
- `rg` by entity found darkbot; `rg` by mechanism (`interactions`, `x-signature-ed25519`, `gateway`) found Ascend's route and darkbot's gateway loop. The two are different mechanisms, not two copies of one kit.

Decision: add an interactions transport to darkbot. Do not copy Ascend's 818-line route into darkhold's API: the parts worth sharing are the signature-verification contract and the interaction reply envelope, not Ascend's fics/session handlers. Ascend remains the reference implementation for that contract; darkbot keeps its own dispatcher and store.

## Goals

1. Discord users can type real registered slash commands under one root, `/darkhold`.
2. All current darkbot actions remain reachable: help, search, work, kudos, import, jobs, bookmark, export, status, unlink, privacy.
3. Discord no longer needs MESSAGE_CONTENT. Matrix and Telegram keep their existing gateway command style.
4. The endpoint refuses unsigned requests before parsing JSON, and is inert when no public key is configured.
5. Command registration is repeatable from code (`darkbot discord-register [--guild ID]`), with guild-only rollout first.
6. Existing account links and `PlatformMessage` rendering are reused, not reimplemented.

## Non-goals

- No Discord gateway for v1. The gateway code may remain in the tree but must be disabled by default for Discord.
- No message context menu in v1. It is useful for archiving links, but darkbot's current product is commands, not channel watching.
- No migration of darkbot's link store into darkhold's database. A bot that holds a reader's token stays out of the product DB, per the existing §23.2 decision.
- No change to Ascend. It stays the model implementation.
- No extraction of a shared crate yet. One donor and one recipient with different state types is enough to write a contract document, not enough to justify a package with a version number.

## Architecture

- `crates/darkbot/src/main.rs` gains a third run mode: `serve-interactions`. It builds the existing `BotConfig`, `Store`, and `Dispatcher`, then starts an Axum listener.
- A new `crates/darkbot/src/interactions.rs` owns:
  - `verify(headers, body) -> bool`, ed25519 over `timestamp || raw_body`, with no JSON parse before verification.
  - the `POST /discord/interactions` handler: 404 `discord_not_configured` when `DARKBOT_DISCORD_PUBLIC_KEY` is unset; 401 `invalid request signature` for bad signatures; PING returns `{"type":1}`.
  - mapping a Discord interaction to `Actor { platform: Discord, platform_user_id: <user id> }` and a `Command { name, args }` the existing `Dispatcher::run` already understands.
  - mapping returned `PlatformMessage`s into Discord interaction responses (`type 4`, ephemeral flag where the message requests it).
- `crates/darkbot/src/main.rs` also gains `discord-register [--guild ID]` and `discord-status` subcommands, following Ascend's admin verbs but using `DISCORD_BOT_TOKEN` / `DISCORD_APPLICATION_ID` from the bot's own env file.
- Configuration additions:
  - `DARKBOT_DISCORD_PUBLIC_KEY` — public key from Developer Portal → General Information. No secret.
  - `DISCORD_APPLICATION_ID` — read only by the register/status subcommand.
  - `DISCORD_BOT_TOKEN` — read only by the register subcommand; never placed in the service's runtime env.
  - `DARKBOT_INTERACTIONS_BIND` — default `127.0.0.1:8087`; production exposes it through the existing reverse proxy at `https://darkhold.polarisocial.xyz/api/discord/interactions`.
  - `DARKBOT_DISCORD_GATEWAY` — default `0`; opt-in only for rollback. Matrix/Telegram are unaffected.

## Command surface

One root command, `/darkhold`, with subcommands. All option names are lowercase and match the existing dispatcher's argument order.

| Subcommand | Options | Dispatcher command |
|---|---|---|
| help | none | `/help` |
| search | `words` (required string) | `/search <words>` |
| work | `id` (required string) | `/work <id>` |
| kudos | `id` (required string) | `/kudos <id>` |
| import | `url` (required string) | `/import <url>` |
| jobs | none | `/jobs` |
| bookmark | `id` (required string) | `/bookmark <id>` |
| export | `id` (required string) | `/export <id>` |
| status | none | `/status` |
| unlink | none | `/unlink` |
| privacy | none | `/privacy` |

Rejected alternatives: one top-level command per action (clutters the `/` picker with 11 entries); `/ascend`-style root (wrong product noun); `/dh` (terser but less discoverable). These are low-stakes naming decisions recorded here so they are not re-litigated.

## Security and data

- Verify before parse. A bad signature cannot cause a store lookup or an outbound Darkhold call.
- Signed-but-unknown interaction types get a polite reply, not a panic.
- No credential enters the repository or the summary. The public key is public; the bot token is only used to register commands.
- No new table, no migration, no schema change. The link store keeps its current shape.
- An interaction user id is an opaque string; do not infer a Darkhold account from it. The existing link flow remains authoritative.

## Acceptance criteria

Each row gets a command that can fail:

1. Unset public key: `POST /discord/interactions` returns 404 with `discord_not_configured`.
2. Unsigned body: 401 with `invalid request signature`, and no dispatcher call.
3. Wrong-key signature: 401.
4. Correct PING signature: `{"type":1}`.
5. Correct `/darkhold help` interaction: response type 4 containing the existing `help_text()` string.
6. Slash registration dry output: `darkbot discord-register --guild <id>` prints 12 registered command definitions (one root + 11 subcommands in one array, so one command definition with 11 subcommands) and GET confirms the stored names.
7. Live guild proof: after guild registration, a human types `/darkhold help` and sees the reply; then the same for `/darkhold search words:...`.
8. Regression: MESSAGE_CONTENT remains disabled in the portal and Discord still receives commands.
9. Matrix/Telegram gateways still start and answer their plain-text commands.

## Order / milestones

1. M1 — verifier, endpoint, PING, inert mode, registration verbs. No dispatcher mapping yet; PING and unsigned probes are the proof.
2. M2 — command mapping into the existing dispatcher and `PlatformMessage` → interaction response conversion, with signed end-to-end tests.
3. M3 — disable Discord gateway by default, keep Matrix/Telegram; deploy and register guild-only; live proof.
4. M4 — after owner approval, global registration and removal/retirement of the gateway code path.

Milestone 1 is planned in detail below; later milestones get a detailed section immediately before implementation.

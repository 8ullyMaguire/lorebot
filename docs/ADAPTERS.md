# Adapters

One page per platform, saying what each one can do, what it deliberately cannot,
and what each of those decisions costs. Read this before adding a fourth.

The shared contract is in `lorebot-adapter/src/lib.rs`. The conformance suite
that every adapter must pass is in `lorebot-adapter/src/conformance.rs`, and
`Adapter::capabilities()` is the single declaration of what a platform can do —
a renderer that contradicts it fails the suite.

## The capabilities, and why each is what it is

| | Discord | Telegram | Matrix |
|---|---|---|---|
| ephemeral / private | yes (`flags: 64`) | yes (1:1 chat) | **no** |
| buttons | yes | yes (inline keyboard) | no — text |
| files | yes | caption | no |
| mention to address | required in guilds | `@botusername` or a reply | `m.room.message` or a reply |
| transport | gateway (REST for sends) | long polling | `/sync` |

`Matrix: ephemeral = no` is the interesting one, and the reason this bot's
privacy guarantee is enforced by more than good behaviour. Discord hides an
ephemeral message from the channel; Telegram has a 1:1 chat. Matrix has neither:
an event sent to a room is in that room, in every client's history, in the
homeserver's database, forever. A Matrix deployment where the bot can read a
reader's private library result has to know that, so the adapter refuses the
message and says so — see `docs/PRIVACY.md`.

## Discord

**Deliberately absent, with reasons:**

- **Sharding.** Required above 2500 guilds. A companion bot for one instance is
  in a handful of rooms; an operator who needs sharding is running a different
  product, and the error they get says so.
- **Resume.** A dropped connection drops the session. For a bot that answers
  commands a reconnect is cheap; a resume buffer that replays messages the
  reader already saw is worse.
- **Privileged intents beyond two.** `GUILDS | MESSAGE_CONTENT`, asserted in a
  test. Both are privileged and Discord surfaces them to an administrator when
  the bot is invited. Adding a third should be a test failure, not a surprise in
  an audit.

**The one that matters:** ephemeral replies are a `flags: 64` on a single
message. Discord does not put it in the channel, does not show it to anyone but
the recipient, and does not fire a message-create event for it. Lorehaven still
cannot know where it went, but the reader's private library result is now
subject to Discord's storage policy rather than the bot's good behaviour.

**Not wired:** the gateway websocket. `on_payload` is the whole event path and
is tested against real payload shapes; what is missing is the socket
(`tungstenite` or `tokio-tungstenite`) plus the identify/heartbeat loop around
it. The REST half — which is where every private message is *sent*, and so the
half the privacy guarantee depends on — is complete. Add the socket without
changing `on_payload`.

## Telegram

**Long polling**, not a webhook. A webhook needs a public HTTPS URL and a
certificate; a bot for a self-hosted Lorehaven usually has neither, and long
polling works on a laptop. `getUpdates` with an offset is exactly one HTTP call
in a loop, and the loop is the whole transport.

**Addressing.** Telegram has no mention flag. A group message is addressed when
it carries `/command@botusername` or is a reply to the bot. A private chat is
addressed unconditionally — Telegram does not deliver a 1:1 message that was not
meant for the bot, and requiring a mention would make the bot silent in the
place readers use it most. The `@username` suffix is stripped before the core
sees the command, because a core that knows Telegram's command syntax is a core
that will get it wrong for the next platform.

**Private sends.** A private destination holds the reader's *user* id, which is
not a chat id, and the mapping is only known from traffic: a reader who has
never messaged the bot directly has no private chat to send to. So a private
send with no known chat is an **error**, not a fallback to the conversation.
The fallback is the leak §23.2 exists to prevent, reached by one plausible
looking `unwrap_or`.

## Matrix

**No ephemeral, and that is not a gap.** Matrix events land in the room. The
adapter returns `AdapterError::CannotSendPrivately` and the binary tells the
reader why, rather than posting a private library result to a room.

**HTML escaping before Markdown conversion.** A work's title comes from
Lorehaven, so from a user, and `formatted_body` is HTML. An unescaped title is
stored XSS in every client's event history. The order matters and is asserted:
escape the source, *then* apply the four Markdown patterns, because the reverse
order re-introduces raw angle brackets.

**Redacted events.** `m.redacted` arrives as an event with empty `content` and
an `unsigned.redacted_because`. It is dropped — the bot never acts on the
absence of a message.

## Adding a platform

1. A new crate `crates/lorebot-<name>`. **No database driver** — the assertion
   in `lorebot-core/src/lib.rs` and `scripts/prove-db-assertion.sh` will fail
   the build.
2. `impl Adapter`: `kind`, `capabilities`, `parse`, `render`, `send`.
3. `run(&adapter).assert_ok("<name>")` in your tests. It will find things.
4. A row in the table above, with the reasoning, not just the value.

The suite is the argument for writing it: it caught Telegram silently discarding
every action row, Matrix emitting buttons it had declared it did not support,
and Discord's mention stripper deleting other people from sentences.

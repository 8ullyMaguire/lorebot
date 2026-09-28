# What Lorehaven knows, and what it does not

This is the document to read before deploying lorebot anywhere, and the one to
point an operator at when they ask *what does this thing see about me*.

## The rule

**lorebot never tells Lorehaven where a message went, and never tells a platform
what Lorehaven said.** Every reply crosses two boundaries in one direction and
none in the other.

Reader → bot: the command text and the platform's user id.
Bot → Lorehaven: an API call with a scoped token, `POST /me/chat/actions`, with
the platform's actor as a string. That actor is stored.
Bot → platform: the rendered result.
Platform → Lorehaven: nothing. Ever. There is no callback, no webhook
registration, no read-receipt, no delivery report, no "the reader opened this".

## Why the second one is structural

The API accepts the actor as a **string** and returns no delivery information.
There is nowhere to report a destination to, so there is nothing to report *to*.
This is not the bot choosing to be quiet about where messages go; it is the
shape of the interface. A future Lorehaven that wanted to know would need an
endpoint that accepts one, and the honest answer is that we would rather it did
not.

The consequence worth stating plainly: **Lorehaven knows a reader used a bot,
on which platform, and what they asked.** It does not know whether the answer
was delivered, whether it was read, or which channel it appeared in. If it
learned the last one it would have a record of where every reader's private
library results are visible, which is a database worth stealing.

## The one exception, and it is an exception

**A link back to the instance.** `/whoami` replies with the instance URL so a
reader can find the thing they are linked to. The URL is public information —
it is in every server config — and it is in the message the reader is already
looking at. It is not a channel, a room, or a chat id.

## Why this is enforced rather than documented

Three tests in `lorebot-core`, plus the adapter conformance suite, would fail if
this stopped being true:

- `lorebot-core/src/privacy.rs` asserts the disclosure set is exactly what is
  documented here, so adding a field to a reply is a test failure.
- `lorebot_adapter::conformance::run` refuses a renderer that emits a private
  message for a platform that declares no private support. Matrix fails this
  case, which is the correct outcome: a Matrix deployment cannot deliver a
  private library result, so the bot says so rather than posting it to a room.
- The binary's `serve` turns `CannotSendPrivately` into a *message to the
  reader* explaining the refusal, so a failure is visible to the person it
  affects rather than only in a log.

## Token storage

A reader's Lorehaven token is stored **only** in lorebot's own encrypted file
(`~/.local/share/lorebot/store.bin` by default), sealed with AES-256-GCM under
`LOREBOT_STORE_KEY`. It is never written to a log, never put in a URL, never
sent to a platform, and never included in a bug report — the config module
refuses a key that is too short rather than padding it, and the store is
`zeroize`d on drop.

The store's contents are links and tokens. The bot's own platform tokens come
from the environment and are not written to it.

`LOREBOT_EPHEMERAL_STORE=1` keeps links in memory only. It exists for test
deployments, it logs a warning at startup, and every reader is unlinked when the
process restarts.

## What a platform can still see

Worth being honest about, because the guarantee above is about Lorehaven, not
about the chat platform:

- **Discord** sees an ephemeral message and not its content in the channel, but
  it is Discord's message and Discord's storage policy applies to it. Its
  `flags: 64` means no message-create event fires, so a *bot* cannot see it; a
  Discord staff member with database access is a different question.
- **Telegram** sees a 1:1 message. Bot privacy mode aside, Telegram has the
  message.
- **Matrix** sees everything, and a homeserver operator can read the room.

So the accurate statement is: lorebot does not tell Lorehaven where a message
went, and does not put private results where a platform can put them in a
public channel. It cannot stop a platform operator from reading a private
message, and no design could.

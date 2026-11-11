# Linking a Discord user to a Darkhold account

## The two models, and why only one of them can be built

darkbot's `/link` + `/link_confirm` pair assumes **the bot holds the reader's credential**:
`POST bot/link/start` returns a code, the reader confirms it on the site, and
`POST bot/link/exchange` hands the **bot** a token acting as that reader. The token is then stored on the bot's
host (`DARKBOT_STORE_KEY`) so `/status`, per-fandom feeds and the rest can act as that person.

Darkhold does not have those two endpoints, and the flow it does have (spec §23.2) puts the credential somewhere
else on purpose:

| step | endpoint | who | what it produces |
|---|---|---|---|
| 1 | `POST /me/bots/{id}/link-challenge` | the **bot's owner**, signed in | a code, plus the scopes the bot asks for |
| 2 | `GET /bots/link/{code}` | anyone holding the code | the disclosure: the bot's name and exactly what it wants |
| 3 | `POST /bots/link/{code}` | the **reader**, signed in, with a pseud chosen | a token **returned once, to that reader's session** |

There is no fourth step, and no door through which the bot learns the token. That is not an omission: a
pseudonymous platform handing a program a long-lived credential that acts as a person is a different privacy claim
from a person acting in their own session, and §23.2 chose the second.

## The decision

**darkbot does not hold reader credentials.** It holds exactly one credential — the registration token issued when
the bot itself was registered — and it acts as the bot.

* `/link` becomes a **pointer, not a handshake**: it explains what linking gives you, links to
  `<web>/settings/connections`, and stops. No code is minted, nothing is exchanged, nothing is stored.
* `/link_confirm` is **removed**. A command that cannot complete without a credential the bot may not hold is a
  command whose failure mode is "it looked like it worked".
* `/unlink` stops being "delete the token from this host" and becomes a pointer to the same page, where the reader
  revokes their own grant — which is where a revocation belongs, because it is theirs.
* **Personalised output becomes a link to the site.** `/status`, `/feed`, "your bookmarks" and the personal half of
  the recommendation commands answer with an ephemeral deep link. They are not deleted; they stop pretending to
  have data the bot cannot see.
* **Public commands keep working unchanged** and now use the one credential the bot legitimately has:
  `public/search`, `public/works/{id}`, `works/{id}/kudos`, `feeds/*`. Anything a signed-out visitor to the site
  can see, the bot can answer, and it is worth saying that in the command descriptions.

## What this costs, stated plainly

A reader can no longer type `/status` in Discord and read their library there. That was the honest capability of
the old design only because the old design kept a person's token on somebody else's server. The alternative — §23.2
growing a bot-exchange door — would put the same token on the same host and call it a spec.

## What this keeps

The linking flow still exists, still shows the reader exactly what is being granted **before** they grant it (step
2), still lets them narrow the scopes (step 3's body), and still issues a credential they can revoke without
asking anybody. What changed is who holds it.

## Status

Recorded 2026-11-11 while taking `M45-43`. The server side is implemented and tested
(`crates/app/tests/m54_bot_link.rs`, `m70_13_bot_link_doors.rs`, and `m45_59_multi_link.rs` — a link is per reader,
so many members can link to one bot). The darkbot side is **not yet changed**: `darkbot-api` still calls
`bot/link/start` and `bot/link/exchange`, which answer 404 against a real darkhold. Do not run darkbot against the
instance until `dispatch.rs`'s two link handlers are replaced as described above; the calls fail closed (404), so
nothing is unsafe in the meantime — it is simply not a working `/link`.

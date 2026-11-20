# darkbot handoff — 2026-11-20

## Current state

M1–M3 of the Discord interactions redesign are implemented. Guild commands are registered. darkbot now has a library target, an ed25519-signed `/discord/interactions` route with PING and inert mode, a registered command tree for `/darkhold`, and operator verbs `discord-status`, `discord-register [--guild ID]`, `serve-interactions`.

## Verification

- `cargo +1.99.0 test -p darkbot --test interactions` → 3 passed, 0 failed (PING/signature, command tree, signed help through dispatcher).
- Clippy all targets with `-D warnings` → exit 0.
- Mutants: verifier bypass produced 200 instead of 401; renamed `privacy` failed the tree assertion; forcing `flags: 0` failed the ephemeral-help assertion. All restored from `/tmp`.

## Known issues, in fix order

1. Live endpoint: `/api/discord/interactions` must be exposed publicly and pasted into Developer Portal → General Information → Interactions Endpoint URL. No Discord login is saved in the vault and no local tunnel route was found, so the owner must do the portal step or authorise a host/tunnel route.
2. M4: no global command registration; owner chose guild-only first.

## Decisions not to relitigate

- Endpoint lives in darkbot, not darkhold's API.
- Discord gateway will be disabled; Matrix/Telegram keep their gateways.
- Root command is `/darkhold`; one root with 13 subcommands. A mixed reply is sent ephemerally so no private line can leak.
- The legacy Discord gateway is off unless `DARKBOT_DISCORD_GATEWAY=1`; Matrix/Telegram gateways are unchanged.
- Ascend is the reference contract, but its fics/session state is not copied.


## Guild registration evidence — 2026-11-20

`darkbot discord-register --guild 1390467506577346782` printed `registered 1 command with 13 subcommand(s)`. A subsequent Discord API GET confirmed `[('darkhold', 13)]`. No token value was printed or stored in the repository.

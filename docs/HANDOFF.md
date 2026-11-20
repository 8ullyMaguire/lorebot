# darkbot handoff — 2026-11-20

## Current state

M1 and M2 of the Discord interactions redesign are implemented. darkbot now has a library target, an ed25519-signed `/discord/interactions` route with PING and inert mode, a registered command tree for `/darkhold`, and operator verbs `discord-status`, `discord-register [--guild ID]`, `serve-interactions`.

## Verification

- `cargo +1.99.0 test -p darkbot --test interactions` → 3 passed, 0 failed (PING/signature, command tree, signed help through dispatcher).
- Clippy all targets with `-D warnings` → exit 0.
- Mutants: verifier bypass produced 200 instead of 401; renamed `privacy` failed the tree assertion; forcing `flags: 0` failed the ephemeral-help assertion. All restored from `/tmp`.

## Known issues, in fix order

1. M3: Discord gateway is still enabled by default; the interactions listener is not deployed or proxied.
2. M4: no global command registration; owner chose guild-only first.

## Decisions not to relitigate

- Endpoint lives in darkbot, not darkhold's API.
- Discord gateway will be disabled; Matrix/Telegram keep their gateways.
- Root command is `/darkhold`; one root with 13 subcommands. A mixed reply is sent ephemerally so no private line can leak.
- Ascend is the reference contract, but its fics/session state is not copied.


# darkbot handoff — 2026-11-20

## Current state

M1 of the Discord interactions redesign is implemented. darkbot now has a library target, an ed25519-signed `/discord/interactions` route with PING and inert mode, a registered command tree for `/darkhold`, and operator verbs `discord-status`, `discord-register [--guild ID]`, `serve-interactions`.

## Verification

- `cargo +1.99.0 test -p darkbot --test interactions` → 2 passed, 0 failed.
- Clippy all targets with `-D warnings` → exit 0.
- Mutants: verifier bypass produced 200 instead of 401; renamed `privacy` subcommand failed the command-tree assertion. Both restored from `/tmp`.

## Known issues, in fix order

1. M2: interaction JSON is not mapped into the dispatcher yet; typed `/darkhold help` would receive the placeholder "Darkhold does not do that yet." reply.
2. M3: Discord gateway is still enabled by default; the interactions listener is not deployed or proxied.
3. M4: no global command registration; owner chose guild-only first.

## Decisions not to relitigate

- Endpoint lives in darkbot, not darkhold's API.
- Discord gateway will be disabled; Matrix/Telegram keep their gateways.
- Root command is `/darkhold`; one root with 11 subcommands.
- Ascend is the reference contract, but its fics/session state is not copied.


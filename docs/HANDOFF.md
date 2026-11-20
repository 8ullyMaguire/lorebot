# darkbot handoff — 2026-11-20

## Current state

M1–M3 of the Discord interactions redesign are implemented. Guild commands are registered. darkbot now has a library target, an ed25519-signed `/discord/interactions` route with PING and inert mode, a registered command tree for `/darkhold`, and operator verbs `discord-status`, `discord-register [--guild ID]`, `serve-interactions`.

## Verification

- `cargo +1.99.0 test -p darkbot --test interactions` → 3 passed, 0 failed (PING/signature, command tree, signed help through dispatcher).
- Clippy all targets with `-D warnings` → exit 0.
- Mutants: verifier bypass produced 200 instead of 401; renamed `privacy` failed the tree assertion; forcing `flags: 0` failed the ephemeral-help assertion. All restored from `/tmp`.

## Known issues, in fix order

1. Portal URL: retry pasting `https://darkhold.polarisocial.xyz/api/discord/interactions` in Developer Portal → General Information → Interactions Endpoint URL. Endpoint is now deployed and reachable.
2. M4: no global command registration; owner chose guild-only first.

## Decisions not to relitigate

- Endpoint lives in darkbot, not darkhold's API.
- Discord gateway will be disabled; Matrix/Telegram keep their gateways.
- Root command is `/darkhold`; one root with 13 subcommands. A mixed reply is sent ephemerally so no private line can leak.
- The legacy Discord gateway is off unless `DARKBOT_DISCORD_GATEWAY=1`; Matrix/Telegram gateways are unchanged.
- Ascend is the reference contract, but its fics/session state is not copied.


## Guild registration evidence — 2026-11-20

`darkbot discord-register --guild 1390467506577346782` printed `registered 1 command with 13 subcommand(s)`. A subsequent Discord API GET confirmed `[('darkhold', 13)]`. No token value was printed or stored in the repository.

## Deployment evidence — 2026-11-20

- Built on thinkcentre from `/personal/documents/code/rust/darkbot`, binary at `target/release/darkbot`.
- Service `darkbot-interactions.service` active, bound to `127.0.0.1:8087`; logs show `discord gateway disabled` and `platforms=["interactions"]`.
- Nginx exact location proxies `/api/discord/interactions` to that listener; `nginx -t` passed and nginx reloaded.
- Public unsigned POST returns `401 invalid request signature`, proving the route reaches the verifier rather than darkhold's API 404.
- The env file's `DARKBOT_DISCORD_PUBLIC_KEY` was stale then briefly empty; corrected from Discord's `/applications/@me` `verify_key`. Comparison now prints `VERIFY_KEY_MATCHES_ENV True`. Values were never printed.

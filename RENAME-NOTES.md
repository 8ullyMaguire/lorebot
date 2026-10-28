# RENAME-NOTES — lorebot → darkbot

The project formerly called **lorehaven** is now **darkhold**, and this repository
followed it. Nothing in here is a redesign: same crates, same behaviour, same tests.

| | |
|---|---|
| Old name | `lorebot` (instance: `lorehaven`) |
| New name | `darkbot` (instance: `darkhold`) |
| Old path | `~/code-local/rust/lorebot` |
| New path | `~/code-local/rust/darkbot` |
| Date | 2026-10-28 |

## What changed

- **Directory**: `lorebot/` → `darkbot/` (`git mv`, so history follows).
- **Crates**: `lorebot`, `lorebot-core`, `lorebot-api`, `lorebot-adapter`,
  `lorebot-discord`, `lorebot-telegram`, `lorebot-matrix` →
  `darkbot`, `darkbot-core`, `darkbot-api`, `darkbot-adapter`,
  `darkbot-discord`, `darkbot-telegram`, `darkbot-matrix`. Crate directories moved
  with `git mv`; `Cargo.toml` names, path dependencies and every `use` statement
  follow. `Cargo.lock` changed in exactly 109 lines, all of them package names —
  **no dependency version moved**.
- **Binary**: `lorebot` → `darkbot`.
- **Environment variables**: prefix `LOREBOT_` → `DARKBOT_`. The instance URL was
  `LOREBOT_LOREHAVEN_URL` and is now `DARKBOT_DARKHOLD_URL` (both halves moved).
- **API client type**: `LorehavenClient` → `DarkholdClient`.
- **Default token store**: `lorebot-store.json` → `darkbot-store.json`.
- **User-facing strings, log targets, error messages, comments, README and
  `docs/`**: `lorebot` → `darkbot`, `Lorehaven` → `Darkhold`. Error messages still
  name the variable to set, now under its new name.
- **`docs/E2E.md`**: the procedure now points at `~/code-local/rust/darkhold` and
  at the server's current seed credentials (`dev@darkhold.local` / `darkhold-dev`).
- **`scripts/prove-db-assertion.sh`**: probe log files renamed to
  `darkbot-db-probe*.log`.

## What stayed

- **Behaviour.** No endpoint, no command, no scope, no wire type, no store format
  changed. The command surface (`/link`, `/search`, `…`) is identical.
- **The dependency set and every version in `Cargo.lock`.** The diff is package
  names only.
- **`rust-toolchain.toml`** (channel 1.82). See "Known breakage" below — it is not
  caused by the rename and was not touched.
- **Git remotes.** They still say `.../lorebot.git`; the forge and GitHub projects
  have not been renamed, and pointing a remote at a URL that does not exist would
  break fetching for no gain.

## The compatibility shim

Two deployed interfaces moved, and both still accept the old spelling.

1. **`LOREBOT_*` environment variables.** `crates/darkbot-core/src/config.rs`
   reads `DARKBOT_<X>`, and if that is unset falls back to `LOREBOT_<X>` — with one
   `WARN` per variable naming both spellings and saying the shim is temporary. The
   instance URL keeps its old full name `LOREBOT_LOREHAVEN_URL` through an explicit
   alias table (`LEGACY_ALIASES`), because a prefix swap alone would look for
   `LOREBOT_DARKHOLD_URL`, which no deployment has ever set.
   Covered by `config::tests::the_pre_rename_variable_names_are_derived_from_the_current_ones`.

   ```
   $ LOREBOT_LOREHAVEN_URL=… LOREBOT_STORE_KEY=… LOREBOT_DISCORD_TOKEN=… darkbot
   WARN configuration variable renamed — the old spelling still works, but move it:
        this shim is temporary old="LOREBOT_LOREHAVEN_URL" new="DARKBOT_DARKHOLD_URL"
   ```

2. **The default token store file.** The new default is `./darkbot-store.json`. If
   that file does not exist and `./lorebot-store.json` does, the old file is used
   and a warning names both. Without this, a fresh start would open an empty store
   and unlink every reader on the first restart.
   Covered by `config::tests::the_default_store_is_the_new_name_unless_only_the_old_file_exists`.

3. **`--help` / `--version`.** Added, because a binary whose only way to state its
   name was its process name is a bad thing to hand an operator after a rename.
   `darkbot --help` prints `darkbot 0.1.0 — a thin REST client for your Darkhold
   instance…`. Any other argument is warned about and ignored, exactly as arguments
   were ignored before, so an existing unit file that passes a stray flag still
   starts.

## What an operator must do

1. **Update the unit file** (or `.env`, or compose file): rename every
   `LOREBOT_*` variable to `DARKBOT_*`. It will keep working while you do not, with
   a warning, but the shim is temporary.
2. **Point `ExecStart=` at the new binary name** — `darkbot`, not `lorebot`.
   `/home/alvaro/code-local/rust/lorebot` is now a **symlink** to `darkbot`, so a
   unit whose `WorkingDirectory=` still names the old path still starts; the unit
   is what needs editing, not the path.
3. **Rename the store file** if you use the default location:
   `mv lorebot-store.json darkbot-store.json` (keep the permissions; the file is
   encrypted, so a copy is harmless and a rename is cleaner). Until you do, the
   bot keeps using the old name and says so on every start.
4. **Nothing to do about the forge URL.** The remotes have not moved.

## Verification (real output, this machine)

```
$ cargo +stable build --workspace
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 26.93s

$ cargo +stable test --workspace
    ... 257 passed; 0 failed        (all 15 suites green)
$ cargo +stable fmt --all --check   # clean
$ cargo +stable clippy --all-targets # no warnings
$ darkbot --help
darkbot 0.1.0 — a thin REST client for your Darkhold instance, on Discord, Telegram and Matrix.
```

## Known breakage, NOT caused by the rename

`cargo build` under the pinned 1.82 toolchain fails before it reaches this
repository's code:

```
error: failed to parse manifest at `…/zeroize_derive-1.5.0/Cargo.toml`
Caused by: feature `edition2024` is required
```

That is a dependency in the local registry whose manifest needs a newer Cargo than
1.82. Confirmed pre-existing: a clean `git worktree` of the **pre-rename** commit
fails with byte-identical output under the same toolchain. `docs/goal-check.py`
therefore reports `builds` and `test suite green` as FAIL on this machine while
`fmt`, `clippy` and "every crate tested" pass. The rename was verified with
`stable` (1.99). Fixing the toolchain floor is a separate change and was left
alone on purpose.

#!/usr/bin/env bash
# Step 10, the half that matters: prove `no_code_path_reaches_a_database` can
# fail. A test that has never been seen red is not known to work, and this one
# is the only thing standing between "the bot is a REST client" and "the bot
# grew a database driver" being a convention rather than a property.
set -uo pipefail
cd "$(dirname "$0")/.."
# Backups live in the workspace, not in `mktemp`. TMPDIR points at a scratch
# directory that is pruned, and a trap that fires after the file is gone leaves
# the tree dirty with whatever the probe added — which is how the first run of
# this script left `const _PROBE` sitting in config.rs.
MANIFEST=crates/lorebot-core/Cargo.toml
SOURCE=crates/lorebot-core/src/config.rs
BACKUP_DIR=target/probe-backup
mkdir -p "$BACKUP_DIR"
cp "$MANIFEST" "$BACKUP_DIR/Cargo.toml"
cp "$SOURCE" "$BACKUP_DIR/config.rs"
restore() {
  cp "$BACKUP_DIR/Cargo.toml" "$MANIFEST"
  cp "$BACKUP_DIR/config.rs" "$SOURCE"
}
trap restore EXIT

echo "== 1. adding a database driver must FAIL the test =="
printf '\nrusqlite = "0.32"\n' >> "$MANIFEST"
# The assertion message is wrapped by rustc, so the needle is matched on the
# crate name alone — "must not depend on" is split across two lines in the
# panic output and a grep for the whole phrase finds nothing.
# Output is captured to a file before grepping. `cargo test ... | grep -q`
# closes the pipe on the first match, cargo takes SIGPIPE, and the script sees
# an early exit instead of the message it was looking for — which is exactly
# what happened on the first run of this script.
cargo test -p lorebot-core no_code_path > /tmp/lorebot-db-probe.log 2>&1 || true
if grep -q "not depend on rusqlite" /tmp/lorebot-db-probe.log; then
  echo "PASS: the assertion caught it"
else
  echo "FAIL: the assertion did not notice a database driver"
  exit 1
fi

echo
echo "== 2. a hand-rolled connection string must FAIL the test =="
restore
trap restore EXIT
# Put the string in the source, not the manifest: a connection opened from a
# literal URL would never appear in Cargo.lock, which is why the source scan
# exists at all.
printf '\nconst _PROBE: &str = "postgres://localhost/leak";\n' >> crates/lorebot-core/src/config.rs
cargo test -p lorebot-core no_code_path > /tmp/lorebot-db-probe2.log 2>&1 || true
if grep -q "would not appear in" /tmp/lorebot-db-probe2.log; then
  echo "PASS: the source scan caught a connection string"
else
  echo "FAIL: the source scan did not notice a hand-rolled connection"
  cp /tmp/lorebot-config-probe.bak crates/lorebot-core/src/config.rs
  exit 1
fi
restore

echo
echo "== 3. the clean tree must PASS =="
cargo test -p lorebot-core no_code_path 2>&1 | grep -E "^test result"

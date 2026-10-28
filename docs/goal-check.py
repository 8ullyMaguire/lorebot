#!/usr/bin/env python3
"""
goal-check.py -- the completion predicate for darkbot.

    cd ~/code-local/rust/darkbot && python3 docs/goal-check.py

Exit 0  -> the goal is COMPLETE.
Exit 1  -> work remains; stdout names the clause.
Exit 2  -> CANNOT CLAIM COMPLETE; a clause could not be evaluated.

## Why this file exists

Fourth project in the estate to get one (polaris 9f855ef, kindred fb681c2,
threadlight). darkbot's suite is GREEN -- 173 tests measured 2026-10-05 -- and there
is no way to say so that a later change cannot quietly invalidate it.

## This project's actual gap, which the clause names

`cargo test` reports 173 passing tests and **four suites reporting 0**:

    unittests src/lib.rs (darkbot-discord)   test result: ok. 0 passed
    unittests src/lib.rs (darkbot-matrix)    test result: ok. 0 passed
    unittests src/lib.rs (darkbot-telegram)  test result: ok. 0 passed

Those are the three platform adapters. darkbot is a bot that talks to Discord,
Matrix and Telegram, so **the code that talks to the platforms is untested** while
the code that routes and parses is tested thoroughly (23 in `client.rs` alone). A
reader would see "173 tests" and reasonably conclude the bot is covered.

That is why this predicate carries a clause the others do not: **every crate must
have at least one test.** A crate with no test is a FAIL, not a WARNING, because
"the adapters are correct" is currently unfalsifiable -- there is no assertion
anywhere that could fail.

Note the trap this clause exists to catch: `cargo test` exits **0** when a suite
reports 0 tests. An empty suite is indistinguishable from a passing one at the exit
code, which is the silent-no-match failure in its purest form.

## Design rules, each tied to a failure this estate has actually produced

  * **A check that can silently match nothing is worse than no check.** polaris's
    `scripts/audit_counters.py` printed "0 counter columns have a write path; 0 do
    not" with exit 0 while its Postgres container was dead -- textually identical to
    a clean bill of health. The crate clause below works in the same direction:
    it enumerates the crates that have nothing, rather than counting tests.

  * **A clause that cannot be evaluated reports UNKNOWN, never PASS.** A timeout
    produces no output and reads exactly like a clean run.

  * **Never trust a count written in a document. Recompute it**, and measure the floor
    with the same function that compares against it. stash-box shipped a floor
    measured one way and compared another, and a suite that lost 27 packages still
    reported PASS. This file shipped a floor of 173 against a real 255 for one
    session, because the number came from a hand-summed subset of the output; the
    clause caught it by printing "255 tests passed (floor 173)".

  * **Assert what the toolchain can actually do, and report which toolchain ran.**
    The other three predicates here use `-- -D warnings`. This repo pins
    `channel = "1.82"` in rust-toolchain.toml, where `-D warnings` turns a
    toolchain-version error into a build failure, so the clause reported an
    unsupported-rustc error as a clippy finding. It also turns out the system
    `clippy-driver` is 0.1.82 while the system `rustc` is 1.98.1, so the system
    clippy blames `zerovec` for needing 1.83. The clause therefore runs a MATCHED
    rustup pair with a toolchain-specific target dir, and names the pair in its
    output. A check that cannot say what it ran is not trustworthy.

No database is involved: darkbot's suite needs no external service, which is worth
recording because it is the one in-scope project where step 1 of the standard plan
(no infrastructure clause) genuinely does not apply.
"""

import os
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CRATES = REPO / "crates"

# Floor, not a target. Measured 2026-10-05 by `--update-baseline`: 255 tests.
# I first wrote 173, from a `cargo test` output I read through `paste` and summed by
# hand across a subset of the suites. That is exactly the unmeasured-floor error this
# file's own rules forbid, and the clause caught it by reporting "255 tests passed
# (floor 173)" -- a floor under reality is not a guard.
# Adding tests can never fail this clause; losing them is a regression, and a
# regression looks exactly like a suite that never ran.
BASELINE_TESTS = 255

results = []


def sh(cmd, timeout=1800, env=None):
    e = dict(os.environ)
    if env:
        e.update(env)
    try:
        p = subprocess.run(cmd, shell=True, cwd=REPO, capture_output=True,
                           text=True, timeout=timeout, env=e)
        return p.returncode, p.stdout.strip(), p.stderr.strip()
    except subprocess.TimeoutExpired:
        return 124, "", f"timeout after {timeout}s"


def add(clause, status, detail):
    results.append((clause, status, detail))
    mark = {"PASS": "ok  ", "FAIL": "FAIL", "UNKNOWN": "????"}[status]
    print(f"[{mark}] {clause}\n         {detail}")


def parse_tests(out):
    """(passed, failed, [names]) from cargo's `test result:` lines.

    ONE parser, used by every clause that reads a run -- because two copies of one
    measurement is two chances to disagree, and they disagreed once in stash-box.
    """
    passed = failed = 0
    for m in re.finditer(r"^test result: (\w+)\. (\d+) passed; (\d+) failed", out, re.M):
        if m.group(1) == "ok":
            passed += int(m.group(2))
            failed += int(m.group(3))
    names = [a or b for a, b in
             re.findall(r"^---- (\S+) stdout ----|^--- FAIL: (\S+)", out, re.M)]
    return passed, failed, names


# ---------------------------------------------------------------- clauses

def clause_fmt():
    rc, out, err = sh("cargo fmt --all -- --check 2>&1 | head -20")
    body = (out + err).strip()
    add("cargo fmt clean", "PASS" if rc == 0 else "FAIL",
        "no diff" if rc == 0 else (body.splitlines()[0][:160] if body else "unformatted"))


def clause_clippy():
    """No clippy warnings, run through a MATCHED toolchain.

    Two traps here, both of which produce a message that looks like a product
    problem.

    1. **`-D warnings` is not usable in this repo.** rust-toolchain.toml pins
       `channel = "1.82"`, and `-D warnings` promotes a toolchain-version error into
       a build failure, so the clause reported "error: rustc 1.82.0 is not supported
       by the following packages" as if it were a lint finding.

    2. **The system clippy is a different version from the system rustc.**
       `/usr/bin/cargo-clippy` is 0.1.82 (Arch) while `/usr/bin/rustc` is 1.98.1
       (Arch). A `clippy-driver` that does not match the `rustc` it wraps checks
       dependencies against the pinned 1.82 and reports the mismatch as
       "`zerovec@0.11.8 requires rustc 1.83`" -- naming innocent crates. Build and
       test both work in the same repo at the same moment, which is the tell.

       So this clause prefers a MATCHED pair from rustup, and falls back to the
       system one only when rustup has none. The fallback still runs the check; it
       just cannot be trusted to, which is why the fallback reports what it ran.

    The target directory must be toolchain-specific: two compiler versions cannot
    share build artifacts, and cargo reports that as a dependency error.
    """
    # `rustup run <toolchain> cargo clippy` -- the only invocation that works here.
    # The three that do not, each for a different reason:
    #   "$TC/bin/cargo-clippy"          -> "no such command": cargo-clippy is a cargo
    #                                     SUBCOMMAND, not a standalone linter
    #   "$TC/bin/cargo clippy"          -> that cargo re-reads rust-toolchain.toml and
    #                                     applies the 1.82 pin we are trying to avoid
    #   "$TC/bin/cargo +1.99.0 clippy"  -> "+toolchain" is rustup syntax; a bare
    #                                     toolchain cargo cannot parse it
    # rustup selects the toolchain, its own cargo-clippy runs, and the explicit
    # toolchain argument overrides the pin.
    tc = None
    for cand in sorted(Path.home().glob(".rustup/toolchains/*")):
        if (cand / "bin" / "cargo-clippy").exists() and re.search(r"^1\.", cand.name):
            tc = cand
            break
    if tc is None:
        rc, out, err = sh("cargo clippy --all-targets 2>&1", timeout=1800)
        body = out + err
        warns = [w for w in re.findall(r"^warning: [a-z].*", body, re.M)
                 if not re.match(r"^warning: `\w+` .* generated", w)]
        if rc != 0 and not warns:
            add("clippy clean", "FAIL", "clippy did not complete: "
                + (body.splitlines()[-1][:150] if body else "no output"))
            return
        add("clippy clean", "PASS" if not warns else "FAIL",
            f"{len(warns)} warnings (system clippy; no matched rustup pair found)")
        return

    # A per-toolchain target dir, so a 1.99 clippy never meets 1.98 artifacts. Sharing
    # one produces a different misleading error: cargo reports the version mismatch
    # as "crate `libc` compiled by rustc 1.99.0 ... please recompile using 1.98.1".
    tdir = f"/home/alvaro/.cache/cargo-target-{REPO.name}-tc"
    # Absolute path: `rustup` is in ~/.cargo/bin, which is deliberately NOT on this
    # host's PATH (a rustup cargo shim there shadows Arch's cargo and breaks
    # check-cfg on other repos). Calling it by bare name gives "command not found",
    # which the clause then reported as "clippy did not complete" -- a check
    # reporting the wrong cause, which is the failure mode it exists to prevent.
    rustup = Path.home() / ".cargo" / "bin" / "rustup"
    rc, out, err = sh(f'"{rustup}" run {tc.name} cargo clippy --all-targets 2>&1',
                      timeout=1800, env={"CARGO_TARGET_DIR": tdir})
    body = out + err
    if rc == 124:
        add("clippy clean", "UNKNOWN", "clippy timed out")
        return
    warns = [w for w in re.findall(r"^warning: [a-z].*", body, re.M)
             if not re.match(r"^warning: `\w+` .* generated", w)]
    if rc != 0 and not warns:
        add("clippy clean", "FAIL", "clippy did not complete: "
            + (body.splitlines()[-1][:150] if body else "no output"))
        return
    if warns:
        kinds = {}
        for w in warns:
            k = re.sub(r"`[^`]*`", "`X`", w)
            kinds[k] = kinds.get(k, 0) + 1
        top = ", ".join(f"{v}x {k[:60]}" for k, v in
                        sorted(kinds.items(), key=lambda kv: -kv[1])[:3])
        add("clippy clean", "FAIL", f"{len(warns)} warnings: {top}")
        return
    add("clippy clean", "PASS", f"no warnings (matched pair: {tc.name})")


def clause_build():
    rc, out, err = sh("cargo build --workspace 2>&1", timeout=1800)
    add("builds", "PASS" if rc == 0 else "FAIL",
        "cargo build --workspace clean" if rc == 0
        else (err or out).splitlines()[-1][:160])


def clause_suite():
    rc, out, err = sh("cargo test --workspace 2>&1", timeout=1800)
    if rc == 124:
        add("test suite green", "UNKNOWN", "cargo test timed out after 1800s")
        return
    passed, failed, names = parse_tests(out)
    if failed:
        add("test suite green", "FAIL", f"{failed} failing: " + ", ".join(names[:5]))
        return
    if rc != 0:
        add("test suite green", "FAIL", (err or out).splitlines()[-1][:160])
        return
    if passed < BASELINE_TESTS:
        add("test suite green", "FAIL",
            f"{passed} tests passed, floor is {BASELINE_TESTS}. A suite that lost tests "
            f"is indistinguishable from one that never ran them.")
        return
    add("test suite green", "PASS", f"{passed} tests passed (floor {BASELINE_TESTS})")


def clause_every_crate_tested():
    """Every crate must contain at least one real test.

    This is the clause that makes "darkbot is covered" falsifiable. Enumerated, not
    counted: a count would say "3 crates untested" and leave the reader to find them.

    A crate counts as tested when any `#[test]`, `#[tokio::test]`, `#[bench]` or
    `mod tests` with an actual `fn` appears inside its sources. Requiring the
    attribute rather than the file is the point -- a `#[cfg(test)] mod tests {}` that
    is empty would otherwise satisfy a file-existence check, and that is the
    silent-no-match failure again.
    """
    if not CRATES.is_dir():
        add("every crate tested", "UNKNOWN",
            f"no crates/ directory; if the layout moved this clause is stale and would "
            f"otherwise pass on nothing")
        return
    untested = []
    for crate in sorted(p for p in CRATES.iterdir() if p.is_dir()):
        blob = ""
        for pat in ("src/**/*.rs", "tests/**/*.rs"):
            blob += "".join(f.read_text(errors="ignore") for f in crate.glob(pat))
        has = re.search(r"#\[(tokio::test|test|bench| rstest|parameterized)", blob) or \
            re.search(r"#\[cfg\(test\)\]\s*mod\s+\w+\s*\{[^}]*fn\s", blob)
        if not has:
            untested.append(crate.name)
    if untested:
        add("every crate tested", "FAIL",
            f"{len(untested)} of {len(list(CRATES.iterdir()))} crates have no test: "
            + ", ".join(untested)
            + "\n         These are the platform adapters. `cargo test` exits 0 with "
              "them empty, so the suite reports green while the code that talks to "
              "Discord, Matrix and Telegram is unmeasured.")
        return
    add("every crate tested", "PASS",
        f"all {len(list(CRATES.iterdir()))} crates have at least one test")


# ---------------------------------------------------------------- driver

def update_baseline():
    rc, out, _ = sh("cargo test --workspace 2>&1", timeout=1800)
    p, f, _ = parse_tests(out)
    if rc != 0 or f:
        print("refusing to write a baseline from a red run", file=sys.stderr)
        sys.exit(1)
    print(f"measured {p} tests; set BASELINE_TESTS = {p}")


def main():
    if "--update-baseline" in sys.argv:
        update_baseline()
        return

    clause_fmt()
    clause_build()
    clause_suite()
    clause_clippy()
    clause_every_crate_tested()

    print()
    failed = [r for r in results if r[1] == "FAIL"]
    unknown = [r for r in results if r[1] == "UNKNOWN"]
    if failed:
        print(f"INCOMPLETE -- {len(failed)} clause(s) failed:")
        for c, _, d in failed:
            print(f"  - {c}: {d}")
        sys.exit(1)
    if unknown:
        print(f"CANNOT CLAIM COMPLETE -- {len(unknown)} clause(s) unevaluated:")
        for c, _, d in unknown:
            print(f"  - {c}: {d}")
        sys.exit(2)
    print(f"COMPLETE -- all {len(results)} clauses pass.")


if __name__ == "__main__":
    main()
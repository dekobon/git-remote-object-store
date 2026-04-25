---
globs:
  - "src/bin/git-remote-*.rs"
  - "src/bin/git-lfs-*.rs"
  - "src/protocol/**/*.rs"
  - "src/lfs/**/*.rs"
---

## Protocol Output Streams Are a Contract

Git invokes remote-helper binaries (`git-remote-s3+https`, `git-remote-az+https`, etc.) and the LFS custom-transfer agent over **a line-based protocol on stdin/stdout**. stdout is reserved for protocol traffic; stderr is the only acceptable channel for diagnostics. A stray byte on stdout — a banner, an info-level log line, an ANSI escape, even a trailing newline emitted by `dbg!` — corrupts the protocol and causes `git fetch` / `git push` to fail with cryptic parse errors. The same constraint applies to the LFS custom-transfer agent (newline-delimited JSON on stdout per the LFS spec).

### Hard requirements

- **Initialize `tracing-subscriber` with `.with_writer(std::io::stderr)` in every helper binary's `main()`**. The default writer is stdout — this default is unsafe for our binaries.
- **Never `println!`, `print!`, `eprintln!`-then-stdout-redirect, `dbg!`, or `writeln!(stdout(), ...)` outside of code that is intentionally writing protocol output.** Production code that wants to inform the user must use `tracing::{trace, debug, info, warn, error}!` (which goes to stderr) or `eprintln!`.
- **Do not emit ANSI escape sequences** unless `std::io::IsTerminal` confirms stdout is a TTY. Helper binaries are never invoked from a TTY.
- **Banner / version / progress output** belongs on stderr or behind a CLI subcommand that is not the helper protocol entrypoint.
- The management binary (`git-remote-object-store`) and `xtask`-style tools have no such constraint — they are regular CLIs and may write to stdout normally.

### Where this matters

| Phase | Surface | Stdout discipline |
|---|---|---|
| 3 / 6 | `src/bin/git-remote-*.rs` REPL | Strict — only protocol responses on stdout |
| 9 | `src/bin/git-lfs-object-store.rs` | Strict — only LFS JSON events on stdout |
| 7 / 12 | `src/bin/git-remote-object-store.rs` (management) | Normal CLI; stdout is for human/JSON output |

### Test discipline

Integration tests for the helpers must run the binary non-interactively (e.g., piped through a buffer) and assert exact stdout bytes. Diagnostic output that "looks fine in a terminal" is exactly the failure mode this rule prevents — surface it by capturing stdout in tests and comparing byte-for-byte.

If a test fails because stdout has a stray log line, the fix is to redirect the log line to stderr, not to filter it out of the test assertion.

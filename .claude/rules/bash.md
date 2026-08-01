---
globs: "**/*.sh"
---

## Bash Coding Conventions

- `shellcheck` for linting
- `set -euo pipefail` at the start of scripts
- Double-quote variable expansions; use `$(...)` for command substitution
- Uppercase variable names with underscores (e.g., `FILE_PATH`)
- Functions for reusable code; avoid global variables
- Lines under 100 characters; `getopts` for option parsing

## Shellspec `Skip if` conditions

`Skip if "<reason>" <cond>` evaluates `<cond>` through shellspec's DSL
preprocessor, not through bash. A leading `!` is folded into the command
name and redirections (`>/dev/null 2>&1`) get mangled by shellspec's
argument quoting. The guard then fails to fire silently: the spec body
runs as if the prerequisite were met and falls over inside `BeforeAll`
or `setup` with an error several layers removed from the real cause.

`<cond>` must be a single command — built-in, executable, or function —
with no leading `!`, no pipeline, and no redirection. Define predicates
in `spec/spec_helper.sh` that already return the desired exit code and
call those:

```bash
have_cmd()    { command -v "$1" >/dev/null 2>&1; }
missing_cmd() { ! command -v "$1" >/dev/null 2>&1; }
flag_unset()  { [[ "${!1:-0}" != "1" ]]; }
```

## fd/fdfind portability

The `fd` binary is named `fdfind` on Debian/Ubuntu. Scripts must detect the correct name at startup:

```bash
FD=$(command -v fd 2>/dev/null || command -v fdfind 2>/dev/null || true)
if [[ -z "$FD" ]]; then
    echo "error: fd (or fdfind) not found." >&2
    exit 1
fi
```

Then use `"$FD"` (quoted) instead of bare `fd` throughout the script.

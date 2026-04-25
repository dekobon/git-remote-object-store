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

---
globs: "**/*.rs"
---

## Rust Coding Conventions

- `cargo fmt` for formatting, `clippy` for linting
- Never write `unsafe` code
- Avoid `unwrap()`, `expect()`, `assert!()`, and `panic!()` in non-test code; return `Result`/`Option` and propagate with `?`
- `expect()` and `assert!()` are acceptable in tests
- In enumeration or discovery loops, a panic kills all remaining iterations -- return an error so the caller can skip and continue
- Prefer default trait implementations for zero-sized types
- Write unit tests for all public functions
- Keep functions short -- helps lifetime inference, readability, and testability

## Data Modeling

- Prefer `enum` for state machines over boolean flags or loosely related fields
- Use newtype wrappers to enforce domain invariants (e.g., `struct Port(u16)` instead of bare `u16`)
- Model invariants with types where possible (`NonZeroU32`, `Duration`, custom enums)
- Choose ownership deliberately per field: `&str` vs `String`, slices vs `Vec`, `Arc<T>` for shared ownership, `Cow<'a, T>` for flexible ownership
- Prefer borrowing over cloning when the owned value isn't needed

## Naming

- **One word per concept**: Pick one verb and use it everywhere. Don't mix `fetch`/`get`/`retrieve` or `parse`/`from_str`/`decode` for the same operation across the codebase.
- **Different words for different concepts**: If two things do different work, they need different names.
- **Boolean names are positive predicates**: `is_valid`, `has_children` -- never `not_disabled` or `no_error`.
- **No unexplained abbreviations**: Domain-standard (`fd`, `pid`, `url`, `sha`, `oid`) are fine. Ad-hoc abbreviations need a full name.
- **Plural names for collections**: `errors: Vec<Error>`, not `error: Vec<Error>`. Singular for single values.
- **Conversion method prefixes must match semantics** ([C-CONV](https://rust-lang.github.io/api-guidelines/naming.html#c-conv)): `as_` (free borrow), `to_` (expensive copy), `into_` (consuming), `from_` (constructor). Signature must match prefix.
- **Getters omit `get_` prefix** ([C-GETTER](https://rust-lang.github.io/api-guidelines/naming.html#c-getter)): `fn name(&self) -> &str`, not `fn get_name()`
- **`is_`/`has_` methods return `bool`**
- **Error type word order follows stdlib**: `ParseError` not `ErrorParse`

## Visibility and API Surface

- Prefer `pub(crate)` over `pub` -- only expose what downstream crates actually need
- Keep public APIs small and expressive; avoid leaking internal types
- Use meaningful module names aligned with domain boundaries

## Code Organization

- Place `impl` blocks immediately below the struct/enum they implement
- Group methods: constructors first, then getters, mutation methods, domain logic, helpers
- Provide clear constructors (`new`, `with_capacity`, builder pattern) where appropriate
- Use standard trait implementations (`Display`, `Debug`, `From`, `TryFrom`) to simplify conversions -- implementing `From` gives `Into` for free via the blanket impl
- Apply `derive` macros (`Debug`, `Clone`, `Serialize`, `Deserialize`) to reduce boilerplate
- Reserve blank lines between logically separate method groups

## Documentation

- Use `///` doc comments on public structs, enums, traits, and non-obvious methods
- Use `//!` for module-level documentation explaining design intent or architecture
- Include examples in doc comments where they clarify non-obvious usage

## Build Speed

- Use `cargo check` during rapid iteration instead of `cargo build`
- Minimize unnecessary dependencies and feature flags

## FFI and Unsafe Code

- Zero `unsafe` by default; every `unsafe` block must have a SAFETY comment
- Prefer `std` first, then well-vetted crates, then raw `libc` as a last resort
- Before adding a new `libc::` reference or other FFI crate, confirm with the user: state the capability, why `std` does not cover it, and what alternatives were considered
- FFI calls must be `#[cfg(unix)]`-gated (or equivalent) and isolated to a small helper
- Do not reach for FFI for convenience -- only to fill a capability gap

## Path-to-String Conversion

- Never use `to_string_lossy()` for paths used as identifiers (JSON fields, map keys, error correlation)
- Use `to_str()` with explicit error handling for identifiers
- `path.display()` is acceptable for human-readable error messages and log output
- `to_string_lossy()` is acceptable only for CLI display (stdout formatting, progress messages)
- For byte-level path operations on Linux (suffix checking, trimming), use `OsStr::as_bytes()` / `OsStr::from_bytes()` via `std::os::unix::ffi::OsStrExt`

## Public API Input Validation

- Guard public functions against degenerate inputs (empty strings, zero-length slices) that cause silent misbehavior
- Prefer early `return None` / `return Err(...)` over letting degenerate values flow through string operations like `split`, `splitn`, `contains`, `find`
- `str::splitn(n, "")` splits on every character boundary -- always guard against empty patterns

## Unreachable Defensive Code

- If a code path is provably unreachable, use `expect("invariant explanation")` not `unwrap_or_else` with fallback logic
- Don't use `eprintln!` or logging in unreachable branches -- this masks bugs
- If the invariant isn't obvious, add a comment explaining why

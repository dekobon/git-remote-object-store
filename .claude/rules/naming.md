## Naming Conventions

Pick names that tell the truth about what the code does. These rules prevent the most
common naming issues -- linters catch mechanical violations, but only humans (and these
rules) catch semantic mismatches.

### Universal (all languages)

- **One word per concept**: Pick one verb and use it everywhere. Don't mix `fetch`/`get`/`retrieve` or `parse`/`from_str`/`decode` for the same operation across the codebase.
- **Different words for different concepts**: If two things do different work, they need different names. Don't reuse `process` for both "validate input" and "transform output".
- **Boolean names are positive predicates**: `is_valid`, `has_children`, `can_retry` -- never `not_disabled` or `no_error`. Double negation (`!not_disabled`) is a bug magnet.
- **No unexplained abbreviations**: Domain-standard abbreviations (`fd`, `pid`, `url`, `sha`, `oid`, `ref`) are fine. Project-specific or ad-hoc abbreviations need a comment or, better, a full name.
- **Name length matches scope**: Single-letter names are fine in tight closures. Wide-scope names (public APIs, struct fields, module names) should be descriptive.
- **Plural names for collections**: `errors: Vec<Error>`, not `error: Vec<Error>`. Singular for single values.

### Rust-Specific

- **Conversion method prefixes must match semantics** ([Rust API Guidelines C-CONV](https://rust-lang.github.io/api-guidelines/naming.html#c-conv)):
  - `as_` -- free, borrowed view (e.g., `as_str()` returns `&str`)
  - `to_` -- expensive conversion, new allocation (e.g., `to_string()` returns `String`)
  - `into_` -- consuming, takes ownership of self (e.g., `into_inner()`)
  - `from_` -- constructor from another type (e.g., `from_bytes()`)
  - The method signature must match the prefix: `as_` borrows, `into_` consumes
- **Getters omit `get_` prefix** ([Rust API Guidelines C-GETTER](https://rust-lang.github.io/api-guidelines/naming.html#c-getter)): use `fn name(&self) -> &str`, not `fn get_name(&self) -> &str`
- **`is_`/`has_` methods return `bool`**: If a method starts with `is_` or `has_`, its return type must be `bool`
- **Error type word order follows stdlib**: `ParseError` not `ErrorParse`, `ConfigError` not `ErrorConfig`
- **Type names match their semantic role**: A field `count: String` or `name: Vec<u8>` is a red flag -- the type should reflect the domain meaning

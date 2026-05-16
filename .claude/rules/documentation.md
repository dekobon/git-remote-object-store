## Documentation: No Stale Counts

Never hardcode specific counts in documentation, comments, specifications, or commit messages. They go stale immediately.

- **Bad**: "25 tests passing", "889 directives", "15 modules", "71% pass rate"
- **Good**: "all tests passing", "all directives", "each module", "majority of tests pass"
- Use approximate language when scale matters: "hundreds of directives", "dozens of tests"
- **Exceptions**: CHANGELOG entries (point-in-time snapshots) and code (compiler/test-verified)

## Documentation: No Hardcoded Sibling Line Numbers

Never cite source files by line number (`file.rs:NNN`, `module.rs:NNN-MMM`) in doc-comments, code comments, specifications, or commit messages. Lines drift every time the file is edited; the citation goes stale silently and misleads the next reader.

Reference the **symbol** instead — function, method, struct, constant, or module — so the reader (or `rustdoc` / IDE jump-to-definition) can follow the link regardless of where the code now lives.

- **Bad**: comments like `// see push.rs:656-676`, prose like `(mirroring src/bundle.rs:332-337)`, doc-comments like `` (`mod.rs:65-67`) ``
- **Good**: comments like `// see protocol::push::push_one`, prose like `(mirroring bundle::unbundle's .keep removal)`, intra-doc links like `` [`ObjectStore::list`][super::ObjectStore::list] ``
- In Rust doc-comments, prefer intra-doc links (`` [`Type::method`] ``) over prose references when the symbol is in scope.
- **Exceptions**: CHANGELOG entries (point-in-time snapshots), commit messages that reference a specific historical revision, and code that produces line numbers programmatically (e.g., `file!()` / `line!()`).

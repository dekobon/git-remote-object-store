## Documentation: No Stale Counts

Never hardcode specific counts in documentation, comments, specifications, or commit messages. They go stale immediately.

- **Bad**: "25 tests passing", "889 directives", "15 modules", "71% pass rate"
- **Good**: "all tests passing", "all directives", "each module", "majority of tests pass"
- Use approximate language when scale matters: "hundreds of directives", "dozens of tests"
- **Exceptions**: CHANGELOG entries (point-in-time snapshots) and code (compiler/test-verified)

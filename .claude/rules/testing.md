---
globs: "**/*"
alwaysApply: false
---

## Build Before Test

Always rebuild (`cargo build`) before running integration tests. Never test against a stale binary.

## No Incidental Coupling in Test Infrastructure

Before encoding any property into test code, verify that the **system under test** actually depends on it. If the code doesn't branch on a property, the tests must not couple to it. Prefer runtime discovery (scanning, globbing) over constructing exact values from environmental or structural assumptions.

Common traps:

- **Host environment** (`cfg(target_arch)`, `std::env::consts::ARCH/OS`): only use if the code under test is architecture/OS-specific
- **Filename structure**: don't parse and reconstruct every segment of a naming convention — couple only to the segments the code needs
- **Directory layout**: don't hardcode paths that reflect organizational choices irrelevant to the logic being tested

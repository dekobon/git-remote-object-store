---
globs: "**/*"
alwaysApply: false
---

## GitHub CLI

When using `gh` with complex arguments, write content to a temp file and pass via `--body-file`:

```bash
cat > /tmp/issue-body.md <<'EOF'
Content with $variables, `backticks`, and "quotes"
EOF
gh issue create --title "Title" --label "bug" --body-file /tmp/issue-body.md
```

## GitHub Issue Hygiene

- Only close an issue when ALL items are resolved
- When updating issues, update BOTH the body AND add a comment

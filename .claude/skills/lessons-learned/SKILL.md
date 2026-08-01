---
name: lessons-learned
description: Review project activity and draft entries for lessons_learned.md. Use when asked to update or review lessons learned.
---

# Lessons Learned Workflow

Review recent project activity (issues, commits, changelog) to identify
hard-won lessons, evaluate them against a strict quality bar, and draft
entries for `docs/development/lessons_learned.md`.

**Argument**: `$ARGUMENTS` — empty for the full workflow, or hint text to
narrow the search (e.g., `"URL parser"`, `"S3 multipart"`).

---

## Step 1: Establish Boundary

Determine the time boundary for evidence gathering — everything since the
last update to the lessons file:

```bash
git log -1 --format=%aI -- docs/development/lessons_learned.md
```

If the file has never been modified beyond its initial creation (no
substantive history), fall back to the repository's first commit date:

```bash
git log --reverse --format=%aI | head -1
```

Record the boundary date as `$BOUNDARY`.

---

## Step 2: Read Current Coverage

Read `docs/development/lessons_learned.md` in full. Record:

1. **Each lesson's title and domain** — used for overlap detection in Step 5
2. **Issue numbers already cited** — avoid re-proposing lessons from known issues
3. **The rule each lesson points to** — a candidate whose prescription is
   already in `.claude/rules/` usually belongs as new evidence under an
   existing lesson, not as a new entry

Entries are keyed by title, not number. This step is mandatory; overlap
detection in Steps 5 and 6 depends on it.

---

## Step 3: Gather Evidence

Collect evidence from four sources. When `$ARGUMENTS` contains hint text,
add the hint as an additional search keyword to narrow results.

### 3a: Closed issues since boundary

```bash
gh issue list --state closed --search "closed:>$BOUNDARY" --limit 100 \
  --json number,title,body,labels,closedAt
```

Triage: scan titles and bodies for hard-lesson signals:

- "root cause", "debugging", "turns out", "subtle", "silent"
- "security", "regression", "broke", "workaround", "misunderstood"

Deep-dive on comments only for candidates that show signals:

```bash
gh issue view <N> --json comments
```

### 3b: Git commits since boundary

```bash
git log --since="$BOUNDARY" --format="%H %s" -- src/ tests/ docs/
```

Look for:

- Fix commits with substantial diffs (not trivial typos)
- Refactors that changed approach after initial implementation
- Multi-issue commits (suggest systemic pattern)

### 3c: CHANGELOG entries since boundary

Read `CHANGELOG.md` and identify entries added since the boundary date.
Focus on entries under "Fixed" and "Changed" sections — these are most
likely to contain lesson-worthy material.

### 3d: Documentation changes (skip when hint provided)

```bash
git log --since="$BOUNDARY" --name-only --format="" -- docs/ AGENTS.md CLAUDE.md
```

Look for new or substantially updated documentation that may reflect
hard-won understanding.

---

## Step 4: Deep Investigation

For items from Step 3 showing hard-lesson signals:

1. Read full issue threads and linked PRs
2. Examine diffs: `git show <commit>`
3. Use Serena LSP tools or code reading for surrounding context
4. Look for pattern repetition — did the same mistake happen more than once?

Record each potential lesson:

- **Source reference**: issue number(s), commit hash(es)
- **One-line summary**: what went wrong or what was learned
- **Evidence strength**: strong (cost real debugging time), moderate
  (non-obvious but caught quickly), weak (obvious in retrospect)

---

## Step 5: Candidate Evaluation

This is the core quality gate. Apply the bar from
`.claude/rules/lessons-learned.md`:

> **"Genuinely hard (cost real debugging time or caused real bugs) AND
> important (likely to recur)."**

Present candidates as a ranked batch:

```
### Candidate N: <summary>
- Source: #<issue>, <commit>
- Quality: QUALIFIES / DOES NOT QUALIFY
- Overlap: None / Related to "<lesson title>" (explain distinction or overlap)
- Reasoning: <why it meets or fails the quality bar>
```

### Handling non-qualifying candidates

For each candidate that does not qualify, suggest an alternative home with
case-by-case reasoning:

| Signal | Alternative Home |
|--------|-----------------|
| One-off debugging trick | Code comment at the relevant site |
| Architectural decision | A `//!` module doc, AGENTS.md, or `.claude/rules/*.md` |
| Testing pattern | Test file comment, or `.claude/rules/testing.md` |
| Bucket-key or destructive-write rule | `.claude/rules/object-store-writes.md` |
| Project convention | `.claude/rules/*.md` or `AGENTS.md` |
| Already covered by existing lesson | Merge as new evidence under that lesson |
| Too specific to one issue | Issue comment or PR description |

A candidate that yields a durable "always do X" prescription belongs in
`.claude/rules/` — those load every session, while this file is read
only by `/review`, `/audit-tests`, and `/fix-issue`. It can still earn a
lessons entry for the *evidence*, but the prescription must live in
exactly one place.

Push-back language must be explicit. **"No candidates qualify" is a valid
success state.** Do not force entries to justify the workflow.

**Wait for the user to select which candidates to draft.** Do not proceed
to Step 6 without user confirmation.

---

## Step 6: Draft Entries

For each user-selected candidate, draft an entry matching the established
format in `docs/development/lessons_learned.md`:

1. `## <Pithy Principle Name>` — no number; entries are keyed by title
2. Opening paragraph: general lesson statement (not issue-specific)
3. Bold sub-examples with issue/commit references (e.g., `**Description
   of specific instance** (#42, abc1234).`)
4. Optional `**Lesson**:` paragraph — only for a takeaway the rule does
   not carry (e.g. what the *repetition* of the bug revealed)
5. Closing `**Rule**:` line naming the section and file in
   `.claude/rules/` that carries the prescription
6. Horizontal rule (`---`) separator after the entry

### Overlap handling

- If a candidate overlaps with an existing lesson, propose one of:
  - **Merge**: add a new bold sub-example to the existing lesson. This
    is the default — a recurrence is stronger evidence than a new entry,
    and the file's value comes from staying small
  - **Skip**: if the overlap is too close, recommend not adding it
- Prefer merging over cross-referencing. A "Related to X" paragraph that
  exists only to justify why two entries are separate is a signal they
  should be one entry
- Do NOT modify existing lessons without explicit user approval

### Placement

- **Default**: append at the end of the file
- Grouping trumps position: if the candidate belongs beside an existing
  lesson, propose placing it there. Entries are cited by title, so
  reordering is safe and needs no renumbering sweep

Show the complete draft in context (the markdown that would be appended).
**Wait for user approval before applying.**

---

## Step 7: Apply and Stage

After user approval:

1. Append approved entries to `docs/development/lessons_learned.md`
2. Run `markdownlint-cli2 docs/development/lessons_learned.md` to ensure
   the file passes lint (per `.claude/rules/markdown.md`)
3. Stage the file: `git add docs/development/lessons_learned.md`
4. Do NOT commit — staging only

Post-completion notes to display:

- "Changes staged but not committed."
- If an entry was retitled, note it: `src/`, `spec/`, and `tests/` cite
  lessons by title. Search for the old title before finishing.

---

## Guardrails

- **Quality bar is non-negotiable**: do not draft entries that fail the
  "genuinely hard AND likely to recur" test. The file should stay small
  and actionable.
- **No automatic commits**: stage only. The user decides when and how to
  commit.
- **Preserve existing lessons**: no modifications to existing entries
  without explicit user approval. This includes rewording and
  reordering.
- **One home per prescription**: never restate in this file a rule that
  `.claude/rules/` already carries. Point at it instead.
- **Complete evidence trail**: every drafted lesson must cite at least
  one issue number or commit hash. No lessons from vibes.
- **No forced lessons**: "no candidates qualify" is a valid and expected
  outcome. Do not lower the bar to produce output.

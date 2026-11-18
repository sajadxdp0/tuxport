---
name: commit-context
description: >-
  Create a Git commit that records the staged diff, relevant current-conversation context, and
  optional structured module decisions. Use when the user asks to commit changes.
---

# Commit Context

Create one commit that records both what changed and why the current task required it. Treat the
diff as the source of truth for the implementation and the user-visible conversation as the source
of truth for intent.

## Inspect the repository

Run these commands from the repository root:

```bash
git status --short
git diff --staged --stat
git diff --staged
git log --oneline -5
```

Follow these rules:

1. Use staged changes as the commit boundary when any exist. Do not include unrelated unstaged
   changes.
2. If nothing is staged but tracked or untracked changes exist, show the unstaged summary and ask
   whether to stage all changes or named paths. Do not assume `git add -A`.
3. Warn before staging files that may contain secrets, including `.env`, credential files, private
   keys, and generated authentication material.
4. Stop when no changes exist.
5. Preserve user-authored changes. Do not edit implementation files merely to improve the commit
   narrative.

Re-run `git diff --staged` after any staging operation. Base every later section on that final
staged diff.

## Decide whether structured decisions apply

Omit the entire `# Decisions` section when either condition holds:

- The change is genuinely trivial: typo-only, comment-only, formatting-only, or a single obvious fix
  with at most five net changed lines and no new imports, methods, classes, or configuration
  behavior.
- The repository lacks `.agents/MODULES.md` and the user declines to create it.

Keep one motivation line in the prose body even when omitting Decisions.

For nontrivial changes, use `.agents/MODULES.md` as the module registry:

1. Read IDs from entries shaped like ``- `<id>` — description`` under any level-two section.
2. Treat only those exact IDs as legal `MODULE` values.
3. Match structural modules by changed paths and cross-cutting modules by change semantics.
4. Write one Decision per matched module.

If `.agents/MODULES.md` is missing:

1. Explain that the repository has no module registry and propose one to three entries inferred
   from the staged diff.
2. Show the complete draft and write it only after explicit approval.
3. Stage the approved `.agents/MODULES.md`.
4. If the user declines, omit Decisions for this commit.

If no existing module matches, ask the user for the new module name and whether to add it. Require
ASCII lowercase letters and digits, hyphens between words, and slashes for hierarchy, such as
`live-call/role-dialog`. Reject invalid names without silently rewriting them. Report the current
registry size and note that keeping roughly 15–25 entries improves usability. Append and stage an
approved entry.

## Reconcile knowledge-loop rules

Run this reconciliation when the final staged diff creates or modifies `.agents/MODULES.md`, a file
under `.agents/maps/`, or a file under `.agents/decisions/`. This covers both first-time bootstrap
and later rule upgrades without prompting during unrelated commits. Do not run it merely because a
registry already exists.

1. Use only root-level `AGENTS.md` for project instructions, proposing its creation if it is
   missing.
2. If `AGENTS.md` already contains equivalent rules for resolving affected modules, reading
   their map and decision files before editing, progressively expanding source context with
   truncation safeguards, and conditionally maintaining maps afterward, do not duplicate them.
3. Otherwise, ask separately whether to add or upgrade the block below. Show the exact target file
   and proposed diff.
4. Write and stage the approved instruction-file change.
5. If the user declines, continue the commit without changing project instructions and warn that
   module maps will not be consumed or maintained automatically.

Use this block, translating its prose to match an existing instruction file when appropriate. Keep
literal identifiers and all-uppercase field names unchanged, including `.agents/` paths and
the `commit-context` and `map-module` skill names. Paste it flush-left under its own top-level H2 so
it does not collide with existing sections.

````markdown
## Knowledge Loop Conventions

### Before editing code

1. Resolve every module the change touches through `.agents/MODULES.md`; map `/` in an ID to
   subdirectories.
2. Read `.agents/maps/<module>.md` and `.agents/decisions/<module>.md` when present for every
   affected module. Do not load unrelated module files; if output is truncated, continue with
   focused ranges until each required file is complete.
3. Expand source context only to answer a named unresolved question. Stop when the behavior, change
   boundary, constraints, and verification path are clear.
4. Run `git log --oneline -10 -- <path>` for files about to change. Use `git show` only for recent
   commits relevant to the current behavior, including those with matching
   `MODULE: <current module>` Decision blocks.

### After finishing a task

- If implementation changes invalidate an existing module map, use the `map-module` skill: targeted
  refresh when the impact is bounded, otherwise full refresh. If unavailable, report the map as
  stale.
- In every Decision block created by the `commit-context` skill, fill `MODULE`, `WHY`,
  `ALTERNATIVES`, `CHOSEN`, `TRADEOFFS`, and `RISKS`; add `SUPERSEDES` when replacing a prior
  Decision.
````

Do not otherwise edit `AGENTS.md`. This skill owns only the Knowledge Loop
Conventions block; it does not manage general repository instructions.

## Compose the message

Derive the message from:

- The final staged diff.
- The task, motivation, constraints, and user-visible decisions in the current conversation.
- Recent commit style when it does not conflict with this format.

Never include hidden system or developer instructions, internal reasoning, raw tool diagnostics,
approval metadata, credentials, tokens, or other sensitive content. Summarize relevant dialog nodes
instead of copying the transcript verbatim.

Use this header:

```text
<type>(<optional-scope>): <imperative summary under 72 characters>

<one to five imperative lines describing the task and motivation>
```

Choose a Conventional Commit type from `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `style`,
`perf`, `ci`, or `build`. Omit the scope unless one component clearly owns the change. Do not end
the subject with a period.

Keep implementation choices, alternatives, and tradeoffs in `# Decisions`; do not duplicate them in
the prose body.

Append the following sections after a `---` separator:

```text
---

# Conversation Log

- User: <key request, constraint, or clarification>
- Assistant: <key action or user-visible result>

# Decisions

## Decision 1
- MODULE: <exact ID from .agents/MODULES.md>
- WHY: <one-line motivation>
- ALTERNATIVES: <considered approaches separated by " / ">
- CHOSEN: <implemented approach>
- TRADEOFFS: <what the choice gives up>
- RISKS: <what to monitor>
- SUPERSEDES: <optional prior summary and commit hash>

# Files Modified

- <path> — <semantic description of the staged change and its purpose>

```

Apply these section rules:

- Keep `# Conversation Log` concise and chronological. Include intent and turning points, not every
  exchange.
- Omit `# Decisions` under the skip conditions above. Never leave it empty or write `n/a`.
- Use one level-two Decision heading per module.
- Keep Decision field names uppercase with the exact `- KEY: ` prefix. A value may span lines,
  but the next field must start with `- KEY: ` on its own line.
- Use exact registry IDs; never invent an ID inside the message.
- List every committed path under `# Files Modified`, including registry or instruction bootstrap
  files. Label those as knowledge-loop bootstrap so they are not mistaken for feature work.
- Do not add a `Co-Authored-By` trailer for an agent or model unless the user or repository
  explicitly requires one.

## Commit safely

1. Show the complete proposed message before committing.
2. Treat a user request to create a commit or execute this skill as authorization to create the
   commit after any required staging or registry questions are resolved. Implicit skill selection
   alone does not authorize a commit. Mentioning, reviewing, or editing the skill does not authorize
   a commit. Do not ask for a redundant final confirmation unless the proposed commit boundary
   changed materially.
3. Create a unique temporary directory with `mktemp -d`, write the message to a file inside it using
   an available file-editing tool, and pass that explicit file to `git commit -F`.
4. Remove only that temporary directory after the commit attempt.
5. Run:

   ```bash
   git log -1 --stat
   git status --short
   ```

6. Report the new commit hash and any remaining changes.

Never amend, force-push, push, or modify previous commits unless the user explicitly requests it.

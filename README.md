# git-cognitive

Cognitive debt detection and management for Git repositories.
Zero LLM. Zero network. All signals from git.

[![Crates.io](https://img.shields.io/crates/v/git-cognitive)](https://crates.io/crates/git-cognitive)
[![License](https://img.shields.io/crates/l/git-cognitive)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange)](https://www.rust-lang.org)

## The problem

AI coding agents ship code fast. Humans lose track of what was AI-generated and what was reviewed. Over time, unreviewed AI code accumulates as a liability — call it cognitive debt.

## How it works

`git-cognitive index` finds the minimal set of commits that covers the current state of your repo — the last commit that touched each tracked file — and builds a `CommitAudit` for each one:

1. Attributes AI lines by matching agent session tool calls against the commit diff
2. Scores friction via tree-sitter AST analysis (complexity, doc gap, author churn)
3. Detects merge commits (via `--auto-sync` or `sync` command)
4. Stores results in SQLite (`.git/cognitive.db`) and the `cognitive/v1` orphan branch

Each commit gets two files in a sharded orphan branch:

```
cognitive/v1
└── ab/cd/ef/
    ├── activity.json   — friction score, AI attribution, hotspots, zombie flag, committed_at timestamp
    └── session.jsonl   — the agent conversation that produced this commit
```

No external service. No daemon. Everything in git. Sessions are preserved across all merge strategies (three-way, squash, rebase), and audits are re-keyed automatically when commits are rewritten by rebase or amend.

## Supported agents

Sessions are discovered on disk by encoding the repo path into each agent's per-project store — no agent hooks required. Supported agents:

- **claude** — Claude Code
- **cursor** — Cursor
- **factory** — Factory Droid (aliases: `droid`, `factory-droid`)
- **pi** — Pi

Enable one or more with `git-cognitive enable <agent>`. Enabled agents are recorded in `.git/cognitive-agents` (defaults to `claude` when unset).

## Friction score

Four signals, weighted sum (0.0–1.0):

- **Complexity (0.4)** — max cyclomatic complexity across changed files, parsed via tree-sitter AST
- **Doc gap (0.4)** — ratio of undocumented functions in changed files
- **Author churn (0.2)** — distinct committers on changed files in the last 90 days
- **+0.15** if large diff (AI-attributed and >100 lines)
- **+0.20** if fatigue (AI-attributed commit after 3h+ session)

## AI attribution

Checked in priority order:

1. `Agent-Attribution: 75%` trailer in the commit message — exact percentage
2. Line-level matching — agent Write/Edit tool calls vs git diff lines, per file
3. Keyword scan — `Co-Authored-By: Claude`, `co-authored-by: copilot`, `cursor`, `ai-generated`

## Install

```sh
cargo install git-cognitive
```

Or from source:

```sh
git clone https://github.com/ccherrad/git-cognitive
cd git-cognitive
cargo install --path .
```

## Quickstart

```sh
# 1. Enable automatic indexing on every commit (claude, cursor, factory, pi)
git-cognitive enable claude

# 2. Index the current repo state
git-cognitive index

# 3. Inspect a file with cognitive overlay
git-cognitive blame src/main.rs

# 4. Share with your team
git-cognitive push
```

## Commands

### `enable`

```
git-cognitive enable <agent>   # claude, cursor, factory, pi
```

Records the agent in `.git/cognitive-agents` and installs three git hooks:

- **post-commit** — index the new commit in the background (`git commit` returns immediately)
- **post-rewrite** — after a rebase/amend, migrate audits to the rewritten SHAs
- **pre-push** — prune orphaned audits (`gc`), then push `cognitive/v1` to origin

Run `enable` again with a different agent to track several at once.

### `index`

```
git-cognitive index [--auto-sync] [--output-json <path>]
```

Finds the minimal covering set of commits for the current repo state: `git ls-files` → last-touching SHA per file → dedup → skip already-indexed. Idempotent — safe to run repeatedly.

Flags:
- `--auto-sync` — automatically detect and sync merge commits before indexing
- `--output-json <path>` — export audits as JSON (useful for cloud DB integration)

### `blame`

```
git-cognitive blame <file>
```

Interactive TUI showing git blame output with cognitive audit data overlaid per line:

- Friction score bar (green → yellow → red)
- AI attribution %
- Zombie flag ☠

Controls: `↑↓`/`jk` navigate, `Enter` drill into full audit detail, `q` quit.

### `show`

```
git-cognitive show <SHA>|HEAD
```

Full audit detail for a commit: friction score, AI attribution, lines changed, session duration, fatigue flag, zombie flag, and per-file hotspots (complexity + doc gap).

### `session`

```
git-cognitive session <SHA>|HEAD
```

Shows the full agent conversation (prompts, responses, tool calls) captured for this commit's window, plus the list of files it modified.

### `gc`

```
git-cognitive gc
```

Prune audits for commits no longer reachable from any local branch — cleans up entries orphaned when a rebase or squash-merge drops their original SHAs. Runs automatically on `pre-push`.

### `sync`

```
git-cognitive sync
```

Detect and sync merge commits (including three-way, squash, and rebase merges) from the working tree to the cognitive debt branch. Useful for tracking merge commits created via GitHub/Bitbucket UI.

### `push` / `pull`

```
git-cognitive push
git-cognitive pull
```

Share cognitive debt data with your team via the `cognitive/v1` branch. Preserves session data across all merge strategies and export methods.

### `mcp`

```
git-cognitive mcp
```

Start a JSON-RPC MCP server over stdio. Tools:

- **`show`** — full audit for a commit SHA
- **`blame`** — per-line cognitive data for a file (friction, AI%, zombie flag)

## Zombie detection

A zombie is an AI-attributed commit where none of the changed files have been touched by a human commit in the last 30 days. Flagged automatically during indexing and visible in `blame` as ☠.

## Agent-Attribution trailer

Add to commit messages for precise attribution without relying on line matching:

```
feat: add payment processing flow

Agent-Attribution: 80%
```

## See also

[git-semantic](https://github.com/ccherrad/git-semantic) — semantic code search, sibling tool.

## License

MIT OR Apache-2.0

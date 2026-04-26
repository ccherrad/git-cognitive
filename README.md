# git-cognitive

Cognitive debt detection and management for Git repositories.
Zero LLM. Zero network. All signals from git.

[![Crates.io](https://img.shields.io/crates/v/git-cognitive)](https://crates.io/crates/git-cognitive)
[![License](https://img.shields.io/crates/l/git-cognitive)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange)](https://www.rust-lang.org)

## The problem

AI coding agents ship code fast. Humans lose track of what was AI-generated and what was reviewed. Over time, unreviewed AI code accumulates as a liability — call it cognitive debt.

## How it works

Every `git commit` automatically:
1. Slices the active Claude Code session to the window between this commit and the previous one
2. Attributes AI lines by matching agent Write/Edit tool calls against the commit diff
3. Scores friction via tree-sitter AST analysis (complexity delta, doc gap, author churn)
4. Classifies the commit and writes to the `cognitive-debt/v1` orphan branch

Each commit gets three files stored in a sharded orphan branch:

```
cognitive-debt/v1
└── ab/cd/ef1234/
    ├── activity.json     — classification, friction score, AI attribution, endorsement status
    ├── endorsements.json — who endorsed it and when
    └── session.jsonl     — the Claude conversation that produced this commit
```

No external service. No daemon. Everything in git.

## Friction score

Three signals, weighted sum (0.0–1.0):

- **Complexity delta (0.4)** — real decision points added (`if`, `match arm`, `&&`, `||`, etc.) parsed via tree-sitter AST
- **Doc gap (0.4)** — functions added without a doc comment node in the AST
- **Author churn (0.2)** — distinct committers on changed files in the last 90 days

## AI attribution

Checked in priority order:

1. `Agent-Attribution: 75%` trailer in the commit message — exact percentage
2. Line-level matching — agent Write/Edit lines vs git diff lines, per file
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
# 1. Enable automatic auditing on every commit
git-cognitive enable claude

# 2. See the debt
git-cognitive debt

# 3. Endorse commits interactively
git-cognitive endorse

# 4. Share with your team
git-cognitive push
```

## Commands

### `enable`

```
git-cognitive enable claude
```

Writes `.git/hooks/post-commit` to auto-audit and push on every commit.

### `audit`

```
git-cognitive audit [--commit <SHA>|HEAD] [--since <SHA>] [--all] [--check-zombies]
```

Walks commits and writes activity to `cognitive-debt/v1`. Run manually to backfill history.

- `--all` — backfill last 500 commits
- `--check-zombies` — flag AI commits unendorsed >30 days with no human follow-up

### `debt`

```
git-cognitive debt [--interactive]
```

Flat table of all commits: SHA, classification, friction score, AI%, title, endorsement status.

### `endorse`

```
git-cognitive endorse [<SHA>|HEAD]
git-cognitive endorse          # interactive TUI picker
```

Mark a commit as understood and vouched for. Pushes automatically.

TUI controls: `↑↓`/`jk` navigate, `e`/Enter endorse, `s` git show, `q` quit.

### `show`

```
git-cognitive show <SHA>|HEAD
```

Full activity item + endorsement history for a commit.

### `session`

```
git-cognitive session <SHA>|HEAD
```

Shows the Claude conversation (prompts, responses, tool calls) captured for this commit's window.

### `push` / `pull`

```
git-cognitive push
git-cognitive pull
```

Share cognitive debt data with your team via the `cognitive-debt/v1` branch.

## Classifications

| Class | Trigger |
|---|---|
| `risk` | AI ≥70% + feat, or files in `auth/`, `payments/`, `migrations/`, `.sql` |
| `tech_debt` | AI ≥70% + refactor/chore |
| `new_feature` | `feat:`, `add:`, `new:` + low AI |
| `bug_fix` | `fix:`, `bug:` |
| `refactor` | `refactor:`, `chore:` + low AI |
| `dependency_update` | `Cargo.lock`, `yarn.lock`, etc. — auto-excluded from endorsement queue |
| `minor` | `docs:`, `test:`, `ci:`, `style:` — auto-excluded |
| `other` | anything else |

## Zombie detection

A zombie is an AI-attributed commit that:
1. Has been unendorsed for >30 days
2. Has had no human follow-up commit touching the same files

```sh
git-cognitive audit --check-zombies
```

## Agent-Attribution trailer

Add to commit messages for precise attribution without relying on line matching:

```
feat: add payment processing flow

Agent-Attribution: 80%
```

## Team workflow

```sh
# morning
git-cognitive pull
git-cognitive debt

# review queue
git-cognitive endorse

# weekly
git-cognitive audit --check-zombies
```

## See also

[git-semantic](https://github.com/ccherrad/git-semantic) — semantic code search, sibling tool.

## License

MIT OR Apache-2.0

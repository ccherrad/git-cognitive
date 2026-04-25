# git-cognitive

Cognitive debt detection and management for Git repositories.
Zero LLM. Zero network. All signals from git.

[![Crates.io](https://img.shields.io/crates/v/gitcog)](https://crates.io/crates/gitcog)
[![License](https://img.shields.io/crates/l/gitcog)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange)](https://www.rust-lang.org)

## The problem

AI coding agents ship code fast. Humans lose track of what was AI-generated and what was reviewed. Over time, unreviewed AI code accumulates as a liability — call it cognitive debt.

## How it works

Storage: `cognitive-debt/v1` orphan branch (sharded JSON). Local cache: `.git/cognitive.db` (SQLite, read-only index). No external services.

Three signals feed the friction score (0.0–1.0):

- **Complexity delta** (0.4 weight) — conditional branches added/removed in diff
- **Doc gap** (0.4 weight) — ratio of comment lines to logic lines in diff
- **Author churn** (0.2 weight) — distinct committers on changed files in last 90 days

AI attribution: reads `Agent-Attribution: 80%` trailer from commit message, falls back to `Co-Authored-By: Claude` keyword heuristic, or parses Claude Code JSONL transcripts for line-level accuracy.

## Install

```sh
cargo install gitcog
```

Or from source:

```sh
git clone https://github.com/ccherrad/git-cognitive
cd git-cognitive
cargo install --path .
```

## Quickstart

```sh
# 1. Set up automatic auditing on every commit
git-cognitive install

# 2. Audit the last commit manually
git-cognitive audit --commit HEAD

# 3. See the heatmap
git-cognitive debt

# 4. Endorse commits interactively
git-cognitive endorse

# 5. Check who knows what
git-cognitive debt --who

# 6. Inspect a specific commit
git-cognitive show HEAD
```

## Commands

### `audit`

```
git-cognitive audit [--commit <SHA>|HEAD] [--since <SHA>] [--all] [--check-zombies]
```

Walks commits and writes activity items to `cognitive-debt/v1`. Tracks:

- Classification: `new_feature`, `bug_fix`, `refactor`, `tech_debt`, `risk`, `minor`, `dependency_update`, `subsystem_change`
- AI-free zone enforcement: paths matching `auth/`, `payments/`, `migrations/`, `.sql` are forced to `risk` regardless of commit message
- Friction score (0.0–1.0)
- AI attribution (from commit trailers or keyword heuristics)

### `endorse`

```
git-cognitive endorse [HEAD|<SHA>] [--status reviewed|endorsed]
git-cognitive endorse          # interactive TUI picker
```

Records endorsement on the `cognitive-debt/v1` branch and pushes automatically. Interactive picker: ↑↓/jk navigate, `e`/Enter endorse, `r` reviewed, `s` git show, `q` quit.

### `debt`

```
git-cognitive debt [--subsystem <name>] [--interactive] [--who]
```

- Default: heatmap table — subsystems × items × endorsed × friction × zombies
- `--interactive`: opens TUI picker filtered to all non-excluded items
- `--who`: bus factor table — who endorsed what per subsystem, color-coded (red = 0, yellow = 1, green = 2+)

### `show`

```
git-cognitive show <SHA>|HEAD
```

Displays full activity item + complete endorsement history for a commit.

### `session`

```
git-cognitive session capture [--session-id <UUID>]
```

Parses a Claude Code JSONL transcript (`~/.claude/projects/<project>/<id>.jsonl`) to compute line-level AI attribution per commit. Called automatically by the Stop hook when using `git-cognitive install`.

### `install`

```
git-cognitive install
```

Writes `.git/hooks/post-commit` to auto-audit on every commit. Prints instructions for adding the Stop hook to `~/.claude/settings.json` for automatic session capture.

## Storage model

```
cognitive-debt/v1 (orphan branch)
└── <ab>/<cd>/<ef1234>/
    ├── activity.json      — classification, friction, AI attribution, endorsement status
    ├── endorsements.json  — ordered list of endorsement events with author + timestamp
    └── session.json       — Claude Code session transcript (if captured)
```

Sharding: first 6 hex chars of SHA split as `ab/cd/ef` — same layout as `entireio/cli`.

Teams share cognitive debt data by pushing/fetching the `cognitive-debt/v1` branch:

```sh
git push origin cognitive-debt/v1
git fetch origin cognitive-debt/v1:cognitive-debt/v1
```

## AI-free zones

Files matching these patterns are always classified as `risk`, regardless of commit message:

```
auth/, authentication/, authorization/
payments/, payment/, billing/
migrations/, migration/, schema, .sql
```

## Zombie detection

A zombie is an AI-attributed commit that:
1. Has been unendorsed for > 30 days
2. Has had no human follow-up commit touching the same files

Run: `git-cognitive audit --check-zombies`

## Agent-Attribution trailer

Add to commit messages for precise attribution without parsing transcripts:

```
feat: add user authentication flow

Agent-Attribution: 75%
```

## See also

[git-semantic](https://github.com/ccherrad/git-semantic) — semantic code search, sibling tool.

## License

MIT OR Apache-2.0

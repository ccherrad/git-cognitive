# Changelog

All notable changes to git-cognitive are documented here.

## [0.3.5] - 2026-06-21

### Fixed
- Test suite: correct merge detection test to not depend on database state
- Ensure tests pass independently without external state

## [0.3.4] - 2026-06-21

### Added
- `git-cognitive sync` command to detect and sync merge commits to cognitive debt branch
- `--auto-sync` flag on `index` command for automatic merge detection before indexing
- `--output-json` flag on `index` command to export audits as JSON (cloud DB integration)
- `committed_at` timestamp field to track when commits were made (vs when audit ran)
- Merge detection logic to handle three-way, squash, and rebase merges
- Unit tests for merge detection and JSON export
- TESTING.md with comprehensive manual test scenarios
- Session preservation across all merge strategies and export methods

### Changed
- Renamed `activity_items` table to `commit_audits` for clarity
- Sessions now captured automatically for all commits (including merges)

### Fixed
- Ensure merge commits are properly audited and tracked in debt branch
- Handle GitHub/Bitbucket UI merges via local `git pull` + `sync`

## [0.3.3] - 2026-05-15

### Added
- Initial release with cognitive debt indexing
- AI attribution detection via session matching and keyword heuristics
- Friction scoring based on complexity, doc gaps, author churn
- SQLite storage of audits in `.git/cognitive.db`
- Orphan branch storage in `cognitive/v1`
- Interactive blame view with cognitive overlay
- MCP server support for Claude integration
- Zombie detection for unreviewed AI code


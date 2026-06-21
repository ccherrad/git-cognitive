# Testing Guide for Merge Sync Features

## Automated Tests

Run the test suite:
```sh
cargo test
```

Tests cover:
- Merge detection (no merges, with merge)
- Unsynced merge tracking

## Manual Testing

### Setup Test Repo

```sh
mkdir /tmp/test-repo && cd /tmp/test-repo
git init
git config user.email "test@example.com"
git config user.name "Test User"
```

### Test 1: Three-way merge detection

```sh
echo "main content" > file.txt
git add file.txt && git commit -m "initial"

git checkout -b feature
echo "feature content" > file.txt
git commit -am "feature work"

git checkout main
echo "main update" > other.txt
git commit -am "main work"

git merge feature -m "merge feature"

# Test sync
cargo run -- sync

# Should output: "Found 1 merge commit(s) to sync"
```

### Test 2: Squash merge detection

```sh
git checkout -b feature2
echo "work1" > f1.txt && git add f1.txt && git commit -m "commit 1"
echo "work2" > f2.txt && git add f2.txt && git commit -m "commit 2"

git checkout main
git merge --squash feature2 -m "squash feature2"
git add . && git commit

cargo run -- sync

# Should detect and sync the squash commit
```

### Test 3: Rebase merge detection

```sh
git checkout -b feature3
echo "rebase work" > f3.txt && git add f3.txt && git commit -m "feature3"

git checkout main
git rebase feature3

cargo run -- sync

# Should detect rebased commits
```

### Test 4: Auto-sync with index

```sh
cargo run -- index --auto-sync

# Should:
# 1. Detect and sync any merges
# 2. Run normal index
# 3. Output both sync and index results
```

### Test 5: JSON export

```sh
cargo run -- index --output-json /tmp/audits.json

# Should create audits.json with audit data
cat /tmp/audits.json | jq '.[] | {id, title, cognitive_friction_score}'
```

### Test 6: End-to-end workflow (simulating GitHub merge)

```sh
# Simulate pull request merge from GitHub UI
git fetch origin main:temp-main
git checkout temp-main
git merge --no-ff -m "Merge pull request #123" main
git checkout main
git reset --hard temp-main

# Now pull locally and sync
git pull
cargo run -- index --auto-sync --output-json /tmp/cloud-audits.json

# Verify audits captured with sessions
jq '.[] | {id, title, session: (.session | length)}' /tmp/cloud-audits.json
```

## Verification Checklist

- [ ] No merges detected when no merges exist
- [ ] Merge commits are detected after `git merge`
- [ ] Merge commits are detected after `git rebase`
- [ ] Squash merges are detected
- [ ] `--auto-sync` runs sync before index
- [ ] JSON output contains all audit fields
- [ ] Sessions are embedded in JSON audits
- [ ] Sync idempotent (running twice has same result)
- [ ] Works with real repos (not just test cases)

## Cloud Integration Test

```sh
# Export audits as JSON
cargo run -- index --output-json audits.json

# Send to PostgreSQL (example)
psql your_db << EOF
\copy commit_audits FROM PROGRAM 'cat audits.json | jq -r ".[].id, .[].title, .[].cognitive_friction_score"' (DELIMITER ',')
EOF
```

## Known Limitations

- Merge detection requires git history to be available locally
- GitHub/Bitbucket UI merges must be pulled before `sync` runs
- Squash merges are detected but original commits not recovered

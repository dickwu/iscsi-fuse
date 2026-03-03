---
name: publish
description: Publish a new release of iscsi-fuse. Bumps version, creates GitHub release, updates Homebrew formula, and installs locally via brew upgrade.
argument-hint: "[version e.g. 0.2.0]"
allowed-tools: Bash, Read, Edit, Glob, Grep
---

You are publishing a new release of **iscsi-fuse**. Follow these steps precisely. If $ARGUMENTS is provided, use it as the new version number (e.g. `0.2.0`). Otherwise, ask the user for the version.

## Variables
- **REPO_DIR**: `/Users/peilinwu/project/iscsi`
- **TAP_DIR**: `/Users/peilinwu/project/iscsi/homebrew-iscsi-fuse`
- **GITHUB_REPO**: `dickwu/iscsi-fuse`
- **TAP_REPO**: `dickwu/homebrew-iscsi-fuse`
- **NEW_VERSION**: `$ARGUMENTS` (strip any leading `v`)

## Pre-flight checks
1. Confirm the working directory is clean (`git status`)
2. Confirm we are on branch `main` and up to date with origin
3. Confirm macFUSE is installed (`brew list --cask macfuse`)

## Steps

### 1. Bump version in Cargo.toml
Edit `Cargo.toml` — change `version = "X.Y.Z"` to `version = "$ARGUMENTS"`.

### 2. Run cargo check to validate
```bash
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo check 2>&1
```
If it fails, stop and report the error.

### 3. Format and lint
```bash
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo fmt --all
cargo clippy --all-targets -- -D warnings 2>&1
```

### 4. Commit the version bump
```bash
git add Cargo.toml Cargo.lock src/
git commit -m "chore: release v$ARGUMENTS

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

### 5. Tag and push
```bash
git tag "v$ARGUMENTS"
git push origin main
git push origin "v$ARGUMENTS"
```

### 6. Wait for GitHub Actions release workflow
```bash
# Find the run ID of the release workflow for this tag
sleep 5
RUN_ID=$(gh run list --repo dickwu/iscsi-fuse --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')
gh run watch "$RUN_ID" --repo dickwu/iscsi-fuse
```
If the workflow fails, show the failure logs with `gh run view --log-failed`.

### 7. Get the release asset name and compute SHA256
```bash
ASSET=$(gh release view "v$ARGUMENTS" --repo dickwu/iscsi-fuse --json assets --jq '.assets[0].name')
mkdir -p /tmp/iscsi-release-new
gh release download "v$ARGUMENTS" --repo dickwu/iscsi-fuse --dir /tmp/iscsi-release-new
SHA256=$(shasum -a 256 "/tmp/iscsi-release-new/$ASSET" | awk '{print $1}')
echo "Asset: $ASSET"
echo "SHA256: $SHA256"
```

### 8. Update the Homebrew formula
In `TAP_DIR/Formula/iscsi-fuse.rb`:
- Update `version "X.Y.Z"` to `version "$ARGUMENTS"`
- Update `sha256 "..."` to the new SHA256
- Update the URL if the asset filename changed (arm64 vs universal)

Then commit and push:
```bash
cd TAP_DIR
git add Formula/iscsi-fuse.rb
git commit -m "Update iscsi-fuse to v$ARGUMENTS

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
git push origin main
```

### 9. Install locally via brew
```bash
brew untap dickwu/iscsi-fuse 2>/dev/null || true
brew tap dickwu/iscsi-fuse
brew upgrade iscsi-fuse 2>/dev/null || brew install iscsi-fuse
```

Verify:
```bash
iscsi-fuse --help
which iscsi-fuse
```

### 10. Report success
Print a summary:
- New version installed
- GitHub Release URL
- Homebrew tap URL
- `brew tap dickwu/iscsi-fuse && brew install iscsi-fuse` command for users

## Error handling
- If any step fails, stop immediately and report what went wrong
- If the release workflow fails, show `gh run view --log-failed`
- If brew install fails, show the full error
- Do NOT continue past a failure — the user must decide how to proceed

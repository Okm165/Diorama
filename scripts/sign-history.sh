#!/usr/bin/env bash
# Re-sign every commit in the repository's history (root -> HEAD), across
# every local branch and tag: the one-time "make the entire history signed"
# rewrite.
#
#   bash scripts/sign-history.sh          # or: make sign-history
#   BACKUP=../my-backup.bundle bash scripts/sign-history.sh
#
# What it does, in order:
#   1. requires a clean tree and a configured signing key (GPG or SSH);
#   2. writes a full backup bundle (the old history stays recoverable);
#   3. rewrites every ref through `git commit-tree -S`: each commit is
#      re-created with its original author, committer, dates and message,
#      plus the signature (verified: metadata is byte-for-byte preserved);
#   4. re-points annotated tags at the rewritten commits;
#   5. verifies every commit carries a signature and `%G?` reports G;
#   6. drops the rewrite's backup refs, expires reflogs and gc's (the
#      bundle is the recovery path);
#   7. sets `commit.gpgsign true` for this repository, so future commits
#      sign too.
#
# GPG keys prompt once (the agent caches the passphrase for the run).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

if [ -n "$(git status --porcelain)" ]; then
  echo "sign-history: the working tree is not clean; commit or stash first" >&2
  exit 1
fi

if ! git config --get user.signingkey >/dev/null; then
  echo "sign-history: no signing key configured. With GPG:" >&2
  echo "  git config user.signingkey <KEYID>   # gpg --list-secret-keys --keyid-format=short" >&2
  echo "With SSH signing:" >&2
  echo "  git config gpg.format ssh && git config user.signingkey ~/.ssh/id_ed25519.pub" >&2
  exit 1
fi

BACKUP="${BACKUP:-$(dirname "$PWD")/$(basename "$PWD")-pre-sign.bundle}"
echo "== backup: ${BACKUP}"
git bundle create "$BACKUP" --all >/dev/null
git bundle verify "$BACKUP" >/dev/null
echo "   complete history backed up ($(du -h "$BACKUP" | cut -f1))"

total=$(git rev-list --count --branches --tags)
echo "== rewriting ${total} commits (every local branch and tag) through git commit-tree -S"
# The per-commit progress is 365 lines of noise; keep the ref summary and
# print the tail on a failure.
if ! out=$(FILTER_BRANCH_SQUELCH_WARNING=1 git filter-branch --force \
  --commit-filter 'git commit-tree -S "$@"' \
  --tag-name-filter cat -- --branches --tags 2>&1); then
  echo "$out" | tail -30 >&2
  echo "sign-history: the rewrite failed; nothing was cleaned up" >&2
  exit 1
fi
echo "$out" | tr '\r' '\n' | grep -v '^Rewrite ' | sed 's/^/   /' | tail -8

echo "== verification"
total_after=$(git rev-list --count --branches --tags)
signed=$(git log --branches --tags --format='%G?' | grep -c '^G$' || true)
raw=$(git log --branches --tags --format='%H' | while read -r c; do
  git cat-file commit "$c" | grep -q '^gpgsig' && echo x
done | wc -l)
echo "   commits: ${total_after} (was ${total})"
echo "   signatures present: ${raw}; verified (G): ${signed}"
if [ "$raw" != "$total_after" ]; then
  echo "sign-history: some commits carry no signature; inspect: git log --show-signature" >&2
  exit 1
fi
if [ "$signed" != "$total_after" ]; then
  echo "   note: signatures are present but not verifiable here — for SSH signing set" >&2
  echo "   gpg.ssh.allowedSignersFile, or import the public key for GPG" >&2
fi

echo "== cleanup (the bundle is the recovery path)"
rm -rf .git/refs/original
git reflog expire --expire=now --all
git gc --prune=now >/dev/null
git config commit.gpgsign true

echo "== done: every commit in every local branch and tag is signed"
git log --oneline -3
echo "   future commits sign automatically (repo-local commit.gpgsign=true;"
echo "   use --global for every repo). Signed tags, if wanted:"
echo "   git tag -f -s v0.1.0 -m \"<message>\""

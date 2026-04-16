#!/usr/bin/env bash
# Sync local main with upstream bytecodealliance/wrpc and push to fork
set -euo pipefail

git fetch upstream
git checkout main
git rebase upstream/main
git push origin main --force-with-lease
echo "✓ main synced with upstream"

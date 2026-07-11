#!/usr/bin/env bash
set -e

echo "Configuring git hooks path..."
chmod +x .githooks/pre-commit .githooks/pre-push
git config core.hooksPath .githooks
echo "Done."

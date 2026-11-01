#!/usr/bin/env bash
# Print the notes of a GitHub Release: that version's CHANGELOG.md section, then how to verify the
# binaries. Fails if the section is missing or empty, so a release cannot ship without one.
#
#   scripts/release-notes.sh v0.3.0
#   scripts/release-notes.sh main      # any other ref: the version in Cargo.toml (a dry run)
set -euo pipefail

cd "$(dirname "$0")/.."
ref=${1:?usage: release-notes.sh <tag or ref>}
case $ref in
    v[0-9]*) version=${ref#v} ;;
    *) version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1) ;;
esac

section=$(awk -v v="[$version]" '
    index($0, "## " v) == 1 { found = 1; next }
    /^## \[/ { found = 0 }
    found' CHANGELOG.md)

if ! grep -q '[^[:space:]]' <<<"$section"; then
    echo "release-notes: CHANGELOG.md has no section for $version" >&2
    exit 1
fi

printf '%s\n\n---\n\n' "$section"
cat <<'NOTES'
Binaries for x86_64 and aarch64 Linux (glibc, built on Ubuntu 24.04). Verify with `SHA256SUMS` or
`gh attestation verify`.
NOTES

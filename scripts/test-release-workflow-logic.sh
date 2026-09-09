#!/usr/bin/env bash
# Local dry-run of the workflow's tag-guard + notes-extraction logic.
# Simulates: TAG from GITHUB_REF_NAME, the PKG_VERSION computation, and
# the CHANGELOG section extraction, exactly as release.yml does.
set -euo pipefail
cd "$(dirname "$0")/.."

TAG="${1:?usage: test-release-workflow-logic.sh <tag>}"
export GITHUB_REF_NAME="$TAG"
GITHUB_ENV="$(mktemp)"

# --- tag guard (verbatim from release.yml) ---
TAG="${GITHUB_REF_NAME}"
CARGO_VER="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
BASE="${TAG#v}"
BASE="${BASE%-hyperv.*}"
if [[ "$BASE" != "$CARGO_VER" ]]; then
  echo "GUARD FAIL: tag '$TAG' base '$BASE' != Cargo.toml version '$CARGO_VER'"
  exit 1
fi
if [[ "${TAG#v}" == *"-hyperv."* ]]; then
  N="${TAG#*-hyperv.}"
  PKG_VERSION="${BASE}-hyperv${N}"
else
  PKG_VERSION="${BASE}"
fi
echo "PKG_VERSION=${PKG_VERSION}" >> "$GITHUB_ENV"
echo "tag OK: base=$BASE pkg=${PKG_VERSION}"

# --- notes extraction (verbatim from release.yml) ---
SECTION="${TAG#v}"
BASE="${SECTION%-hyperv.*}"
BODY=""
for head in "[${SECTION}]" "[${BASE}]"; do
  # Guard via byte count, NOT `awk ... | grep -q .`: grep -q exits at the
  # first matching line, closing the pipe while awk still has output
  # buffered → awk dies on SIGPIPE (141) → with pipefail the whole
  # condition is false → extraction silently falls back to the commit
  # list for EVERY multi-line section. (Same fix as release.yml; see the
  # "release: fix CHANGELOG notes extraction" commit.)
  OUT="$(awk -v h="${head}" '
    index($0, "## " h) == 1 { inside=1; next }
    inside && index($0, "## [") == 1 { inside=0 }
    inside { print }
  ' CHANGELOG.md)"
  if [[ -n "$OUT" ]]; then
    BODY="$OUT"
    break
  fi
done
if [[ -z "$BODY" ]]; then
  echo "NOTES FALLBACK (no CHANGELOG section) — would use git log"
else
  echo "NOTES FOUND for $head:"
  echo "$BODY" | head -8
  echo "  ... ($(echo "$BODY" | wc -l) lines total)"
fi
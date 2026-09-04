#!/usr/bin/env bash
# One-off spot check: is anything credential-shaped tracked in the source
# tree? Not a build gate — a manual review tool. Run from repo root:
#   bash scripts/spot-check-secrets.sh
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0

echo "=== 1. GitHub/AWS/Slack/Stripe token formats in tracked files:"
if git grep -nIE 'gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|xox[abprs]-[A-Za-z0-9-]{10,}|sk_live_[A-Za-z0-9]{20,}' -- . ':(exclude)CHANGELOG.md'; then
  fail=1
else
  echo "  none"
fi

echo "=== 2. Credential-looking literal assignments in tracked files:"
if git grep -nIE '(PASSWORD|TOKEN|SECRET|API_KEY|CLIENT_SECRET)["'"'"']?[[:space:]]*[=:][[:space:]]*["'"'"'][A-Za-z0-9+/=_@.~!?-]{12,}' -- '*.rs' '*.toml' '*.yml' '*.sh' '*.c'; then
  fail=1
else
  echo "  none"
fi

echo "=== 3. Auth-embedded URLs (https://user:pass@host) in tracked files:"
if git grep -nIE 'https://[^/[:space:]]+:[^@[:space:]]+@' -- .; then
  fail=1
else
  echo "  none"
fi

echo "=== 4. Embedded PEM bodies (header followed by base64) in tracked files:"
# Bare "-----BEGIN PRIVATE KEY-----" string literals are legit cert-validation
# code; only a header IMMEDIATELY followed by 40+ base64 chars is a real key.
found=0
while IFS= read -r -d '' f; do
  if grep -A2 -- '-----BEGIN [A-Z ]*PRIVATE KEY' "$f" 2>/dev/null | grep -qE '^[A-Za-z0-9+/]{40,}={0,2}$'; then
    echo "  $f"
    found=1
  fi
done < <(git ls-files -z -- '*.rs' '*.toml' '*.yml' '*.sh' '*.c' '*.pem' '*.key' '*.json')
if [[ $found -eq 0 ]]; then
  echo "  none"
else
  fail=1
fi

echo
if [[ $fail -ne 0 ]]; then
  echo "RESULT: REVIEW NEEDED — findings above"
  exit 1
fi
echo "RESULT: CLEAN — no credential-shaped content found in tracked files"
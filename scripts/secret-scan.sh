#!/usr/bin/env bash
set -euo pipefail

# bbscord secret firewall: blocks Discord tokens, GitHub tokens, PEM private keys,
# hardcoded API keys, and forbidden credential files from entering git history.
#
# Usage:
#   ./scripts/secret-scan.sh --staged        (pre-commit hook)
#   ./scripts/secret-scan.sh --all-commits   (pre-push hook / CI full history audit)

MODE="${1:---staged}"

case "$MODE" in
  --staged|--all-commits)
    ;;
  -h|--help)
    echo "Usage: $0 [--staged | --all-commits]"
    exit 0
    ;;
  *)
    echo "error: unknown mode '$MODE'. Use --staged or --all-commits." >&2
    exit 2
    ;;
esac

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

# Regex patterns (constructed so the scanner script does not self-match).
DISCORD_TOKEN_RE='[MN][A-Za-z0-9]{23,}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,}'
DISCORD_MFA_RE='mfa\.[A-Za-z0-9_-]{80,}'
GITHUB_TOKEN_RE='gh[opsu]_[A-Za-z0-9]{20,}'
PEM_KEY_RE='[-]{5}BEGIN ([A-Z0-9 ]+ )?PRIVATE KEY[-]{5}'
API_KEY_RE='api_key[[:space:]]*=[[:space:]]*"[^"]{12,}"'

FORBIDDEN_FILE_RE='(^|/)\.env($|\.)|\.(token|key|pem|secret)$|(^|/)[^/]*keyring[^/]*$|(^|/)(config\.local\.toml|secrets\.toml)$'

VIOLATIONS=0

check_filenames() {
  local scope="$1"
  local file
  while IFS= read -r file; do
    [[ -z "$file" ]] && continue
    if [[ "$file" =~ $FORBIDDEN_FILE_RE ]]; then
      echo "[SECRET-SCAN BLOCK] Forbidden secret filename in ${scope}: ${file}" >&2
      VIOLATIONS=$((VIOLATIONS + 1))
    fi
  done
}

scan_diff_stream() {
  local scope="$1"
  local current_commit="staged"
  local current_file=""
  local line=""
  local content=""

  while IFS= read -r line; do
    if [[ "$line" =~ ^commit[[:space:]]([0-9a-f]{7,40}) ]]; then
      current_commit="${BASH_REMATCH[1]}"
      continue
    fi
    if [[ "$line" == "+++ b/"* ]]; then
      current_file="${line#+++ b/}"
      continue
    fi
    if [[ "$line" == "+++ /dev/null" ]]; then
      current_file=""
      continue
    fi
    # Only scan added lines in unified diffs.
    [[ "$line" != "+"* || "$line" == "+++"* ]] && continue
    content="${line#+}"

    if [[ "$content" =~ $DISCORD_TOKEN_RE ]]; then
      echo "[SECRET-SCAN BLOCK] Discord token pattern detected (${scope}, commit=${current_commit}, file=${current_file}) [REDACTED]" >&2
      VIOLATIONS=$((VIOLATIONS + 1))
    fi

    if [[ "$content" =~ $DISCORD_MFA_RE ]]; then
      echo "[SECRET-SCAN BLOCK] Discord MFA token pattern detected (${scope}, commit=${current_commit}, file=${current_file}) [REDACTED]" >&2
      VIOLATIONS=$((VIOLATIONS + 1))
    fi

    if [[ "$content" =~ $GITHUB_TOKEN_RE ]]; then
      echo "[SECRET-SCAN BLOCK] GitHub token pattern detected (${scope}, commit=${current_commit}, file=${current_file}) [REDACTED]" >&2
      VIOLATIONS=$((VIOLATIONS + 1))
    fi

    if [[ "$content" =~ $PEM_KEY_RE ]]; then
      echo "[SECRET-SCAN BLOCK] PEM private key detected (${scope}, commit=${current_commit}, file=${current_file}) [REDACTED]" >&2
      VIOLATIONS=$((VIOLATIONS + 1))
    fi

    if [[ "$content" =~ $API_KEY_RE ]]; then
      # Exempt mock/test fixtures only for synthetic api_key assignments.
      if [[ "$current_file" != *"mock"* && "$current_file" != *"/tests/"* && "$current_file" != "tests/"* && "$current_file" != *"_test.rs" && "$content" != *"secret-scan:allow-mock"* ]]; then
        echo "[SECRET-SCAN BLOCK] Hardcoded api_key literal detected (${scope}, commit=${current_commit}, file=${current_file}) [REDACTED]" >&2
        VIOLATIONS=$((VIOLATIONS + 1))
      fi
    fi
  done
}

if [[ "$MODE" == "--staged" ]]; then
  check_filenames "staged" < <(git diff --cached --name-only --diff-filter=ACMR)
  scan_diff_stream "staged" < <(git diff --cached -U0 --no-color --diff-filter=ACMR)
elif [[ "$MODE" == "--all-commits" ]]; then
  if git rev-parse --verify HEAD >/dev/null 2>&1; then
    check_filenames "git-history" < <(git log --all --pretty=format: --name-only | sort -u)
    scan_diff_stream "git-history" < <(git log --all -p -U0 --no-color --diff-filter=ACMR)
  else
    echo "[secret-scan] No commits in repository yet; checking staged index instead."
    check_filenames "staged" < <(git diff --cached --name-only --diff-filter=ACMR)
    scan_diff_stream "staged" < <(git diff --cached -U0 --no-color --diff-filter=ACMR)
  fi
fi

if [[ "$VIOLATIONS" -gt 0 ]]; then
  echo "secret-scan FAILED with ${VIOLATIONS} violation(s). Remove secrets before committing/pushing." >&2
  exit 1
fi

echo "[secret-scan] PASS (${MODE})"
exit 0

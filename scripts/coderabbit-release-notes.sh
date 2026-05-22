#!/usr/bin/env bash
# Build a release-notes Markdown body covering every change since the last tag.
#
# Walks all commits between the previous tag and CURRENT_TAG and produces:
#   - "## Highlights"      — the "Summary by CodeRabbit" block CodeRabbit writes
#                            into each backing PR's description.
#   - "## Direct commits"  — commits pushed straight to the branch (no PR), which
#                            GitHub's auto-generated notes omit since they only
#                            list PRs.
# The result is meant to be passed to softprops/action-gh-release as `body_path`;
# GitHub's auto-generated "What's Changed" notes are appended below it.
#
# Usage:
#   scripts/coderabbit-release-notes.sh <current_tag> [previous_tag]
#
# Output goes to stdout. Diagnostics go to stderr. If there is no previous tag
# (first release) or nothing to report, nothing is printed and GitHub's
# auto-generated notes stand alone.
#
# Requires: gh (authenticated), git, python3.
# Honors GITHUB_REPOSITORY (or falls back to `gh repo view`).

set -euo pipefail

CURRENT_TAG="${1:?usage: coderabbit-release-notes.sh <current_tag> [previous_tag]}"
PREV_TAG="${2:-}"

REPO="${GITHUB_REPOSITORY:-}"
if [ -z "$REPO" ]; then
    REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
fi

# Resolve the previous tag (highest version tag that isn't the current one).
if [ -z "$PREV_TAG" ]; then
    PREV_TAG="$(git tag --sort=-v:refname | grep -vFx "$CURRENT_TAG" | head -n1 || true)"
fi

if [ -z "$PREV_TAG" ]; then
    echo "No previous tag found; relying on GitHub auto-generated notes." >&2
    exit 0
fi

echo "Collecting changes in ${PREV_TAG}..${CURRENT_TAG} for ${REPO}" >&2

# Walk every commit in the range and split it into two buckets:
#   - PR-backed commits  -> collect the originating (merged) PR number once
#   - direct commits      -> pushed straight to the branch, no PR
# Merge commits are skipped: their content is already covered by the individual
# commits they bring in.
declare -A seen_pr=()
pr_order=()
orphan_commits=()

while read -r sha; do
    [ -z "$sha" ] && continue
    prs="$(gh api "repos/${REPO}/commits/${sha}/pulls" \
        --jq '.[] | select(.merged_at != null) | .number' 2>/dev/null || true)"
    if [ -n "$prs" ]; then
        while read -r n; do
            [ -z "$n" ] && continue
            if [ -z "${seen_pr[$n]:-}" ]; then
                seen_pr[$n]=1
                pr_order+=("$n")
            fi
        done <<< "$prs"
    elif [ "$(git show -s --format=%P "$sha" | wc -w)" -lt 2 ]; then
        # No merged PR and not a merge commit -> a direct commit.
        orphan_commits+=("$sha")
    fi
done < <(git rev-list "${PREV_TAG}..${CURRENT_TAG}")

# Takes a PR's JSON (number,title,url,body) in $1 and prints a Markdown section
# if the PR description contains a CodeRabbit summary; otherwise prints nothing.
# The JSON is passed via an env var so the heredoc keeps stdin for the program.
render_section() {
    PR_JSON="$1" python3 - <<'PY'
import json, os, re

pr = json.loads(os.environ["PR_JSON"])
body = pr.get("body") or ""

# Prefer the markers CodeRabbit wraps the release-notes block in.
m = re.search(
    r"<!--\s*This is an auto-generated comment: release notes by coderabbit\.ai\s*-->"
    r"(.*?)"
    r"<!--\s*end of auto-generated comment: release notes by coderabbit\.ai\s*-->",
    body, re.S | re.I)
block = m.group(1) if m else body

# Keep only the "Summary by CodeRabbit" section if it's present.
m2 = re.search(r"#{2,}\s*Summary by CodeRabbit\s*\n(.*)", block, re.S | re.I)
if m2:
    block = m2.group(1)
elif not m:
    # No CodeRabbit content at all.
    raise SystemExit(0)

block = re.sub(r"<!--.*?-->", "", block, flags=re.S).strip()
if not block:
    raise SystemExit(0)

print(f"### {pr['title']} ([#{pr['number']}]({pr['url']}))\n")
print(block + "\n")
PY
}

# Rich CodeRabbit summaries for PR-backed changes. PRs without a summary are
# left to GitHub's auto-generated "What's Changed" list (appended below).
highlights=""
for n in "${pr_order[@]}"; do
    data="$(gh pr view "$n" --repo "$REPO" --json number,title,url,body 2>/dev/null || true)"
    [ -z "$data" ] && continue
    sec="$(render_section "$data" || true)"
    [ -n "$sec" ] && highlights+="${sec}"$'\n'
done

# Direct commits — the gap GitHub's release notes miss (they only list PRs).
direct=""
for sha in "${orphan_commits[@]}"; do
    subject="$(git show -s --format=%s "$sha")"
    short="$(git rev-parse --short "$sha")"
    direct+="- ${subject} ([\`${short}\`](https://github.com/${REPO}/commit/${sha}))"$'\n'
done

if [ -n "$highlights" ]; then
    printf '## Highlights\n\n%s\n' "$highlights"
fi
if [ -n "$direct" ]; then
    printf '## Direct commits\n\n%s\n' "$direct"
fi
if [ -z "$highlights" ] && [ -z "$direct" ]; then
    echo "No CodeRabbit summaries or direct commits found in range." >&2
fi

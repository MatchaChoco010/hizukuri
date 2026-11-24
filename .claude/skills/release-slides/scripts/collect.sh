#!/usr/bin/env bash
# Lists what a release contains: the pull requests that were merged in a
# range of commits, the issues they close, and the commits that went in
# without a pull request.
#
#   collect.sh BASE HEAD OUT_DIR
#
# BASE and HEAD are revisions (tags, branches, hashes); the range is
# BASE..HEAD. Writes OUT_DIR/inventory.md, an overview to start reading
# from, and one file per pull request and issue with its description and
# discussion:
#
#   OUT_DIR/inventory.md
#   OUT_DIR/pr-<number>.md      description, commits, changed files, comments
#   OUT_DIR/issue-<number>.md   description and comments
#
# Reads the repository with `gh` (it must be logged in) and changes nothing.
set -euo pipefail

base="${1:?usage: collect.sh BASE HEAD OUT_DIR}"
head="${2:?usage: collect.sh BASE HEAD OUT_DIR}"
out="${3:?usage: collect.sh BASE HEAD OUT_DIR}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
cd "$(git rev-parse --show-toplevel)"

git rev-parse --verify --quiet "$base^{commit}" >/dev/null || { echo "collect.sh: no such revision: $base" >&2; exit 1; }
git rev-parse --verify --quiet "$head^{commit}" >/dev/null || { echo "collect.sh: no such revision: $head" >&2; exit 1; }
repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"

# Every commit of the range that is not a merge, oldest first. A pull
# request that was merged with a merge commit contributes its own commits;
# one that was squashed contributes the squashed commit. Both are found
# through the commit.
mapfile -t commits < <(git rev-list --reverse --no-merges "$base..$head")

declare -A pr_of=()      # commit -> pull request number ("" when none)
declare -A commits_of=() # pull request number -> its commits, in order
prs=()
loose=()
for commit in "${commits[@]}"; do
    number="$(gh api "repos/$repo/commits/$commit/pulls" \
        --jq '[.[] | select(.merged_at != null)] | sort_by(.merged_at) | .[0].number // empty' 2>/dev/null || true)"
    pr_of[$commit]="$number"
    if [[ -n "$number" ]]; then
        [[ -n "${commits_of[$number]:-}" ]] || prs+=("$number")
        commits_of[$number]="${commits_of[$number]:-}$commit "
    else
        loose+=("$commit")
    fi
done

issues=()
{
    echo "# $base..$head"
    echo
    echo "- repository: https://github.com/$repo"
    echo "- base: \`$base\` ($(git rev-parse --short "$base^{commit}"))"
    echo "- head: \`$head\` ($(git rev-parse --short "$head^{commit}"), $(git log -1 --format=%cI "$head^{commit}"))"
    echo "- commits: ${#commits[@]} (${#loose[@]} without a pull request)"
    echo "- pull requests: ${#prs[@]}"
    echo
    echo "## Pull requests, in the order they were merged"
    echo
    if ((${#prs[@]} == 0)); then
        echo "None."
    fi
    for number in "${prs[@]}"; do
        gh pr view "$number" --json number,title,url,mergedAt,baseRefName,headRefName,additions,deletions,changedFiles,closingIssuesReferences \
            --jq '"- #\(.number) \(.title)\n  - \(.url)\n  - merged \(.mergedAt), \(.headRefName) -> \(.baseRefName), \(.changedFiles) files, +\(.additions) -\(.deletions)\n  - closes: \([.closingIssuesReferences[] | "#\(.number)"] | join(", ") | if . == "" then "nothing" else . end)\n  - details: pr-\(.number).md"'
        while read -r issue; do
            [[ -n "$issue" ]] && issues+=("$issue")
        done < <(gh pr view "$number" --json closingIssuesReferences --jq '.closingIssuesReferences[].number')
    done
    echo
    echo "## Commits without a pull request"
    echo
    if ((${#loose[@]} == 0)); then
        echo "None."
    fi
    for commit in "${loose[@]}"; do
        git log -1 --format='- %h %s (%cs)' "$commit"
        git log -1 --format='%b' "$commit" | sed '/^$/d; s/^/      /'
        git show --stat --format= "$commit" | tail -n 1 | sed 's/^/     /'
    done
    echo
    echo "## Issues closed by these pull requests"
    echo
    if ((${#issues[@]} == 0)); then
        echo "None."
    fi
    for issue in "${issues[@]}"; do
        gh issue view "$issue" --json number,title,url,state \
            --jq '"- #\(.number) \(.title) (\(.state))\n  - \(.url)\n  - details: issue-\(.number).md"'
    done
} >"$out/inventory.md"

for number in "${prs[@]}"; do
    {
        gh pr view "$number" --json number,title,url,author,mergedAt,body \
            --jq '"# PR #\(.number): \(.title)\n\n\(.url)\nmerged \(.mergedAt)\n\n## Description\n\n\(.body)"'
        echo
        echo "## Commits"
        echo
        for commit in ${commits_of[$number]}; do
            git log -1 --format='- %h %s%n%n%b' "$commit"
        done
        echo "## Changed files"
        echo
        gh pr view "$number" --json files --jq '.files[] | "- \(.path) (+\(.additions) -\(.deletions))"'
        echo
        echo "## Discussion"
        echo
        gh pr view "$number" --json comments,reviews \
            --jq '(.comments + .reviews)[] | select(.body != "") | "### \(.author.login)\n\n\(.body)\n"'
    } >"$out/pr-$number.md"
done

for issue in "${issues[@]}"; do
    gh issue view "$issue" --json number,title,url,body,comments \
        --jq '"# Issue #\(.number): \(.title)\n\n\(.url)\n\n## Description\n\n\(.body)\n\n## Discussion\n\n\([.comments[] | "### \(.author.login)\n\n\(.body)\n"] | join("\n"))"' \
        >"$out/issue-$issue.md"
done

echo "wrote $out/inventory.md (${#prs[@]} pull requests, ${#loose[@]} commits without one, ${#issues[@]} issues)"

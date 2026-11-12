#!/usr/bin/env bash
# Decides whether a workflow run has to run its checks, or may skip them
# because only documentation changed. ci.yml and vm.yml both call it, in a
# checkout with the full history (fetch-depth: 0).
#
# usage: tools/ci-changes.sh
#
# Environment (set by the workflow from the event):
#   EVENT_NAME    github.event_name
#   REF           github.ref
#   PR_BASE       github.event.pull_request.base.sha (pull_request only)
#   PUSH_BEFORE   github.event.before (push only)
#   PUSH_CREATED  github.event.created (push only): "true" for a new branch
#
# Writes code=true or code=false to $GITHUB_OUTPUT (to stdout when it is not
# set). Every check runs (code=true) for:
#   - a push to a release candidate branch (refs/heads/release-candidate/...),
#     whatever it changed: a candidate has to pass every check;
#   - a push that created the branch;
#   - a push or pull request whose changes cannot be listed (a forced push);
#   - any other event (schedule, workflow_dispatch).
# Otherwise the checks are skipped (code=false) when every changed file is
# documentation (docs/**, *.md). A skip is written to the job summary.
set -euo pipefail

output="${GITHUB_OUTPUT:-/dev/stdout}"

run_all() {
    echo "$1; running all checks"
    echo "code=true" >> "$output"
    exit 0
}

case "${EVENT_NAME:-}" in
    pull_request)
        range="${PR_BASE:-}...HEAD"
        ;;
    push)
        case "${REF:-}" in
            refs/heads/release-candidate/*) run_all "release candidate branch ${REF#refs/heads/}" ;;
        esac
        if [ "${PUSH_CREATED:-}" = true ]; then
            run_all "this push created ${REF:-the branch}"
        fi
        range="${PUSH_BEFORE:-}..HEAD"
        ;;
    *)
        run_all "event ${EVENT_NAME:-(none)}"
        ;;
esac

if ! files="$(git diff --name-only "$range" --)"; then
    run_all "cannot list the changes of $range"
fi

code=false
while IFS= read -r file; do
    case "$file" in
        "" | docs/* | *.md) ;;
        *)
            code=true
            echo "not documentation: $file"
            ;;
    esac
done <<< "$files"
echo "code=$code" >> "$output"

if [ "$code" = false ]; then
    echo "only documentation changed in $range; the checks are skipped"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        {
            echo "### Checks skipped"
            echo
            echo "Only documentation (\`docs/**\`, \`*.md\`) changed in \`$range\`, so the checks of this workflow were skipped."
            echo
            echo '```'
            if [ -n "$files" ]; then
                echo "$files"
            else
                echo "(no files changed)"
            fi
            echo '```'
        } >> "$GITHUB_STEP_SUMMARY"
    fi
fi

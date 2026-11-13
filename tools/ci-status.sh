#!/usr/bin/env bash
# Reports whether the check workflows passed on one commit of a branch: the
# latest run of each workflow that a push of that commit to that branch
# started. Only reads, through the GitHub API (`gh`, logged in).
#
# usage: tools/ci-status.sh [options] <branch>
#
#   --commit SHA         the commit to look at. Default: the head of the
#                        branch on GitHub.
#   --workflows "A B"    the workflow files, by file name. Default:
#                        "ci.yml vm.yml gpu.yml".
#   --wait               wait until every run has finished.
#   --interval SECONDS   time between two looks while waiting. Default: 30.
#   --start-timeout SECONDS
#                        while waiting, how long a workflow may have no run
#                        before it is reported as not started. Default: 300.
#   --timeout SECONDS    give up waiting after this long. Default: 14400.
#
# A workflow passed when its latest run finished with success and every job
# of that run (its latest attempt) succeeded. Anything else is not a pass: a
# workflow file the repository does not have, a workflow that no push of the
# commit to the branch started, a run that has not finished, and a run that
# finished with success but skipped a job.
#
# Exit status: 0 when every workflow passed, 1 when one did not, 2 for a
# usage error or a failed request.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() {
    sed -n '/^# usage:/,/^set -euo/p' "$0" | sed -e '$d' -e 's/^# \{0,1\}//' >&2
    exit 2
}

commit=""
workflows="ci.yml vm.yml gpu.yml"
wait=false
interval=30
start_timeout=300
timeout=14400
branch=""
while (($#)); do
    case "$1" in
        --commit) commit="${2:?}"; shift 2 ;;
        --workflows) workflows="${2:?}"; shift 2 ;;
        --wait) wait=true; shift ;;
        --interval) interval="${2:?}"; shift 2 ;;
        --start-timeout) start_timeout="${2:?}"; shift 2 ;;
        --timeout) timeout="${2:?}"; shift 2 ;;
        -h | --help) usage ;;
        -*) echo "unknown option: $1" >&2; usage ;;
        *)
            [ -z "$branch" ] || usage
            branch="$1"
            shift
            ;;
    esac
done
[ -n "$branch" ] || usage
for number in "$interval" "$start_timeout" "$timeout"; do
    [[ "$number" =~ ^[0-9]+$ ]] || { echo "not a number of seconds: $number" >&2; exit 2; }
done

if [ -z "$commit" ]; then
    commit="$(gh api "repos/{owner}/{repo}/git/ref/heads/$branch" --jq .object.sha)" || {
        echo "cannot find the branch $branch on GitHub" >&2
        exit 2
    }
fi

# Sets STATE, DETAIL and URL for one workflow file.
#   passed       the run and all its jobs succeeded
#   failed       the run finished with another conclusion
#   incomplete   the run succeeded, but not every job ran and succeeded
#   running      the run has not finished
#   not-started  no push of the commit to the branch started the workflow
#   missing      the repository has no such workflow
look() {
    local file="$1" runs error
    STATE="" DETAIL="" URL=""
    if ! runs="$(gh api -X GET "repos/{owner}/{repo}/actions/workflows/$file/runs" \
        -f branch="$branch" -f head_sha="$commit" -f event=push -f per_page=100 \
        --jq '.workflow_runs | max_by(.id) // empty
              | [.id, .status, (.conclusion // "none"), .html_url] | @tsv' 2> "$errors")"; then
        error="$(cat "$errors")"
        if [[ "$error" == *"HTTP 404"* ]]; then
            STATE=missing
            DETAIL="the repository has no workflow $file"
            return
        fi
        echo "$error" >&2
        echo "cannot list the runs of $file" >&2
        exit 2
    fi
    if [ -z "$runs" ]; then
        STATE=not-started
        DETAIL="no push of the commit to $branch started it"
        return
    fi
    local id status conclusion
    IFS=$'\t' read -r id status conclusion URL <<< "$runs"
    if [ "$status" != completed ]; then
        STATE=running
        DETAIL="$status"
        return
    fi
    if [ "$conclusion" != success ]; then
        STATE=failed
        DETAIL="$conclusion"
        return
    fi
    local jobs
    jobs="$(gh api -X GET "repos/{owner}/{repo}/actions/runs/$id/jobs" \
        -f filter=latest -f per_page=100 \
        --jq '.jobs[] | [(.conclusion // .status), .name] | @tsv')" || {
        echo "cannot list the jobs of run $id" >&2
        exit 2
    }
    if [ -z "$jobs" ]; then
        STATE=incomplete
        DETAIL="the run has no jobs"
        return
    fi
    local others
    others="$(awk -F'\t' '$1 != "success" { printf "%s%s: %s", sep, $2, $1; sep = ", " }' <<< "$jobs")"
    if [ -n "$others" ]; then
        STATE=incomplete
        DETAIL="$others"
        return
    fi
    STATE=passed
    DETAIL="every job succeeded ($(wc -l <<< "$jobs"))"
}

errors="$(mktemp)"
trap 'rm -f "$errors"' EXIT

declare -A state detail url
look_all() {
    local file
    for file in $workflows; do
        look "$file"
        state[$file]="$STATE"
        detail[$file]="$DETAIL"
        url[$file]="$URL"
    done
}

start="$SECONDS"
look_all
if "$wait"; then
    while :; do
        pending=""
        for file in $workflows; do
            case "${state[$file]}" in
                running) pending+=" $file" ;;
                not-started | missing)
                    if ((SECONDS - start < start_timeout)); then
                        pending+=" $file"
                    fi
                    ;;
            esac
        done
        [ -n "$pending" ] || break
        if ((SECONDS - start >= timeout)); then
            echo "gave up waiting after ${timeout}s for:$pending" >&2
            break
        fi
        echo "waiting for:$pending" >&2
        sleep "$interval"
        look_all
    done
fi

echo "branch: $branch"
echo "commit: $commit"
status=0
for file in $workflows; do
    printf '%-16s %-12s %s\n' "$file" "${state[$file]}" "${detail[$file]}"
    [ -z "${url[$file]}" ] || printf '%-16s %-12s %s\n' "" "" "${url[$file]}"
    [ "${state[$file]}" = passed ] || status=1
done
if ((status == 0)); then
    echo "all workflows passed"
else
    echo "not every workflow passed"
fi
exit "$status"

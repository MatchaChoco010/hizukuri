#!/usr/bin/env bash
# Runs a command and watches it, so that a command that gets stuck, or a
# process of it that crashes, leaves something to go on. tools/ci.sh runs the
# tests through this.
#
# usage: tools/ci-watch.sh COMMAND [ARGUMENT...]
#        tools/ci-watch.sh --self-test
#
# The command runs in the foreground. Its standard output and standard error
# are passed on, as one stream. The status of the command is the status of
# this script, except when the command got stuck: then it is 124.
#
# Stuck: the command wrote nothing for HIZUKURI_TEST_STALL_SECONDS. Then,
# for the command and every process below it (found by following the parent
# of each process, never by name), this script prints
#
#   - the memory, load and pressure of the machine (/proc/meminfo,
#     /proc/loadavg, /proc/pressure),
#   - the memory and the number of threads of each process,
#   - the state of each thread, and where it waits in the kernel
#     (/proc/<pid>/task/<tid>/stat, wchan, syscall, and stack where it can be
#     read),
#   - the backtrace of every thread, taken with gdb,
#
# and stops those processes, by their ids.
#
# Crashed: a process dumped core into HIZUKURI_TEST_CORE_DIR while the
# command ran. This script prints the backtrace of every thread from the
# core, and removes the core. It does not change the status.
#
# Nothing here changes a setting of the system. Two things depend on
# settings this script only reads:
#
#   - gdb can attach only where the kernel lets a process trace one that is
#     not its child (kernel.yama.ptrace_scope is 0, or this runs as root).
#     Elsewhere the backtraces of a stuck command are left out, and the
#     output says so.
#   - Cores are found only where the kernel writes them as files into
#     HIZUKURI_TEST_CORE_DIR, named core.<pid>.<executable> with the
#     kernel's %p and %E (kernel.core_pattern = <directory>/core.%p.%E).
#
# Environment:
#   HIZUKURI_TEST_STALL_SECONDS  seconds without output after which the
#                             command is taken to be stuck. Default: 300.
#                             0 turns the watch off.
#   HIZUKURI_TEST_CORE_DIR    the directory the kernel writes cores into.
#                             When set, the command runs with core dumps
#                             enabled. Not set: core dumps are left as they
#                             are, and crashes are not looked at.
#   HIZUKURI_TEST_INSPECT_AS_ROOT  set to 1 to read the kernel stacks of the
#                             threads of a stuck command with `sudo -n`.
#                             Without it, sudo is never used.
#
# --self-test runs a command that writes nothing (and, with
# HIZUKURI_TEST_CORE_DIR, one that crashes) through this script, and fails
# unless the report has the backtraces. It shows whether the machine it runs
# on allows what the reports need.
set -uo pipefail

STALL_STATUS=124
# How long gdb may take for one process, and /proc for one process.
GDB_SECONDS=120
PROC_SECONDS=30

limit="${HIZUKURI_TEST_STALL_SECONDS:-300}"
core_dir="${HIZUKURI_TEST_CORE_DIR:-}"
as_root="${HIZUKURI_TEST_INSPECT_AS_ROOT:-}"

note() {
    printf '%s\n' "$*" >&2
}

# Reads a stat file of /proc ($1) into COMM and FIELDS. FIELDS starts at the
# third field of the file: [0] state, [1] parent, [11] and [12] user and
# system time in ticks, [19] start time.
read_stat() {
    local line
    { IFS= read -r line < "$1"; } 2> /dev/null || return 1
    COMM="${line#*(}"
    COMM="${COMM%)*}"
    read -r -a FIELDS <<< "${line##*) }"
}

# Fills TREE with the id of process $1 and of every process below it,
# parents first, and STARTED with the start time of each. The start time
# tells a process from a later one that got the same id.
process_tree() {
    local -A parent_of=() started_at=()
    local stat pid index=0
    for stat in /proc/[0-9]*/stat; do
        read_stat "$stat" || continue
        pid="${stat#/proc/}"
        pid="${pid%/stat}"
        parent_of[$pid]="${FIELDS[1]}"
        started_at[$pid]="${FIELDS[19]}"
    done
    TREE=()
    STARTED=()
    [[ -n "${parent_of[$1]:-}" ]] || return 0
    TREE=("$1")
    while ((index < ${#TREE[@]})); do
        for pid in "${!parent_of[@]}"; do
            if [[ "${parent_of[$pid]}" == "${TREE[index]}" ]]; then
                TREE+=("$pid")
            fi
        done
        index=$((index + 1))
    done
    for pid in "${TREE[@]}"; do
        STARTED[$pid]="${started_at[$pid]}"
    done
}

# Whether process $1 is still the one process_tree saw.
same_process() {
    read_stat "/proc/$1/stat" && [[ "${FIELDS[19]}" == "${STARTED[$1]:-}" ]]
}

# The name of system call $1, for the few a waiting thread is usually in.
syscall_name() {
    [[ "$HOSTTYPE" == x86_64 ]] || return 0
    case "$1" in
        0) echo read ;;
        1) echo write ;;
        7) echo poll ;;
        16) echo ioctl ;;
        23) echo select ;;
        35) echo nanosleep ;;
        45) echo recvfrom ;;
        47) echo recvmsg ;;
        61) echo wait4 ;;
        202) echo futex ;;
        230) echo clock_nanosleep ;;
        232) echo epoll_wait ;;
        247) echo waitid ;;
        270) echo pselect6 ;;
        271) echo ppoll ;;
        281) echo epoll_pwait ;;
    esac
}

# Runs a function in a subshell for at most $1 seconds. Reading /proc of a
# process that is stuck inside the kernel can get stuck as well.
bounded() {
    local seconds="$1" waited=0 child
    shift
    "$@" &
    child=$!
    while kill -0 "$child" 2> /dev/null; do
        if ((waited >= seconds * 5)); then
            kill -KILL "$child" 2> /dev/null
            note "   (gave up after $seconds seconds)"
            break
        fi
        sleep 0.2
        waited=$((waited + 1))
    done
    wait "$child" 2> /dev/null
    return 0
}

# Prints the command line of process $1, its state, and for each thread the
# state and where it waits in the kernel.
describe_process() {
    local pid="$1" task tid wchan syscall number name target line
    if ! read_stat "/proc/$pid/stat"; then
        note "-- process $pid: gone"
        return
    fi
    note "-- process $pid (parent ${FIELDS[1]}, state ${FIELDS[0]}), executable $(timeout 5 readlink "/proc/$pid/exe" 2> /dev/null || echo "?")"
    note "   command line: $(timeout 5 tr '\0' ' ' < "/proc/$pid/cmdline" 2> /dev/null || echo "?")"
    while IFS= read -r line; do
        case "$line" in
            VmSize:* | VmRSS:* | VmSwap:* | Threads:*) note "   ${line//$'\t'/ }" ;;
        esac
    done <<< "$(timeout 5 cat "/proc/$pid/status" 2> /dev/null)"
    for task in "/proc/$pid/task"/[0-9]*; do
        tid="${task##*/}"
        read_stat "$task/stat" || continue
        wchan=""
        { IFS= read -r wchan < "$task/wchan"; } 2> /dev/null || true
        [[ -n "$wchan" && "$wchan" != 0 ]] || wchan="nothing"
        syscall=""
        { IFS= read -r syscall < "$task/syscall"; } 2> /dev/null || true
        number="${syscall%% *}"
        name=""
        target=""
        if [[ "$number" =~ ^[0-9]+$ ]]; then
            name="$(syscall_name "$number")"
            case "$name" in
                read | write | ioctl | recvfrom | recvmsg | epoll_wait | epoll_pwait)
                    line="${syscall#* }"
                    line="${line%% *}"
                    target="$(timeout 5 readlink "/proc/$pid/fd/$((line))" 2> /dev/null || true)"
                    ;;
            esac
        fi
        note "   thread $tid ($COMM): state ${FIELDS[0]}, cpu time $((FIELDS[11] + FIELDS[12])) ticks, waits in ${wchan}, system call ${syscall:-?}${name:+ ($name${target:+ on $target})}"
        if [[ "$as_root" == 1 ]]; then
            timeout 5 sudo -n cat "$task/stack" 2> /dev/null | sed 's/^/      /' >&2
        elif [[ -r "$task/stack" ]]; then
            timeout 5 cat "$task/stack" 2> /dev/null | sed 's/^/      /' >&2
        fi
    done
}

# Prints what the whole machine was doing: memory, load, and pressure. A
# machine that ran out of memory stalls every process, not only one.
describe_machine() {
    local line file
    note "== the machine"
    while IFS= read -r line; do
        case "$line" in
            MemTotal:* | MemAvailable:* | SwapTotal:* | SwapFree:* | Committed_AS:*)
                note "   $line"
                ;;
        esac
    done < /proc/meminfo
    { IFS= read -r line < /proc/loadavg; } 2> /dev/null && note "   load average: $line"
    for file in /proc/pressure/cpu /proc/pressure/memory /proc/pressure/io; do
        [[ -r "$file" ]] || continue
        while IFS= read -r line; do
            note "   pressure ${file##*/}: $line"
        done < "$file"
    done
}

# Why gdb cannot attach to the processes of the command, or nothing.
no_attach_reason() {
    local scope=0
    if ! command -v gdb > /dev/null; then
        echo "gdb is not on PATH"
        return
    fi
    { read -r scope < /proc/sys/kernel/yama/ptrace_scope; } 2> /dev/null || scope=0
    if [[ "$scope" != 0 && "$EUID" != 0 ]]; then
        echo "kernel.yama.ptrace_scope is $scope, so a debugger may only attach to its own children"
    fi
}

gdb_batch() {
    timeout --signal=KILL "$GDB_SECONDS" gdb -q -batch -nx \
        -iex "set auto-load off" -iex "set debuginfod enabled off" \
        -iex "set pagination off" -iex "set print thread-events off" \
        "$@" -ex "thread apply all backtrace 100" < /dev/null 2>&1 | sed 's/^/   /' >&2
    if [[ "${PIPESTATUS[0]}" == 137 ]]; then
        note "   (gdb did not finish in $GDB_SECONDS seconds)"
    fi
}

# Stops the processes of TREE: asks first, the command itself before what it
# started, so that it starts nothing more; then kills what is left.
stop_tree() {
    local pid waited=0 left
    for pid in "${TREE[@]}"; do
        if same_process "$pid"; then
            kill -TERM "$pid" 2> /dev/null
        fi
    done
    while ((waited < 25)); do
        left=0
        for pid in "${TREE[@]}"; do
            if same_process "$pid" && [[ "${FIELDS[0]}" != Z ]]; then
                left=1
            fi
        done
        ((left)) || return 0
        sleep 0.2
        waited=$((waited + 1))
    done
    for pid in "${TREE[@]}"; do
        if same_process "$pid"; then
            kill -KILL "$pid" 2> /dev/null
        fi
    done
}

report_stall() {
    local idle="$1" root="" pid reason
    { read -r root < "$work/pid"; } 2> /dev/null || true
    note ""
    note "== tools/ci-watch.sh: no output for $idle seconds; the command is taken to be stuck"
    if [[ "${GITHUB_ACTIONS:-}" == true ]]; then
        echo "::error title=Stuck::The command wrote nothing for $idle seconds. The log has the state and the backtrace of every thread."
    fi
    if [[ -z "$root" ]]; then
        note "the command had not started"
        return
    fi
    bounded "$PROC_SECONDS" describe_machine
    process_tree "$root"
    note "== the processes of the command, parents first: ${TREE[*]}"
    for pid in "${TREE[@]}"; do
        bounded "$PROC_SECONDS" describe_process "$pid"
    done
    reason="$(no_attach_reason)"
    if [[ -n "$reason" ]]; then
        note "== no backtraces: $reason"
    else
        for pid in "${TREE[@]}"; do
            if same_process "$pid"; then
                note "-- backtrace of process $pid"
                gdb_batch -p "$pid"
            fi
        done
    fi
    note "== stopping the processes of the command"
    stop_tree
}

# Wakes every $poll seconds, until a line arrives on the stop pipe. Time is
# counted in wake-ups, so that a suspended machine does not look like a
# stuck command.
watchdog() {
    local size last_size="" idle=0
    while :; do
        if read -r -t "$poll" -u "$stop" _; then
            return 0
        fi
        size="$(stat -c %s "$work/output" 2> /dev/null)" || size=""
        if [[ "$size" != "$last_size" ]]; then
            last_size="$size"
            idle=0
            continue
        fi
        idle=$((idle + poll))
        if ((idle >= limit)); then
            : > "$work/stalled"
            report_stall "$idle"
            return 0
        fi
    done
}

# Prints the backtraces from the cores that appeared in the core directory
# while the command ran, and removes those cores.
report_cores() {
    local core rest pid exe
    [[ -n "$core_dir" ]] || return 0
    for core in "$core_dir"/core.*; do
        [[ -f "$core" && "$core" -nt "$work/started" ]] || continue
        rest="${core##*/core.}"
        pid="${rest%%.*}"
        exe="${rest#*.}"
        exe="${exe//\!//}"
        [[ -x "$exe" ]] || exe=""
        note ""
        note "== tools/ci-watch.sh: process $pid${exe:+ ($exe)} crashed and dumped core"
        if [[ "${GITHUB_ACTIONS:-}" == true ]]; then
            echo "::error title=Crashed::Process $pid${exe:+ ($exe)} crashed. The log has the backtrace of every thread."
        fi
        if command -v gdb > /dev/null; then
            gdb_batch ${exe:+"$exe"} -c "$core"
        else
            note "no backtrace: gdb is not on PATH"
        fi
        rm -f "$core"
    done
}

cleanup() {
    local pid
    for pid in "${watchdog_pid:-}" "${tee_pid:-}"; do
        if [[ -n "$pid" ]]; then
            kill "$pid" 2> /dev/null
        fi
    done
    rm -rf "$work"
}

run() {
    local status waited=0

    work="$(mktemp -d "${TMPDIR:-/tmp}/ci-watch-XXXXXX")"
    trap cleanup EXIT
    : > "$work/started"
    : > "$work/output"
    mkfifo "$work/pipe" "$work/stop"
    # Read-write, so that neither end waits for the other.
    exec {stop}<> "$work/stop"

    # Passes the output on, and keeps a copy whose size the watchdog reads.
    tee "$work/output" < "$work/pipe" &
    tee_pid=$!
    if ((limit > 0)); then
        watchdog &
        watchdog_pid=$!
    fi

    # In the foreground, as without this script: an interrupt from the
    # terminal reaches the command. The braces keep the shell's own line
    # about a command that a signal ended ("Terminated") out of the output.
    {
        (
            echo "$BASHPID" > "$work/pid"
            exec {stop}>&-
            if [[ -n "$core_dir" ]]; then
                ulimit -S -c unlimited 2> /dev/null || ulimit -S -c "$(ulimit -H -c)" 2> /dev/null
            fi
            exec "$@"
        ) > "$work/pipe" 2>&1
        status=$?
    } 2> /dev/null

    if [[ -n "${watchdog_pid:-}" ]]; then
        echo >&"$stop"
        wait "$watchdog_pid" 2> /dev/null
        watchdog_pid=""
    fi
    if [[ -e "$work/stalled" ]]; then
        status="$STALL_STATUS"
    fi

    # tee ends when the last process that holds the output of the command
    # has closed it. A process the command left running may never do so.
    while kill -0 "$tee_pid" 2> /dev/null; do
        if ((waited >= 25)); then
            kill "$tee_pid" 2> /dev/null
            note "== tools/ci-watch.sh: a process the command started still holds its output open; not waiting for it"
            break
        fi
        sleep 0.2
        waited=$((waited + 1))
    done
    wait "$tee_pid" 2> /dev/null
    tee_pid=""

    report_cores
    return "$status"
}

# Fails unless $1 (what is checked) holds: $2 is the text, $3 a pattern.
expect() {
    if grep -Eq -- "$3" <<< "$2"; then
        echo "ok: $1"
    else
        echo "FAILED: $1" >&2
        self_test_failed=1
    fi
}

self_test() {
    local self="${BASH_SOURCE[0]}" output status
    self_test_failed=0

    echo "== self test: a command that writes nothing"
    # GITHUB_ACTIONS is emptied: the reports of the commands below are what
    # is being checked, not errors of the job.
    output="$(GITHUB_ACTIONS="" HIZUKURI_TEST_STALL_SECONDS=2 HIZUKURI_TEST_CORE_DIR="" "$self" sleep 600 2>&1)"
    status=$?
    printf '%s\n' "$output"
    expect "the status is $STALL_STATUS (it was $status)" "$status" "^$STALL_STATUS\$"
    expect "the report names the stuck process" "$output" '^ +command line: sleep 600'
    expect "the report has where a thread waits in the kernel" "$output" 'thread [0-9]+ \(sleep\): state S'
    expect "the report has the memory of the machine" "$output" '^ +MemAvailable: '
    expect "the report has the memory of the process" "$output" '^ +VmRSS: '
    expect "the report has a backtrace" "$output" '^ +#0 '

    if [[ -n "$core_dir" ]]; then
        echo "== self test: a command that crashes"
        output="$(GITHUB_ACTIONS="" HIZUKURI_TEST_STALL_SECONDS=60 "$self" bash -c 'sleep 600 & kill -SEGV $!; wait $!' 2>&1)"
        status=$?
        printf '%s\n' "$output"
        expect "the status is that of the command, 139 (it was $status)" "$status" '^139$'
        expect "the report names the crashed process" "$output" 'crashed and dumped core'
        expect "the report has a backtrace" "$output" '^ +#0 '
        expect "the core is removed" "$(ls "$core_dir")" '^$'
    else
        echo "== self test: HIZUKURI_TEST_CORE_DIR is not set, so crashes are not looked at"
    fi

    if ((self_test_failed)); then
        echo "the self test failed (see above)" >&2
        exit 1
    fi
    echo "== the self test passed"
}

if ! [[ "$limit" =~ ^[0-9]+$ ]]; then
    echo "HIZUKURI_TEST_STALL_SECONDS is not a number of seconds: $limit" >&2
    exit 2
fi
if [[ -n "$core_dir" && ! -d "$core_dir" ]]; then
    echo "HIZUKURI_TEST_CORE_DIR is not a directory: $core_dir" >&2
    exit 2
fi
# How often the watchdog looks.
poll=5
if ((limit > 0 && limit < 30)); then
    poll=1
fi

case "${1:-}" in
    "")
        echo "usage: tools/ci-watch.sh COMMAND [ARGUMENT...] | --self-test" >&2
        exit 2
        ;;
    --self-test) self_test ;;
    *)
        run "$@"
        exit $?
        ;;
esac

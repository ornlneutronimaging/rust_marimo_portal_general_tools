#!/usr/bin/env bash
#
# list_and_fix_running_browser.sh - find (and optionally kill) the current
# user's browser / Jupyter processes across the shared analysis machines.
#
# Why this is needed
# ------------------
# The Firefox profile (~/.mozilla) lives on shared NFS/GPFS storage, so a
# Firefox running on ANY analysis node locks the profile. A new Firefox started
# on a different machine then refuses to launch ("Firefox is already running"),
# or leaves stale ".nfsXXXX" / "Device or resource busy" files behind.
#
# Jupyter is included because `jupyter-lab --browser=firefox` is what usually
# spawns those Firefox sessions; the Jupyter server is the live parent that
# holds files open and keeps the (often zombie) Firefox child around. Killing
# the Jupyter parent both frees the profile and reaps the zombie.
#
# It walks a list of hosts (read from list_and_fix_running_browser.cfg, a JSON
# file next to this script) over SSH in parallel, so it needs passwordless SSH
# to those hosts. Home directories are shared across the analysis machines, so
# the script provisions that itself on first use: it generates a dedicated
# passphrase-less key (~/.ssh/id_ed25519_fixbrowser) and lists it in
# ~/.ssh/authorized_keys, which every host then honours. Without this every
# host shows up as "unreachable" and nothing is killed - which is what users
# who had never set up an SSH key were seeing.
#
# Usage:
#   list_and_fix_running_browser.sh            # (default) list matching procs
#   list_and_fix_running_browser.sh list       # same as above
#   list_and_fix_running_browser.sh kill       # kill them, then verify
#   list_and_fix_running_browser.sh -firefox   # match only Firefox
#   list_and_fix_running_browser.sh -chrome    # match only Chrome / Chromium
#   list_and_fix_running_browser.sh -firefox -chrome  # both (also the default)
#   list_and_fix_running_browser.sh list -v    # also show clean/unreachable hosts
#   list_and_fix_running_browser.sh --help
#
set -uo pipefail

# ---------------------------------------------------------------------------
# Host / browser list (loaded from the .cfg file next to this script)
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG_FILE="${SCRIPT_DIR}/list_and_fix_running_browser.cfg"

if [[ ! -f "$CONFIG_FILE" ]]; then
    echo "$(basename "$0"): config file not found: $CONFIG_FILE" >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "$(basename "$0"): 'jq' is required to read $CONFIG_FILE" >&2
    exit 1
fi
if ! jq -e . "$CONFIG_FILE" >/dev/null 2>&1; then
    echo "$(basename "$0"): invalid JSON in $CONFIG_FILE" >&2
    exit 1
fi

mapfile -t HOSTS < <(jq -r '.hosts[]?' "$CONFIG_FILE")

if [[ ${#HOSTS[@]} -eq 0 ]]; then
    echo "$(basename "$0"): no 'hosts' defined in $CONFIG_FILE" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Passwordless SSH: make sure this user has a key the analysis hosts accept.
#
# ~/.ssh is on the shared home directory, so one key pair whose public half
# is listed in ~/.ssh/authorized_keys works for every host in the list. Users
# who never ran ssh-keygen have no key at all, and with BatchMode=yes every
# host is then reported "unreachable" - so we create a dedicated one here.
# ---------------------------------------------------------------------------
SSH_KEY="$HOME/.ssh/id_ed25519_fixbrowser"

ensure_ssh_key() {
    local sshdir="$HOME/.ssh" auth pubkey
    auth="$sshdir/authorized_keys"
    mkdir -p "$sshdir" 2>/dev/null || return 1
    chmod 700 "$sshdir" 2>/dev/null
    if [[ ! -f "$SSH_KEY" || ! -f "$SSH_KEY.pub" ]]; then
        rm -f "$SSH_KEY" "$SSH_KEY.pub"
        if ! ssh-keygen -q -t ed25519 -N '' \
                -C "$USER@$(hostname -s) list_and_fix_running_browser" \
                -f "$SSH_KEY" </dev/null >/dev/null 2>&1; then
            echo "$(basename "$0"): could not create SSH key $SSH_KEY" >&2
            return 1
        fi
        echo "Created SSH key $SSH_KEY for passwordless access to the analysis machines."
    fi
    chmod 600 "$SSH_KEY" 2>/dev/null
    pubkey="$(awk '{print $2}' "$SSH_KEY.pub")"
    [[ -n "$pubkey" ]] || return 1
    touch "$auth" 2>/dev/null || return 1
    chmod 600 "$auth" 2>/dev/null
    if ! grep -qF -- "$pubkey" "$auth"; then
        cat "$SSH_KEY.pub" >>"$auth"
        echo "Added $SSH_KEY.pub to $auth."
    fi
    return 0
}

if ! ensure_ssh_key; then
    echo "$(basename "$0"): WARNING: passwordless SSH could not be set up;" \
         "hosts will likely show as unreachable." >&2
fi

SSH_OPTS=(
    -o BatchMode=yes
    -o ConnectTimeout=5
    -o StrictHostKeyChecking=accept-new
    -o LogLevel=ERROR
)
[[ -f "$SSH_KEY" ]] && SSH_OPTS+=(-i "$SSH_KEY")

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
mode="list"
verbose=0
want_firefox=0
want_chrome=0
for arg in "$@"; do
    case "$arg" in
        list)        mode="list" ;;
        kill)        mode="kill" ;;
        -firefox)    want_firefox=1 ;;
        -chrome)     want_chrome=1 ;;
        -v|--verbose) verbose=1 ;;
        -h|--help)
            sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "$(basename "$0"): unknown argument '$arg' (try --help)" >&2
            exit 2
            ;;
    esac
done

# Browser matching: -firefox and/or -chrome select which process names to look
# for. With neither flag we default to both (the original behaviour).
if [[ "$want_firefox" -eq 0 && "$want_chrome" -eq 0 ]]; then
    want_firefox=1
    want_chrome=1
fi
BROWSER_RE=""
BROWSER_LABEL=""
if [[ "$want_firefox" -eq 1 ]]; then
    BROWSER_RE+="${BROWSER_RE:+|}firefox.*"
    BROWSER_LABEL+="${BROWSER_LABEL:+/}Firefox"
fi
if [[ "$want_chrome" -eq 1 ]]; then
    BROWSER_RE+="${BROWSER_RE:+|}chrome.*|chromium.*"
    BROWSER_LABEL+="${BROWSER_LABEL:+/}Chrome"
fi

# ---------------------------------------------------------------------------
# Remote payload - runs on each host via `bash -s -- <mode>`.
#
# Single-quoted: every variable here is expanded REMOTELY, so $USER is the
# remote user (same account on shared home dirs).
#
# Process matching:
#   * Firefox/Chromium  -> by process NAME (pgrep -x), so we don't match
#     unrelated commands that merely mention a browser in their arguments.
#   * Jupyter           -> by full command line (pgrep -f). The "[l]" bracket
#     trick keeps this very command from matching itself.
#   * Zombies (<defunct>) are listed for information but never counted as
#     kill targets - they hold no files open and cannot be killed; they go
#     away when their parent (the Jupyter server) is killed.
# ---------------------------------------------------------------------------
read -r -d '' REMOTE_SCRIPT <<'REMOTE_EOF'
mode="${1:-list}"
browser_re="${2:-firefox.*}"
me="$USER"

list_pids() {
    {
        pgrep -u "$me" -x "$browser_re" 2>/dev/null
        pgrep -u "$me" -f 'jupyter-[l]ab|jupyter-[n]otebook' 2>/dev/null
    } | sed '/^$/d' | sort -un
}

# Subset of pids that are alive and not zombies -> the real kill targets.
live_pids() {
    local p st
    for p in $(list_pids); do
        st="$(ps -o stat= -p "$p" 2>/dev/null | tr -d ' ')"
        case "$st" in
            ""|Z*) ;;          # gone or zombie -> skip
            *) printf '%s\n' "$p" ;;
        esac
    done
}

pids="$(list_pids)"
if [ -z "$pids" ]; then
    echo "COUNT 0"
    exit 0
fi

count="$(printf '%s\n' "$pids" | wc -l | tr -d ' ')"
echo "COUNT $count"
# shellcheck disable=SC2086
ps -o pid=,stat=,args= -p $(printf '%s ' $pids) 2>/dev/null

if [ "$mode" = "kill" ]; then
    targets="$(live_pids | tr '\n' ' ')"
    if [ -n "${targets// /}" ]; then
        # shellcheck disable=SC2086
        kill $targets 2>/dev/null || true
        for _ in 1 2 3 4 5; do
            sleep 1
            [ -z "$(live_pids)" ] && break
        done
        leftover="$(live_pids | tr '\n' ' ')"
        if [ -n "${leftover// /}" ]; then
            # shellcheck disable=SC2086
            kill -9 $leftover 2>/dev/null || true
            sleep 1
        fi
    fi
    echo "REMAINING $(live_pids | grep -c .)"
fi
REMOTE_EOF

# ---------------------------------------------------------------------------
# Scan all hosts in parallel
# ---------------------------------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

scan_host() {
    local host="$1" out rc
    # Extra single quotes keep the regex one argument on the remote side:
    # ssh joins its args into a single command line, so an unquoted '|' in
    # $BROWSER_RE would be parsed as a pipeline by the remote shell.
    out="$(ssh "${SSH_OPTS[@]}" "$host" bash -s -- "$mode" "'$BROWSER_RE'" <<<"$REMOTE_SCRIPT" 2>"$tmp/$host.err")"
    rc=$?
    if [ "$rc" -ne 0 ] && [ -z "$out" ]; then
        printf 'UNREACHABLE\n' >"$tmp/$host"
    else
        printf '%s\n' "$out" >"$tmp/$host"
    fi
}

if [ "$mode" = "kill" ]; then
    echo "== Killing this user's ${BROWSER_LABEL} / Jupyter processes on ${#HOSTS[@]} hosts =="
else
    echo "== Listing this user's ${BROWSER_LABEL} / Jupyter processes on ${#HOSTS[@]} hosts =="
fi
echo

for h in "${HOSTS[@]}"; do
    scan_host "$h" &
done
wait

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------
total_procs=0
hosts_with=0
unreachable=0
clean=0
remaining_total=0
auth_failed=0

for h in "${HOSTS[@]}"; do
    content="$(cat "$tmp/$h" 2>/dev/null)"

    if [ -z "$content" ] || [ "$content" = "UNREACHABLE" ]; then
        unreachable=$((unreachable + 1))
        reason="$(head -n 1 "$tmp/$h.err" 2>/dev/null)"
        [ "$verbose" -eq 1 ] && printf '  %-26s unreachable%s\n' "$h" "${reason:+ ($reason)}"
        case "$reason" in
            *"Permission denied"*) auth_failed=$((auth_failed + 1)) ;;
        esac
        continue
    fi

    count="$(printf '%s\n' "$content" | awk 'NR==1 && $1=="COUNT"{print $2}')"
    [ -z "$count" ] && count=0

    if [ "$count" -eq 0 ]; then
        clean=$((clean + 1))
        [ "$verbose" -eq 1 ] && printf '  %-26s clean\n' "$h"
        continue
    fi

    hosts_with=$((hosts_with + 1))
    total_procs=$((total_procs + count))

    printf '[%s] %s process(es):\n' "$h" "$count"
    # detail lines = everything except the COUNT / REMAINING markers
    printf '%s\n' "$content" | grep -vE '^(COUNT|REMAINING) ' | sed 's/^/    /'

    if [ "$mode" = "kill" ]; then
        rem="$(printf '%s\n' "$content" | awk '$1=="REMAINING"{print $2}')"
        [ -z "$rem" ] && rem=0
        remaining_total=$((remaining_total + rem))
        if [ "$rem" -eq 0 ]; then
            printf '    -> killed; none remaining\n'
        else
            printf '    -> WARNING: %s still running after kill\n' "$rem"
        fi
    fi
    echo
done

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "---------------------------------------------------------------"
if [ "$mode" = "kill" ]; then
    printf 'Found %s process(es) on %s host(s); %s still running after kill.\n' \
        "$total_procs" "$hosts_with" "$remaining_total"
else
    printf 'Found %s process(es) on %s host(s).\n' "$total_procs" "$hosts_with"
fi
printf '%s host(s) clean, %s unreachable (run with -v to list them).\n' \
    "$clean" "$unreachable"

if [ "$auth_failed" -gt 0 ]; then
    echo
    echo "WARNING: $auth_failed host(s) refused the SSH login (Permission denied),"
    echo "so nothing could be checked or killed there. Passwordless SSH uses the"
    echo "key $SSH_KEY listed in ~/.ssh/authorized_keys;"
    echo "make sure ~/.ssh and ~/.ssh/authorized_keys are only writable by you."
fi

if [ "$mode" = "list" ] && [ "$total_procs" -gt 0 ]; then
    echo
    echo "Re-run with 'kill' to terminate them:"
    echo "    $(basename "$0") kill"
fi

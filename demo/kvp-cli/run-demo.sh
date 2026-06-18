#!/usr/bin/env bash
#
# Demo: inspecting azure-init diagnostics and a provisioning report with the
# libazureinit-kvp CLI.
#
# This walks through writing some sample diagnostics + a provisioning report
# into a KVP pool file and reading them back with the CLI -- primarily via
# `dump`. It does NOT touch the real Hyper-V pool at /var/lib/hyperv; it uses a
# throwaway directory so it is safe to run anywhere.
#
# Usage:
#   ./run-demo.sh            # step through, pausing between commands
#   STEP=0 ./run-demo.sh     # run straight through without pausing
#
set -euo pipefail

# --- Locate the repo, the CLI binary, and the demo data -----------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"

# Throwaway pool directory. The CLI derives the file name (.kvp_pool_1 for the
# guest pool) from --pool, so we only point it at a directory.
POOL_DIR="${POOL_DIR:-$SCRIPT_DIR/.demo-pool}"
DIAGNOSTICS="diagnostics.kvp"
REPORT="provisioning-report.kvp"

# Run from the demo directory so the input files show up as short,
# copy-pasteable paths in the echoed commands (e.g. `--file diagnostics.kvp`).
cd "$SCRIPT_DIR"

# Pause between steps unless STEP=0 (handy for a live, narrated demo).
STEP="${STEP:-1}"

# --- Pretty helpers -----------------------------------------------------------
BOLD="$(tput bold 2>/dev/null || true)"
DIM="$(tput dim 2>/dev/null || true)"
CYAN="$(tput setaf 6 2>/dev/null || true)"
GREEN="$(tput setaf 2 2>/dev/null || true)"
RESET="$(tput sgr0 2>/dev/null || true)"

KVP=()

say() { printf '\n%s\n' "${BOLD}${CYAN}# $*${RESET}"; }

pause() {
  if [[ "$STEP" != "0" ]]; then
    printf '%s' "${DIM}  (press enter)${RESET}"
    read -r _
  fi
}

# Render an argument list the way a user would actually type it, quoting any
# argument that contains whitespace so the echoed command is copy-pasteable.
fmt_args() {
  local out="" a
  for a in "$@"; do
    if [[ "$a" == *[[:space:]]* ]]; then
      out+=" \"$a\""
    else
      out+=" $a"
    fi
  done
  printf '%s' "${out# }"
}

# Echo a CLI invocation, then run it. Non-zero exits are reported (not fatal)
# so we can demonstrate commands that signal status through their exit code
# (e.g. `read` of a missing key, `is-stale` on a fresh pool).
run() {
  printf '%s\n' "${GREEN}\$ libazureinit-kvp $(fmt_args "$@")${RESET}"
  local rc=0
  "${KVP[@]}" "$@" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    printf '%s\n' "${DIM}  (exit status: $rc)${RESET}"
  fi
  return 0
}

# Echo a CLI invocation piped through grep, then run it -- used to spotlight a
# few records without dumping the whole pool.
run_grep() {
  local pattern="$1"
  shift
  printf '%s\n' "${GREEN}\$ libazureinit-kvp $(fmt_args "$@") | grep ${pattern}${RESET}"
  "${KVP[@]}" "$@" | grep "$pattern" || true
}

# Echo a `cat`/`head` of an input file and print it, so the audience sees
# exactly what the data fed to `load` / `append-multiple` has to look like.
# Pass a line count as the 2nd arg to preview only the first N lines.
show_file() {
  local file="$1" n="${2:-0}"
  if [[ "$n" -gt 0 ]]; then
    printf '%s\n' "${GREEN}\$ head -n ${n} ${file}${RESET}"
    head -n "$n" "$file"
  else
    printf '%s\n' "${GREEN}\$ cat ${file}${RESET}"
    cat "$file"
  fi
}

# --- Build the CLI ------------------------------------------------------------
say "Building the libazureinit-kvp CLI"
cargo build --quiet -p libazureinit-kvp --bin libazureinit-kvp
KVP=("$REPO_ROOT/target/debug/libazureinit-kvp" --pool guest --dir "$POOL_DIR")
printf '%s\n' "${DIM}  binary: $REPO_ROOT/target/debug/libazureinit-kvp${RESET}"
printf '%s\n' "${DIM}  pool dir: $POOL_DIR (throwaway; not /var/lib/hyperv)${RESET}"

# Start from a clean slate so the demo is repeatable.
mkdir -p "$POOL_DIR"
run clear
pause

say "1. Nothing provisioned yet -- the pool is empty"
run info
pause

say "2. Load the diagnostics a provisioning run produced -- here is the input file"
printf '%s\n' "${DIM}  KEY=VALUE per line; previewing the first 4 of $(wc -l < "$DIAGNOSTICS") records:${RESET}"
show_file "$DIAGNOSTICS" 4
printf '%s\n' "${DIM}  now load the whole file into the pool:${RESET}"
run load --file "$DIAGNOSTICS"
pause

say "3. Dump every diagnostic record in the order it was written"
run dump
pause

say "4. Append a provisioning report on top -- this is the input it expects"
show_file "$REPORT"
printf '%s\n' "${DIM}  append those records with 'append-multiple --file':${RESET}"
run append-multiple --file "$REPORT"
pause

say "5. Dump everything now in the pool (diagnostics + report)"
run dump
pause

say "6. 'entries' gives a sorted, de-duplicated view -- the report keys group together"
run entries
pause

say "7. 'read' a single value back by key"
run read "azure-init/report/status"
run read "azure-init/report/duration_ms"
printf '%s\n' "${DIM}  ...and a key that does not exist -- signalled via exit status 1:${RESET}"
run read "azure-init/report/does-not-exist"
pause

say "8. 'write' a single record -- e.g. an operator annotating the pool"
run write "azure-init/report/remediation" "Restarted azure-init.service at 23:05Z"
run read "azure-init/report/remediation"
pause

say "9. 'write --append' keeps prior values for a key instead of replacing"
run write --append "azure-init/report/remediation" "Re-ran provisioning at 23:07Z"
printf '%s\n' "${DIM}  dump preserves BOTH values (duplicate keys, in write order):${RESET}"
run_grep remediation dump
printf '%s\n' "${DIM}  while 'read' returns only the most recent value:${RESET}"
run read "azure-init/report/remediation"
pause

say "10. 'delete' removes every record for a key (prints true / false)"
run delete "azure-init/report/remediation"
printf '%s\n' "${DIM}  deleting again prints false -- nothing left to remove:${RESET}"
run delete "azure-init/report/remediation"
pause

say "11. 'delete-multiple' removes several keys at once (prints count removed)"
run delete-multiple \
  "azure-init/report/warnings" \
  "azure-init/report/errors" \
  "azure-init/report/imds_endpoint"
pause

say "12. 'is-stale' reports whether the pool predates the current boot"
printf '%s\n' "${DIM}  fresh data -> not stale -> exit status 1:${RESET}"
run is-stale
pause

say "13. 'clear --if-stale' only clears a stale pool -- fresh data is left intact"
run clear --if-stale
run info
pause

say "14. Machine-readable JSON for downstream tooling (e.g. pipe to jq)"
run --json entries
pause

say "15. 'clear' unconditionally empties the pool"
run clear
run info
pause

say "Demo complete. Re-run with STEP=0 for a non-interactive pass."
printf '%s\n' "${DIM}  pool file left at: $POOL_DIR/.kvp_pool_1${RESET}"

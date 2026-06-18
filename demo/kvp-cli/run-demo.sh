#!/usr/bin/env bash
#
# Demo: inspecting real azure-init / cloud-init guest KVP pools with the
# libazureinit-kvp CLI.
#
# The pools come from KVP telemetry logs captured off three VMs:
#   * azure-init.kvp  -- an azure-init guest that provisioned successfully
#   * cloud-init.kvp  -- a cloud-init guest that provisioned successfully
#   * failed-vm.kvp   -- a cloud-init guest with a forced provisioning failure
#
# At setup we rebuild each capture into a real binary pool file, then stage it
# the way Hyper-V would (a .kvp_pool_1 file the guest reads) and inspect it.
# This does NOT touch the real host pool at /var/lib/hyperv -- everything runs
# in a throwaway directory, and the "sudo cp ... /var/lib/hyperv" lines shown
# are illustrative only.
#
# Usage:
#   ./run-demo.sh            # step through, pausing between commands
#   STEP=0 ./run-demo.sh     # run straight through without pausing
#
set -euo pipefail

# --- Locate the repo, the CLI binary, and the demo data -----------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
PARSER="$SCRIPT_DIR/kvp_log_to_pairs.py"
KVPBIN="$REPO_ROOT/target/debug/libazureinit-kvp"

# Run from the demo directory so input files show up as short paths.
cd "$SCRIPT_DIR"

# Pause between steps unless STEP=0 (handy for a live, narrated demo).
STEP="${STEP:-1}"

# Throwaway workspace. We mimic the host layout (.../var/lib/hyperv) so the
# guest pool file path looks familiar, without ever touching the real one.
WORK="$(mktemp -d)"
FAKE_HYPERV="$WORK/var/lib/hyperv"
mkdir -p "$FAKE_HYPERV"
trap 'rm -rf "$WORK"' EXIT

# --- Pretty helpers -----------------------------------------------------------
BOLD="$(tput bold 2>/dev/null || true)"
DIM="$(tput dim 2>/dev/null || true)"
CYAN="$(tput setaf 6 2>/dev/null || true)"
GREEN="$(tput setaf 2 2>/dev/null || true)"
RESET="$(tput sgr0 2>/dev/null || true)"

# CLI invocation targeting the staged guest pool. --pool/--dir are hidden from
# the echoed commands so they read like a default on-host invocation.
KVP=("$KVPBIN" --pool guest --dir "$FAKE_HYPERV")

say() { printf '\n%s\n' "${BOLD}${CYAN}# $*${RESET}"; }

pause() {
  if [[ "$STEP" != "0" ]]; then
    read -r _
  fi
}

# Render an argument list the way a user would type it, quoting any argument
# that contains whitespace so the echoed command is copy-pasteable.
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
# so we can demonstrate commands that signal status via exit code (e.g. `read`
# of a missing key).
run() {
  printf '%s\n' "${DIM}\$${RESET} ${GREEN}libazureinit-kvp $(fmt_args "$@")${RESET}"
  local rc=0
  "${KVP[@]}" "$@" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    printf '%s\n' "${DIM}  (exit status: $rc)${RESET}"
  fi
  return 0
}

# Echo a CLI invocation piped through `head`, then run it -- used to peek at
# the top of a large pool without dumping every record.
run_head() {
  local n="$1"
  shift
  printf '%s\n' "${DIM}\$${RESET} ${GREEN}libazureinit-kvp $(fmt_args "$@")${RESET}${DIM} | head -n ${n}${RESET}"
  "${KVP[@]}" "$@" 2>/dev/null | head -n "$n" || true
}

# Stage a prebuilt pool as the guest's .kvp_pool_1, showing the copy the way an
# operator would run it on the host (path is illustrative; see header).
stage_pool() {
  local name="$1"
  printf '%s\n' "${DIM}\$${RESET} ${GREEN}sudo cp ${name}.kvp_pool_1 /var/lib/hyperv/.kvp_pool_1${RESET}"
  cp "$WORK/pools/$name/.kvp_pool_1" "$FAKE_HYPERV/.kvp_pool_1"
}

# Rebuild a capture log into a real binary pool. --unsafe lifts the per-record
# limits so large cloud-init telemetry values load verbatim.
build_pool() {
  local log="$1" name="$2"
  mkdir -p "$WORK/pools/$name"
  python3 "$PARSER" "$SCRIPT_DIR/$log" \
    | "$KVPBIN" --pool guest --dir "$WORK/pools/$name" --unsafe load >/dev/null
}

# --- Setup (quiet) ------------------------------------------------------------
say "Building the libazureinit-kvp CLI and staging captured guest pools"
cargo build --quiet -p libazureinit-kvp --bin libazureinit-kvp
build_pool azure-init.kvp azure-init
build_pool cloud-init.kvp cloud-init
build_pool failed-vm.kvp  failed-vm
printf '%s\n' "${DIM}  rebuilt 3 pools from captured KVP logs (azure-init, cloud-init, failed-vm)${RESET}"
printf '%s\n' "${DIM}  working dir: $WORK (throwaway; the real host pool is untouched)${RESET}"
pause

# --- 1. A successful azure-init guest -----------------------------------------
say "1. A guest provisioned by azure-init -- drop its pool in place and peek at the top"
stage_pool azure-init
run_head 6 dump
printf '%s\n' "${DIM}  every line is one telemetry record: span/event keys with timing + messages${RESET}"
pause

say "   Did provisioning succeed? Read the provisioning report:"
run read PROVISIONING_REPORT
printf '%s\n' "${DIM}  result=success -- azure-init reported the VM provisioned cleanly${RESET}"
pause

# --- 2. A successful cloud-init guest -----------------------------------------
say "2. A guest provisioned by cloud-init -- same pool, different producer"
stage_pool cloud-init
run_head 6 dump
printf '%s\n' "${DIM}  cloud-init writes JSON event records, but the pool format is identical${RESET}"
pause

say "   And its provisioning report:"
run read PROVISIONING_REPORT
printf '%s\n' "${DIM}  result=success -- cloud-init agrees the VM came up fine${RESET}"
pause

# --- 3. A failed guest --------------------------------------------------------
say "3. A guest where provisioning FAILED -- stage its pool and read the report"
stage_pool failed-vm
run read PROVISIONING_REPORT
printf '%s\n' "${DIM}  result=error with a reason + documentation_url -- exactly what we triage${RESET}"
pause

# --- 4. Editing a pool: write + delete + clear --------------------------------
say "4. The CLI also edits pools. Re-stage the azure-init pool, then 'write' an operator note:"
stage_pool azure-init
run write operator/note "triaged 2026-06-17, provisioning OK"
printf '%s\n' "${DIM}  'write' added a record.${RESET}"
pause

say "   Read it straight back to confirm it's there:"
run read operator/note
pause

say "   Now remove a key with 'delete':"
run delete PROVISIONING_REPORT
printf '%s\n' "${DIM}  'delete' prints true when it removed a record.${RESET}"
pause

say "   Read it back -- it's gone:"
run read PROVISIONING_REPORT
printf '%s\n' "${DIM}  'read' exits 1 because the key no longer exists${RESET}"
pause

say "   Finally, 'clear' empties the whole pool:"
run clear
pause

say "   Dump confirms there's nothing left:"
run_head 6 dump
printf '%s\n' "${DIM}  the pool is empty${RESET}"
pause

say "Demo complete. Re-run with STEP=0 for a non-interactive pass."

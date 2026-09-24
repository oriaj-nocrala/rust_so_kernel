#!/usr/bin/env bash
# scripts/metal-resume.sh — close the bare-metal loop after the machine comes
# back to Linux (docs/metal/autonomous-loop-plan.md, phase 6).
#
# Run from the tty1 login shell (~/.zprofile), which kmscon logs in
# automatically on boot. If a run is pending (target/metal/pending, written
# by metal-run.sh), it collects the result and resumes the Claude Code
# session that launched the run, in the foreground of this terminal — so
# whoever sits at the machine sees what the agent does and can interrupt it.
# With nothing pending it does nothing and returns at once.
#
# Brakes, both files under target/metal/:
#   budget   number of automatic resumes left. Missing or 0 means collect
#            only: the result is printed and nothing is resumed. Each
#            resume decrements it, so an agent that doesn't converge stops
#            rebooting the machine after that many cycles.
#   stop     if present, collect only (and leave the file in place).
#
#   scripts/metal-resume.sh            what ~/.zprofile runs
#   scripts/metal-resume.sh --dry-run  say what would happen, change nothing

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
STATE="$REPO_ROOT/target/metal"
DRY=0
[[ "${1:-}" == "--dry-run" ]] && DRY=1

say() { echo "metal-resume: $*"; }

[[ -e "$STATE/pending" ]] || exit 0

nonce="$(sed -n 's/^nonce=//p' "$STATE/pending")"
session="$(sed -n 's/^session=//p' "$STATE/pending")"
budget="$(cat "$STATE/budget" 2>/dev/null || echo 0)"
[[ "$budget" =~ ^[0-9]+$ ]] || budget=0
say "run $nonce is pending (session ${session:-unknown}, $budget automatic resume(s) left)"

if [[ $DRY -eq 1 ]]; then
    say "--dry-run: would collect, then $( [[ -e $STATE/stop || $budget -eq 0 ]] && echo "stop" || echo "resume the agent")"
    exit 0
fi

# Collecting needs the stick, not the network; resuming needs both.
out="$(scripts/metal-run.sh --collect 2>&1)"
echo "$out"
run="$STATE/runs/$nonce"
[[ -d "$run" ]] || { say "collect failed; not resuming"; exit 1; }
echo "$out" > "$run/collect.out"
verdict="$(cat "$run/verdict")"

if [[ -e "$STATE/stop" ]]; then
    say "target/metal/stop exists: collected, not resuming"
    exit 0
fi
if (( budget == 0 )); then
    say "no automatic resumes left (target/metal/budget): collected, not resuming"
    exit 0
fi

for _ in $(seq 1 60); do
    curl -s -m 3 -o /dev/null https://api.anthropic.com && break
    sleep 2
done
echo $(( budget - 1 )) > "$STATE/budget"

prompt="[metal-resume] Volviste de una ejecución en metal (la máquina se reinició y esta sesión se retomó sola, sin nadie delante). Veredicto: ${verdict}. Todo está en target/metal/runs/${nonce}/ (boot.log, verdict, collect.out). Lee el log guardado antes de creerte el veredicto. Quedan $(( budget - 1 )) reanudaciones automáticas (target/metal/budget). Si la tarea en curso necesita otra vuelta en metal y quedan reanudaciones, lánzala con scripts/metal-run.sh; si no, detente y resume el resultado para el usuario."

say "resuming the agent (Ctrl-C to take over)"
if [[ -n "$session" ]]; then
    exec claude --resume "$session" "$prompt"
else
    exec claude --continue "$prompt"
fi

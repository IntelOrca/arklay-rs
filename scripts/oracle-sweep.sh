#!/usr/bin/env bash
# Batch parity sweep: run scripts/oracle-room.sh for many rooms/players and
# write a TSV summary of state parity and frame diffs.
#
# Usage:
#   scripts/oracle-sweep.sh [ROOM ...]
#
#   ROOM...   three hex digits; default is every room found in the game
#             install (RE1_ASSETS) that has an RDT for the player digit.
#
# Environment:
#   SWEEP_PLAYERS  space list of player digits (default "0 1")
#   SWEEP_TICKS    traced ticks per run (default 120)
#   SWEEP_JOBS     concurrent runs (default 4)
#   SWEEP_OUT      output root (default target/oracle/sweep)
#   RE1_ASSETS     game install root (default /usr/share/games/re1)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ASSETS="${RE1_ASSETS:-/usr/share/games/re1}"
PLAYERS="${SWEEP_PLAYERS:-0 1}"
TICKS="${SWEEP_TICKS:-120}"
JOBS="${SWEEP_JOBS:-4}"
OUT="${SWEEP_OUT:-$REPO_ROOT/target/oracle/sweep}"

case "$OUT" in
    /*) ;;
    *) OUT="$PWD/$OUT" ;;
esac
mkdir -p "$OUT"

if [ "$#" -gt 0 ]; then
    ROOMS=("$@")
else
    mapfile -t ROOMS < <(
        find "$ASSETS/JPN"/STAGE* -iname '*.rdt' 2>/dev/null |
            sed -E 's|.*/[Rr][Oo][Oo][Mm]([0-9A-Fa-f]{3})[0-9]\.rdt$|\1|I' |
            grep -E '^[0-9A-Fa-f]{3}$' | sort -u
    )
fi

rdt_for() {  # room player -> matching RDT path or empty
    local room="$1" player="$2"
    local stage="${room:0:1}"
    # A 4-byte RDT is the shipped stub for a cut room (110, 119, 200, 20C,
    # 213-21C); the reference crashes loading one, and the real game cannot
    # reach them, so they are not loadable rooms.
    find "$ASSETS/JPN/STAGE$stage" -maxdepth 1 -iname "ROOM${room}${player}.rdt" \
        -size +4c 2>/dev/null | head -1
}

run_one() {
    local room="$1" player="$2"
    local dir="$OUT/${room}_p${player}"
    local log="$dir/run.log"
    mkdir -p "$dir"
    local status=OK
    if ! "$REPO_ROOT/scripts/oracle-room.sh" "$room" "$player" "$TICKS" "$dir" >"$log" 2>&1; then
        status=FAIL
    fi

    local state="?"
    if grep -q "MATCH: .* ticks identical" "$log"; then
        state=MATCH
    elif grep -q "first divergence" "$log"; then
        state=MISMATCH
    elif [ "$status" = FAIL ]; then
        state=ERROR
    fi

    local worst="-"
    worst="$(sed -n 's/^worst frame \([0-9]*\): \([0-9.]*\)% differing.*/\2/p' "$log" | tail -1)"
    [ -n "$worst" ] || worst="-"

    local captures="-" missing="-"
    captures="$(sed -n 's/^frames: \([0-9]*\) compared.*/\1/p' "$log" | tail -1)"
    [ -n "$captures" ] || captures=0
    missing="$(sed -n 's/^missing in ref: \[\(.*\)\]$/\1/p' "$log" | tail -1)"

    local note=""
    if [ "$status" = FAIL ]; then
        note="$(grep -m1 -E 'error|Error|panicked|FAILED|missing' "$log" | cut -c1-120 || true)"
        note="${note//$'\t'/ }"
    elif [ "$missing" != "-" ] && [ -n "$missing" ]; then
        note="missing-captures:$missing"
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$room" "$player" "$state" "$worst" "$captures" "$status" "$note" >>"$OUT/summary.tsv"
}

: >"$OUT/summary.tsv"
printf 'room\tplayer\tstate\tworst%%\tcaptures\trun\tnote\n' >>"$OUT/summary.tsv"

queue=()
for room in "${ROOMS[@]}"; do
    for player in $PLAYERS; do
        if [ -n "$(rdt_for "$room" "$player")" ]; then
            queue+=("$room $player")
        fi
    done
done

echo "sweep: ${#queue[@]} runs, ${TICKS} ticks, ${JOBS} jobs -> $OUT" >&2

running=0
for item in "${queue[@]}"; do
    run_one $item &
    running=$((running + 1))
    if [ "$running" -ge "$JOBS" ]; then
        wait -n || true
        running=$((running - 1))
    fi
done
wait || true

echo "== summary: $OUT/summary.tsv" >&2
column -t -s $'\t' "$OUT/summary.tsv" >&2

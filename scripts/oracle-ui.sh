#!/usr/bin/env bash
# Differential oracle runner for an input-playback UI/interaction scenario.
#
# Boots the reference and arklay into the same room, replays the same scripted
# pad input on both, writes both oracle traces and capture sets into OUTDIR,
# then runs scripts/oracle.py to compare game state tick-for-tick and frames
# pixel-by-pixel. Unlike oracle-room.sh, the reference is armed with
# --test-input, which traces and captures every frame past the room-entry arm
# point - including the frames a pause menu owns, when the reference's gameplay
# loop is suspended.
#
# Room mode boots both engines into the same *new game* in ROOM. Save mode
# boots both engines from the same checked-in `savedatN.dat` (the file's own
# stage/room/character/flags/inventory), which is how scenarios that need
# files, keys, the item box or a typewriter reach their screens. The saves
# live in oracle/saves/ and are generated with `arklay --setup-script ...
# --save-out`; see oracle/saves/README.md.
#
# Usage:
#   scripts/oracle-ui.sh ROOM PLAYER INPUT [TICKS] [OUTDIR] [CAPTURE_EVERY]
#   scripts/oracle-ui.sh --save SAVEDAT INPUT [TICKS] [OUTDIR] [CAPTURE_EVERY]
#
#   ROOM           three hex digits, stage + room (e.g. 101)
#   PLAYER         0 = Chris, 1 = Jill
#   SAVEDAT        save block file, e.g. oracle/saves/pause_chris_101.dat
#   INPUT          playback script (see oracle/inputs/README.md)
#   TICKS          traced ticks (default 300)
#   OUTDIR         output directory
#   CAPTURE_EVERY  capture cadence (default 1 = every traced tick)
#
# Environment overrides:
#   RE1_REF_REPO, RE1_REF_BIN, RE1_REF_SYSROOT, RE1_ASSETS, ARKLAY_BIN,
#   ARKLAY_RE1_PACK, ARKLAY_LANGUAGE (default ja-JP, matching the reference's
#   config.ini [Assets] Version=JPN).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REF_REPO="${RE1_REF_REPO:-$REPO_ROOT/../ref}"
REF_BIN="${RE1_REF_BIN:-$REF_REPO/build/linux/residentevil}"
SYSROOT="${RE1_REF_SYSROOT:-$REF_REPO/build/sysroot32}"
ASSETS="${RE1_ASSETS:-/usr/share/games/re1}"
ARK_BIN="${ARKLAY_BIN:-$REPO_ROOT/target/release/arklay}"
PACK="${ARKLAY_RE1_PACK:-$REPO_ROOT/games/re1.akpak}"
LANGUAGE="${ARKLAY_LANGUAGE:-ja-JP}"

SAVE=""
ROOM=""
PLAYER=""
if [[ "${1:-}" == "--save" ]]; then
    SAVE="${2:?usage: oracle-ui.sh --save SAVEDAT INPUT [TICKS] [OUTDIR] [CAPTURE_EVERY]}"
    INPUT="${3:?usage: oracle-ui.sh --save SAVEDAT INPUT [TICKS] [OUTDIR] [CAPTURE_EVERY]}"
    TICKS="${4:-300}"
    OUT="${5:-$REPO_ROOT/target/oracle/ui-save-$(basename "${INPUT%.*}")}"
    EVERY="${6:-1}"
else
    ROOM="${1:?usage: oracle-ui.sh ROOM PLAYER INPUT [TICKS] [OUTDIR] [CAPTURE_EVERY]}"
    PLAYER="${2:?usage: oracle-ui.sh ROOM PLAYER INPUT [TICKS] [OUTDIR] [CAPTURE_EVERY]}"
    INPUT="${3:?usage: oracle-ui.sh ROOM PLAYER INPUT [TICKS] [OUTDIR] [CAPTURE_EVERY]}"
    TICKS="${4:-300}"
    OUT="${5:-$REPO_ROOT/target/oracle/ui-$(basename "${INPUT%.*}")}"
    EVERY="${6:-1}"
fi

for path in "$REF_BIN" "$ARK_BIN" "$PACK" "$INPUT"; do
    if [ ! -e "$path" ]; then
        echo "missing $path" >&2
        exit 2
    fi
done
if [[ -n "$SAVE" && ! -e "$SAVE" ]]; then
    echo "missing $SAVE" >&2
    exit 2
fi

case "$OUT" in
    /*) ;;
    *) OUT="$PWD/$OUT" ;;
esac
# The reference runs from OUT, so every path it is handed must be absolute
# before it cd's.
abs() {
    case "$1" in
        /*) printf '%s' "$1" ;;
        *) printf '%s' "$PWD/$1" ;;
    esac
}
INPUT="$(abs "$INPUT")"
if [[ -n "$SAVE" ]]; then
    SAVE="$(abs "$SAVE")"
fi
mkdir -p "$OUT/refcaps" "$OUT/arkcaps"

REF_CONFIG="$(dirname "$REF_BIN")/config.ini"
if [ ! -f "$REF_CONFIG" ]; then
    cat > "$REF_CONFIG" <<EOF
[Display]
FullScreen=0
Width=320
Height=240
BitDepth=32
VSync=0

[Assets]
Path=$ASSETS
Version=JPN

[Save]
Path=

[Game]
Mode=OG
Ps1EndingCredits=0
SkipUnskippableFmv=0

[Debug]
EnableDebug=0
EOF
    echo "wrote $REF_CONFIG"
fi

if [[ -n "$SAVE" ]]; then
    WHAT="save $(basename "$SAVE")"
    REF_BOOT=(--test-load "$SAVE")
    ARK_BOOT=(--ui game --load "$SAVE")
else
    WHAT="room $ROOM player $PLAYER"
    REF_BOOT=(--test-newgame "$PLAYER" --test-room "$ROOM")
    ARK_BOOT=(--ui game --room "$ROOM" --player "$PLAYER" --character "$PLAYER")
fi

echo "== reference: $WHAT, input $INPUT, $TICKS ticks"
( cd "$OUT" && \
SDL_VIDEODRIVER=offscreen \
LD_LIBRARY_PATH="$SYSROOT/usr/lib32:$SYSROOT/usr" \
__EGL_VENDOR_LIBRARY_FILENAMES="$SYSROOT/usr/share/glvnd/egl_vendor.d/50_mesa.json" \
LIBGL_DRIVERS_PATH="$SYSROOT/usr/lib32/dri" \
LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe MESA_LOADER_DRIVER_OVERRIDE=llvmpipe \
    "$REF_BIN" \
    "${REF_BOOT[@]}" \
    --test-input "$INPUT" \
    --test-trace "$OUT/ref.trace" \
    --test-capture-every "$EVERY" --test-capture-dir "$OUT/refcaps" \
    --test-exit-after "$TICKS" --test-nolimit )

echo "== arklay: $WHAT, input $INPUT, $TICKS ticks"
"$ARK_BIN" "$PACK" "${ARK_BOOT[@]}" \
    --language "$LANGUAGE" --ticks "$TICKS" --no-mods \
    --input "$INPUT" \
    --trace "$OUT/ark.trace" \
    --capture-every "$EVERY" --capture-dir "$OUT/arkcaps"

echo "== compare"
python3 "$REPO_ROOT/scripts/oracle.py" compare "$OUT/ref.trace" "$OUT/ark.trace" \
    --first 5 --context 1
python3 "$REPO_ROOT/scripts/oracle.py" pixels "$OUT/refcaps" "$OUT/arkcaps" \
    --report 20
echo "artifacts: $OUT"

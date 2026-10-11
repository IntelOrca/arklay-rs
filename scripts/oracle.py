#!/usr/bin/env python3
"""Oracle comparator for the arklay differential harness.

``compare`` diffs two ``oracle-trace v1`` files (see oracle/trace-v1.md) emitted
by the C++ reference decomp and by arklay, and reports the first divergent tick
and field plus a per-field mismatch summary.

``pixels`` diffs the per-tick capture frames of both engines
(``f%05d.rgba`` from the reference, ``f%05d.bmp`` from arklay) and reports a
tolerance summary with the worst frames.

Usage:
    python scripts/oracle.py compare REF_TRACE ARK_TRACE [--first N]
        [--context N] [--offset N] [--max-scan N] [--json OUT]
    python scripts/oracle.py pixels REF_DIR ARK_DIR [--report N]
        [--diff-dir DIR]
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class Entity:
    slot: int
    fields: dict[str, str]

    def __str__(self) -> str:
        values = ",".join(self.fields[k] for k in ENTITY_FIELDS)
        return f"{self.slot},{values}"


ENTITY_FIELDS = [
    "id",
    "x",
    "y",
    "z",
    "angle",
    "state",
    "beh",
    "as",
    "hp",
    "hit",
    "status",
    "ticks",
]


@dataclass
class Tick:
    t: int
    room: str
    rng: str
    rseed: str
    fade: str
    player: dict[str, str]
    cam: str
    entities: dict[int, Entity] = field(default_factory=dict)
    frame_hash: str = ""

    def entity_str(self, slot: int) -> str:
        entity = self.entities.get(slot)
        return "absent" if entity is None else str(entity)


PLAYER_FIELDS = ["x", "y", "z", "angle", "beh", "as", "hp"]


def normalize_room(stage: str, room: str) -> str:
    """Accept both the 3-digit form (1:106) and the byte form (1:6)."""
    value = int(room, 16)
    if value > 255:
        value &= 0xFF
    return f"{int(stage)}:{value}"


def normalize_fade(text: str) -> str:
    """An inactive fade (state < 0) only compares as inactive.

    The two engines leave different stale counters once the accumulator has
    run negative (the reference keeps the last step, arklay zeroes it), so
    only the active range compares counter and state.
    """
    counter, state = text.split(":")
    if int(state) < 0:
        return "inactive"
    return f"{counter}:{state}"


def parse_trace(path: str) -> list[Tick]:
    ticks: list[Tick] = []
    with open(path, "r", encoding="utf-8") as handle:
        header = handle.readline().strip()
        if not header.startswith("oracle-trace v1"):
            raise SystemExit(f"{path}: not an oracle trace (header: {header!r})")
        for line in handle:
            line = line.strip()
            if not line:
                continue
            tokens = line.split(" ")
            fields: dict[str, str] = {}
            entities: dict[int, Entity] = {}
            player_values: dict[str, str] = {}
            for token in tokens:
                key, _, value = token.partition("=")
                if key == "e":
                    parts = value.split(",")
                    slot = int(parts[0])
                    values = dict(zip(ENTITY_FIELDS, parts[1:]))
                    entities[slot] = Entity(slot, values)
                elif key == "p":
                    values = value.split(",")
                    player_values = dict(zip(PLAYER_FIELDS, values))
                else:
                    fields[key] = value
            stage, _, room = fields["room"].partition(":")
            ticks.append(
                Tick(
                    t=int(fields["t"]),
                    room=normalize_room(stage, room),
                    rng=fields["rng"],
                    rseed=fields["rseed"],
                    fade=normalize_fade(fields["fade"]),
                    player=player_values,
                    cam=fields["cam"],
                    entities=entities,
                    frame_hash=fields.get("f", ""),
                )
            )
    return ticks


def tick_fields(tick: Tick) -> list[tuple[str, str]]:
    items: list[tuple[str, str]] = [
        ("room", tick.room),
        ("rng", tick.rng),
        ("rseed", tick.rseed),
        ("fade", tick.fade),
        ("cam", tick.cam),
    ]
    for name in PLAYER_FIELDS:
        items.append((f"p.{name}", tick.player.get(name, "?")))
    for slot in sorted(tick.entities):
        entity = tick.entities[slot]
        for name in ENTITY_FIELDS:
            items.append((f"e{slot}.{name}", entity.fields.get(name, "?")))
    return items


def compare_pair(ref: list[Tick], ark: list[Tick], ref_offset: int, ark_offset: int,
                 first: int, context: int) -> dict:
    """Compare the two traces at the given start offsets."""
    length = min(len(ref) - ref_offset, len(ark) - ark_offset)
    first_divergences: list[dict] = []
    mismatch_counts: dict[str, int] = {}
    first_divergent_index: int | None = None
    matched_ticks = 0

    for i in range(length):
        left = ref[ref_offset + i]
        right = ark[ark_offset + i]
        left_fields = dict(tick_fields(left))
        right_fields = dict(tick_fields(right))
        keys = sorted(set(left_fields) | set(right_fields))
        differences = [
            (key, left_fields.get(key, "absent"), right_fields.get(key, "absent"))
            for key in keys
            if left_fields.get(key, "absent") != right_fields.get(key, "absent")
        ]
        if differences:
            if first_divergent_index is None:
                first_divergent_index = i
            for key, _, _ in differences:
                mismatch_counts[key] = mismatch_counts.get(key, 0) + 1
            if len(first_divergences) < first:
                first_divergences.append(
                    {
                        "index": i,
                        "ref_t": left.t,
                        "ark_t": right.t,
                        "differences": differences,
                    }
                )
        else:
            matched_ticks += 1

    context_rows: list[dict] = []
    if first_divergent_index is not None and context > 0:
        start = max(0, first_divergent_index - context)
        end = min(length, first_divergent_index + context + 1)
        for i in range(start, end):
            left = ref[ref_offset + i]
            right = ark[ark_offset + i]
            context_rows.append(
                {
                    "index": i,
                    "ref": tick_line(left),
                    "ark": tick_line(right),
                }
            )

    return {
        "length": length,
        "matched_ticks": matched_ticks,
        "first_divergent_index": first_divergent_index,
        "first_divergences": first_divergences,
        "mismatch_counts": dict(
            sorted(mismatch_counts.items(), key=lambda item: -item[1])
        ),
        "context": context_rows,
        "ref_offset": ref_offset,
        "ark_offset": ark_offset,
    }


def tick_line(tick: Tick) -> str:
    entities = " ".join(f"e={tick.entity_str(slot)}" for slot in sorted(tick.entities))
    player = ",".join(tick.player.get(name, "?") for name in PLAYER_FIELDS)
    return (
        f"t={tick.t} room={tick.room} rng={tick.rng} rseed={tick.rseed} "
        f"fade={tick.fade} p={player} cam={tick.cam} {entities} f={tick.frame_hash}"
    )


def find_best_alignment(ref: list[Tick], ark: list[Tick], max_scan: int) -> dict:
    """Try start offsets to find the one with the longest identical prefix."""
    best: dict | None = None
    for offset in range(max_scan + 1):
        if offset >= len(ref) or offset >= len(ark):
            break
        length = min(len(ref) - offset, len(ark) - offset)
        matched = 0
        for i in range(length):
            if tick_fields(ref[offset + i]) == tick_fields(ark[i]):
                matched += 1
            else:
                break
        if best is None or matched > best["matched"]:
            best = {"offset": offset, "matched": matched}
    return best or {"offset": 0, "matched": 0}


def read_rgba_capture(path: Path) -> tuple[int, int, bytes]:
    """Read the reference's ``RE1CAP <w> <h>\\n`` + RGBA capture (top-down)."""
    data = path.read_bytes()
    newline = data.index(b"\n")
    header = data[:newline].decode("ascii").split()
    if header[0] != "RE1CAP":
        raise SystemExit(f"{path}: not a RE1CAP capture")
    width, height = int(header[1]), int(header[2])
    pixels = data[newline + 1:]
    if len(pixels) != width * height * 4:
        raise SystemExit(f"{path}: expected {width * height * 4} pixel bytes, got {len(pixels)}")
    return width, height, pixels


def read_bmp(path: Path) -> tuple[int, int, bytes]:
    """Read an 8-bit indexed or 24-bit BMP into top-down RGB bytes (3/pixel)."""
    data = path.read_bytes()
    if data[:2] != b"BM":
        raise SystemExit(f"{path}: not a BMP")
    offset = struct.unpack_from("<I", data, 10)[0]
    header_size = struct.unpack_from("<I", data, 14)[0]
    width, height = struct.unpack_from("<ii", data, 18)
    depth = struct.unpack_from("<H", data, 28)[0]
    if depth not in (8, 24, 32):
        raise SystemExit(f"{path}: unsupported BMP depth {depth}")
    top_down = height < 0
    height = abs(height)
    palette: list[tuple[int, int, int]] = []
    if depth == 8:
        colors = struct.unpack_from("<I", data, 46)[0] or 256
        palette_start = 14 + header_size
        for index in range(colors):
            b, g, r, _ = data[palette_start + index * 4: palette_start + index * 4 + 4]
            palette.append((r, g, b))
    stride = ((width * depth // 8) + 3) & ~3
    rows: list[bytes] = []
    for row in range(height):
        source = offset + row * stride
        line = bytearray()
        if depth == 8:
            for column in range(width):
                line += bytes(palette[data[source + column]])
        else:
            step = depth // 8
            for column in range(width):
                b, g, r = data[source + column * step: source + column * step + 3]
                line += bytes((r, g, b))
        rows.append(bytes(line))
    if not top_down:
        rows.reverse()
    return width, height, b"".join(rows)


def rgba_to_rgb(pixels: bytes) -> bytes:
    """Drop the alpha channel of a top-down RGBA buffer."""
    out = bytearray(len(pixels) // 4 * 3)
    for i in range(0, len(pixels), 4):
        out[i // 4 * 3: i // 4 * 3 + 3] = pixels[i:i + 3]
    return bytes(out)


def write_ppm(path: Path, width: int, height: int, rgb: bytes) -> None:
    with open(path, "wb") as handle:
        handle.write(f"P6\n{width} {height}\n255\n".encode("ascii"))
        handle.write(rgb)


def pixel_diff(ref_rgb: bytes, ark_rgb: bytes, width: int, height: int) -> dict:
    """Per-frame diff metrics between two RGB buffers."""
    total = width * height
    differing = 0
    beyond = {8: 0, 32: 0}
    total_error = 0
    max_delta = 0
    worst_x = worst_y = 0
    for i in range(0, len(ref_rgb), 3):
        delta = 0
        for channel in range(3):
            difference = abs(ref_rgb[i + channel] - ark_rgb[i + channel])
            delta = max(delta, difference)
            total_error += difference
        if delta > 0:
            differing += 1
            for threshold in beyond:
                if delta > threshold:
                    beyond[threshold] += 1
        if delta > max_delta:
            max_delta = delta
            pixel = i // 3
            worst_x, worst_y = pixel % width, pixel // width
    return {
        "total": total,
        "differing": differing,
        "beyond_8": beyond[8],
        "beyond_32": beyond[32],
        "mean_error": total_error / (total * 3),
        "max_delta": max_delta,
        "worst_x": worst_x,
        "worst_y": worst_y,
    }


def pixels_command(args: argparse.Namespace) -> int:
    ref_dir = Path(args.ref_dir)
    ark_dir = Path(args.ark_dir)
    ref_files = {int(p.stem[1:]): p for p in ref_dir.glob("f*.rgba")}
    ark_files = {int(p.stem[1:]): p for p in ark_dir.glob("f*.bmp")}
    ticks = sorted(set(ref_files) & set(ark_files))
    if not ticks:
        raise SystemExit("no matching capture frames")
    missing_ref = sorted(set(ark_files) - set(ref_files))
    missing_ark = sorted(set(ref_files) - set(ark_files))

    diff_dir = Path(args.diff_dir) if args.diff_dir else None
    if diff_dir:
        diff_dir.mkdir(parents=True, exist_ok=True)

    rows: list[dict] = []
    for tick in ticks:
        ref_w, ref_h, ref_pixels = read_rgba_capture(ref_files[tick])
        ark_w, ark_h, ark_pixels = read_bmp(ark_files[tick])
        if (ref_w, ref_h) != (ark_w, ark_h):
            raise SystemExit(
                f"frame {tick}: size mismatch ref {ref_w}x{ref_h} ark {ark_w}x{ark_h}"
            )
        ref_rgb = rgba_to_rgb(ref_pixels)
        metrics = pixel_diff(ref_rgb, ark_pixels, ref_w, ref_h)
        metrics["tick"] = tick
        rows.append(metrics)

    identical = sum(1 for row in rows if row["differing"] == 0)
    print(f"frames: {len(rows)} compared, {identical} byte-identical RGB")
    if missing_ref:
        print(f"missing in ref: {missing_ref[:10]}")
    if missing_ark:
        print(f"missing in ark: {missing_ark[:10]}")
    print(f"{'tick':>6} {'diff%':>7} {'>8':>6} {'>32':>6} {'mean':>7} {'max':>4}  worst")
    for row in sorted(rows, key=lambda r: -r["differing"])[: args.report]:
        percent = 100.0 * row["differing"] / row["total"]
        print(
            f"{row['tick']:>6} {percent:>6.2f}% {row['beyond_8']:>6} "
            f"{row['beyond_32']:>6} {row['mean_error']:>7.3f} {row['max_delta']:>4}  "
            f"({row['worst_x']},{row['worst_y']})"
        )
    worst = max(rows, key=lambda r: r["differing"])
    print(
        f"\nworst frame {worst['tick']}: {100.0 * worst['differing'] / worst['total']:.2f}% "
        f"differing, mean error {worst['mean_error']:.3f}/255"
    )
    if diff_dir:
        tick = worst["tick"]
        ref_w, ref_h, ref_pixels = read_rgba_capture(ref_files[tick])
        ark_w, ark_h, ark_pixels = read_bmp(ark_files[tick])
        ref_rgb = rgba_to_rgb(ref_pixels)
        amplified = bytearray()
        for i in range(len(ref_rgb)):
            amplified.append(min(255, abs(ref_rgb[i] - ark_pixels[i]) * 8))
        write_ppm(diff_dir / f"diff_f{tick:05d}.ppm", ref_w, ref_h, bytes(amplified))
        write_ppm(diff_dir / f"ref_f{tick:05d}.ppm", ref_w, ref_h, ref_rgb)
        write_ppm(diff_dir / f"ark_f{tick:05d}.ppm", ark_w, ark_h, ark_pixels)
        print(f"wrote diff/ref/ark PPMs for frame {tick} to {diff_dir}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    compare = sub.add_parser("compare", help="compare two oracle traces")
    compare.add_argument("ref_trace")
    compare.add_argument("ark_trace")
    compare.add_argument("--first", type=int, default=5)
    compare.add_argument("--context", type=int, default=3)
    compare.add_argument("--offset", type=int, default=None,
                         help="force this start offset for the reference trace")
    compare.add_argument("--ark-offset", type=int, default=0)
    compare.add_argument("--max-scan", type=int, default=600,
                         help="scan this many start offsets for the best alignment")
    compare.add_argument("--json", dest="json_out", default=None)

    pixels = sub.add_parser("pixels", help="compare the capture frame directories")
    pixels.add_argument("ref_dir")
    pixels.add_argument("ark_dir")
    pixels.add_argument("--report", type=int, default=10,
                        help="show this many worst frames")
    pixels.add_argument("--diff-dir", default=None,
                        help="write ref/ark/diff PPMs for the worst frame here")
    args = parser.parse_args()

    if args.command == "pixels":
        return pixels_command(args)

    ref = parse_trace(args.ref_trace)
    ark = parse_trace(args.ark_trace)
    print(f"ref: {len(ref)} ticks, ark: {len(ark)} ticks")

    scan = args.max_scan if args.offset is None else args.offset
    alignment = find_best_alignment(ref, ark, scan)
    if args.offset is not None:
        alignment = {"offset": args.offset, "matched": alignment["matched"]}
    print(
        f"alignment: ref offset {alignment['offset']} matches ark offset "
        f"{args.ark_offset} for {alignment['matched']} consecutive ticks"
    )

    result = compare_pair(
        ref, ark, alignment["offset"], args.ark_offset, args.first, args.context
    )
    result["alignment_scan"] = alignment

    if result["first_divergent_index"] is None:
        print(f"MATCH: {result['length']} ticks identical")
    else:
        index = result["first_divergent_index"]
        print(
            f"first divergence at aligned index {index} "
            f"(ref t={ref[alignment['offset'] + index].t}, "
            f"ark t={ark[args.ark_offset + index].t})"
        )
        for divergence in result["first_divergences"]:
            print(f"\n-- aligned index {divergence['index']} --")
            for key, left, right in divergence["differences"]:
                print(f"  {key}: ref={left} ark={right}")
        print("\nmismatch counts (top 20):")
        for key, count in list(result["mismatch_counts"].items())[:20]:
            print(f"  {count:6d}  {key}")

    if result["context"]:
        print("\ncontext:")
        for row in result["context"]:
            print(f"  [{row['index']}] ref {row['ref']}")
            print(f"  [{row['index']}] ark {row['ark']}")

    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as handle:
            json.dump(result, handle, indent=2)
        print(f"\nwrote {args.json_out}")

    return 0 if result["first_divergent_index"] is None else 1


if __name__ == "__main__":
    sys.exit(main())

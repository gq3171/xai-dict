#!/usr/bin/env python3
"""Offline CER smoke check for xai-dict / Qwen3 (or any hypothesis vs reference).

Usage:
  # Compare one pair
  python3 scripts/cer_smoke.py --ref "你好世界" --hyp "你好是界"

  # Batch file: each line  REF\\tHYP   or  REF|||HYP
  python3 scripts/cer_smoke.py --file samples.tsv

  # Transcribe wavs then score (needs xai-dict + model; slow)
  # samples.manifest lines:  path/to.wav\\t参考文本
  python3 scripts/cer_smoke.py --manifest samples.manifest --run-asr

CER = edit_distance(chars) / len(ref_chars). Lower is better.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


def normalize(s: str) -> str:
    s = s.strip().lower()
    # drop whitespace and common punct for Chinese CER
    s = re.sub(r"[\s\u3000，。！？、；：,.!?;:\"'“”‘’（）()【】\[\]…—\-]", "", s)
    return s


def edit_distance(a: list[str], b: list[str]) -> int:
    """Classic Levenshtein on char lists."""
    n, m = len(a), len(b)
    if n == 0:
        return m
    if m == 0:
        return n
    prev = list(range(m + 1))
    for i, ca in enumerate(a, 1):
        cur = [i] + [0] * m
        for j, cb in enumerate(b, 1):
            cost = 0 if ca == cb else 1
            cur[j] = min(
                prev[j] + 1,  # delete
                cur[j - 1] + 1,  # insert
                prev[j - 1] + cost,  # substitute
            )
        prev = cur
    return prev[m]


def cer(ref: str, hyp: str) -> tuple[float, int, int]:
    r = list(normalize(ref))
    h = list(normalize(hyp))
    if not r:
        return (0.0 if not h else 1.0), edit_distance(r, h), 0
    d = edit_distance(r, h)
    return d / len(r), d, len(r)


def run_asr_wav(wav: Path) -> str:
    """Best-effort: use sherpa-onnx-offline if available, else fail clearly."""
    # Prefer a tiny helper if user wires xai-dict later; for now use offline CLI.
    for bin_ in ("sherpa-onnx-offline",):
        if shutil_which(bin_):
            # Too model-specific; just document.
            break
    raise SystemExit(
        " --run-asr needs custom wiring to your model CLI.\n"
        " Prefer offline: generate hyp yourself, then:\n"
        "   python3 scripts/cer_smoke.py --ref '…' --hyp '…'\n"
        f" (wav was: {wav})"
    )


def shutil_which(name: str) -> str | None:
    from shutil import which

    return which(name)


def main() -> int:
    ap = argparse.ArgumentParser(description="CER smoke for Chinese ASR")
    ap.add_argument("--ref", help="reference text")
    ap.add_argument("--hyp", help="hypothesis text")
    ap.add_argument("--file", type=Path, help="TSV/||| pairs")
    ap.add_argument("--manifest", type=Path, help="wav\\tref lines")
    ap.add_argument("--run-asr", action="store_true", help="transcribe wavs (stub)")
    args = ap.parse_args()

    pairs: list[tuple[str, str]] = []

    if args.ref is not None and args.hyp is not None:
        pairs.append((args.ref, args.hyp))

    if args.file:
        for line in args.file.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            if "|||" in line:
                ref, hyp = line.split("|||", 1)
            elif "\t" in line:
                ref, hyp = line.split("\t", 1)
            else:
                print(f"skip bad line: {line}", file=sys.stderr)
                continue
            pairs.append((ref, hyp))

    if args.manifest:
        for line in args.manifest.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            if "\t" not in line:
                print(f"skip: {line}", file=sys.stderr)
                continue
            wav_s, ref = line.split("\t", 1)
            wav = Path(wav_s)
            if args.run_asr:
                hyp = run_asr_wav(wav)
            else:
                print(
                    f"manifest entry {wav} needs --run-asr or precomputed hyp",
                    file=sys.stderr,
                )
                continue
            pairs.append((ref, hyp))

    if not pairs:
        ap.print_help()
        print("\nExample: python3 scripts/cer_smoke.py --ref '测试一下' --hyp '测试下'")
        return 2

    total_d = 0
    total_n = 0
    print(f"{'CER':>8}  {'d':>4}  {'n':>4}  ref → hyp")
    for ref, hyp in pairs:
        rate, d, n = cer(ref, hyp)
        total_d += d
        total_n += n
        print(f"{rate:8.3f}  {d:4d}  {n:4d}  {normalize(ref)!r} → {normalize(hyp)!r}")

    if total_n:
        overall = total_d / total_n
        print(f"\noverall CER: {overall:.3f}  ({total_d}/{total_n} edits)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

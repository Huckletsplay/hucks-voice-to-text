#!/usr/bin/env python3
"""Split docs/icon/mark.svg into the parts the UI animates.

The mark is one compound path with fill-rule="evenodd": the arcs and greeked words are holes
punched in the solid H. The UI therefore draws the H and masks those shapes away, so animating
them never distorts the letterform - which docs/icon-spec.md requires stay intact.
"""
import re, json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
d = re.search(r'\sd="([^"]+)"', (ROOT / "docs/icon/mark.svg").read_text()).group(1)
subs = ["M" + s for s in d.split("M") if s.strip()]
body, arcs, words = subs[0], subs[1:4], subs[4:]
(ROOT / "app/desktop/ui/mark.js").write_text(
    "// GENERATED from docs/icon/mark.svg by app/scripts/split_mark.py - do not hand-edit.\n"
    "// The arcs and words are knocked OUT of the H (fill-rule evenodd in the source), so the\n"
    "// UI draws a solid H and masks these shapes away. Animating a mask shape makes that part\n"
    "// of the mark appear or vanish without disturbing the letterform.\n"
    f"export const H_BODY = {json.dumps(body)};\n"
    f"export const ARCS = {json.dumps(arcs, indent=2)};\n"
    f"export const WORDS = {json.dumps(words, indent=2)};\n"
)
print("wrote app/desktop/ui/mark.js")

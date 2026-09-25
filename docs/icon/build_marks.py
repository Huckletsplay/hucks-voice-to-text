#!/usr/bin/env python3
"""Build Huck's Voice to Text marks. Run: python3 build_marks.py

Lives in the project on purpose. An earlier version of this generator was
written in a scratch directory and vanished between sessions, leaving finished
SVGs no one could regenerate. The marks are OUTPUT; this file is the source.

THE H NEVER CHANGES. It is identical in every Huck's program. Only the interior
varies, and the interior is knocked out so the background shows through.

Three rules, each learned by wasting a review round:
  1. ONE path with fill-rule="evenodd". Never a <mask>: a mask inside a <symbol>
     referenced by <use> silently fails to render in WebKit, and six different
     marks all painted as a plain H for three rounds running.
  2. Strokes must NOT overlap. Overlapping subpaths under evenodd cancel back to
     filled and speckle every junction.
  3. Every stroke is clipped to the letter's own mass. A hole lying outside the
     outer path renders SOLID.
Also: emit straight runs as plain rectangles. Sampling them like curves produced
24 KB of path data per mark against under 2 KB, for an identical render.
"""
import math, pathlib

H = "M6 6H29V27H35V6H58V58H35V37H29V58H6Z"

def band(x):
    """Vertical extent the interior may occupy at this x, inset from the edges.
    Between the posts the letter has mass only in the 10-unit crossbar - which
    is why exactly ONE strand can ever cross the middle, in any design."""
    if 9 <= x < 26:   return (9.0, 55.0)
    if 26 <= x < 38:  return (29.0, 35.0)
    if 38 <= x <= 55: return (9.0, 55.0)
    return None

def f(v):
    s = f"{v:.2f}".rstrip("0").rstrip(".")
    return s if s and s != "-0" else "0"

def line(y, x0, x1, w):
    out, run, N = "", None, 240
    for i in range(N + 1):
        x = x0 + (x1 - x0) * i / N
        b = band(x)
        ok = b and (y - w/2) >= b[0] and (y + w/2) <= b[1]
        if ok and run is None: run = x
        elif not ok and run is not None:
            out += f"M{f(run)} {f(y-w/2)}H{f(x)}V{f(y+w/2)}H{f(run)}Z"; run = None
    if run is not None:
        out += f"M{f(run)} {f(y-w/2)}H{f(x1)}V{f(y+w/2)}H{f(run)}Z"
    return out

def words(y, widths, w, x0=39.5, x1=55, gap=1.3):
    """A line of writing, greeked: short blocks of UNEVEN width with gaps.

    This is the difference between a mark that says 'text' and one that says
    'rule'. A solid line never reads as a sentence at any size; blocks of
    varying length separated by spaces do, because that is the shape writing
    makes once it is too small to read."""
    out, x = "", x0
    for wd in widths:
        if x + wd > x1: break
        b = band(x)
        if b and (y - w/2) >= b[0] and (y + w/2) <= b[1]:
            out += f"M{f(x)} {f(y-w/2)}H{f(x+wd)}V{f(y+w/2)}H{f(x)}Z"
        x += wd + gap
    return out

def arc(cx, cy, r, half_span, w):
    """Concentric arc opening rightward - the broadcast reading. Struck from a
    centre OUTSIDE the letter so the post clips it, which is what makes the
    waves look like they arrive from somewhere rather than just sitting there."""
    steps = max(14, int(math.radians(2 * half_span) * r / 1.1))
    P = [(cx + r*math.cos(a), cy + r*math.sin(a)) for a in
         (math.radians(-half_span + 2*half_span*i/steps) for i in range(steps + 1))]
    runs, cur = [], []
    for (x, y) in P:
        b = band(x)
        if b and (y - w/2) >= b[0] and (y + w/2) <= b[1]: cur.append((x, y))
        elif cur: runs.append(cur); cur = []
    if cur: runs.append(cur)
    out = ""
    for run in runs:
        if len(run) < 3: continue
        L, R = [], []
        for j, (x, y) in enumerate(run):
            px, py = run[max(j-1, 0)]; nx, ny = run[min(j+1, len(run)-1)]
            dx, dy = nx - px, ny - py; d = math.hypot(dx, dy) or 1
            ox, oy = -dy/d*w/2, dx/d*w/2
            L.append((x+ox, y+oy)); R.append((x-ox, y-oy))
        out += "M" + "L".join(f"{f(a)} {f(b)}" for a, b in L + R[::-1]) + "Z"
    return out

def broadcast(radii, spans, w_arc, bar, text_lines, w_txt, cx=7, gap=1.3):
    """bar: (x0, x1) to cut a connecting strand through the crossbar, or None to
    LEAVE THE CROSSBAR SOLID - which is the locked choice. Removing the strand
    separates the waves from the words instead of drawing a pipe between them,
    and it keeps the one piece of the letter that joins the two posts intact.
    The story still reads: sound on one side, writing on the other.

    text_lines: (y, widths) or (y, widths, weight) - a per-line weight lets a
    line THIN as well as shorten, which is how a knockout does 'fading'. There
    is only one colour available; the only levers are size and density."""
    parts  = [arc(cx, 32, r, s, w_arc) for r, s in zip(radii, spans)]
    if bar: parts += [line(32, bar[0], bar[1], w_arc)]
    for row in text_lines:
        y, ws = row[0], row[1]
        parts.append(words(y, ws, row[2] if len(row) > 2 else w_txt, gap=gap))
    return H + "".join(parts)

ARCS = ([9, 14, 19], [70, 60, 46])          # the locked arc set

MARKS = {
 "broadcast": broadcast(*ARCS, 2.2, None,
     [(25, [5, 3.5, 4.5]), (32, [4, 5, 2.5]), (39, [3.5, 2])], 2.0),
 "broadcast-fine": broadcast(*ARCS, 2.0, None,
     [(23, [4, 3, 5]), (28.5, [3, 5, 3.5]), (34, [5, 2.5, 4]), (39.5, [3, 2])], 1.7, gap=1.2),
 "broadcast-bold": broadcast(*ARCS, 2.6, None,
     [(25, [5, 3.5, 4.5]), (32, [4, 5, 3]), (39, [4, 2.5])], 2.5, gap=1.5),
 "broadcast-wide": broadcast(*ARCS, 2.2, None,
     [(25, [5, 4]), (32, [4, 5.5]), (39, [4.5, 2.5])], 2.1, gap=2.0),
 "broadcast-fade": broadcast(*ARCS, 2.2, None,
     [(24.5, [5, 3.5, 4.5], 2.2), (31.5, [4, 5, 3], 1.9),
      (38, [3.5, 2.5], 1.6), (43.5, [2.5, 1.5], 1.3)], 2.0, gap=1.3),
}

NOTES = {
 "broadcast": "words on three lines, the last one trailing off",
 "broadcast-fine": "four lines of smaller words - the most text-like, the least robust",
 "broadcast-bold": "heavier words, best survival as the mark shrinks",
 "broadcast-wide": "fewer words, wider spaces, so the gaps read as spaces",
 "broadcast-fade": "words that shrink AND thin as they go - the text trailing off",
}

CHOSEN = "broadcast"   # decided 2026-09-10 - see ../icon-spec.md

def write():
    here = pathlib.Path(__file__).parent / "candidates"
    here.mkdir(exist_ok=True)
    for old in here.glob("*.svg"):
        if not old.name.startswith("._"): old.unlink()
    for key, d in MARKS.items():
        (here / f"huck-voice-{key}.svg").write_text(
f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64" role="img" aria-label="Huck's mark, {key}">
  <!-- GENERATED by ../build_marks.py - edit that, not this file.

       Huck's Voice to Text: {NOTES[key]}.
       Curved waves radiate in from the left, narrow to one strand through the
       crossbar because that is all the letter allows, and land as words.

       The H is identical in every Huck's program: {H}
       Only the interior varies. -->
  <path fill="currentColor" fill-rule="evenodd" d="{d}"/>
</svg>
''')
    # The chosen mark is promoted out of candidates/ - candidates are options,
    # docs/icon/mark.svg is the product's actual mark.
    root = pathlib.Path(__file__).parent
    (root / "mark.svg").write_text(
f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64" role="img" aria-label="Huck\'s Voice to Text">
  <!-- GENERATED by build_marks.py from MARKS["{CHOSEN}"]. Edit the script.

       THE MARK for Huck\'s Voice to Text. Chosen 2026-09-10.
       Curved arcs radiate in from the left; words on the right; the crossbar
       left SOLID between them. Separation carries the meaning - an earlier
       version cut a connecting strand through the bar and it both weakened the
       letter and explained something the composition already said.

       32 px and up only. Below 32 the word gaps close and the interior clogs:
       use h-solid.svg at 20-24 and h-16.svg at 16.

       The H is identical in every Huck\'s program: {H} -->
  <path fill="currentColor" fill-rule="evenodd" d="{MARKS[CHOSEN]}"/>
</svg>
''')
    print(f"wrote {len(MARKS)} candidates to {here}")
    for k, v in MARKS.items():
        star = "  <- CHOSEN" if k == CHOSEN else ""
        print(f"  {k:22} {len(v):>5} chars{star}")
    print(f"wrote the chosen mark to {root / 'mark.svg'}")

if __name__ == "__main__":
    write()

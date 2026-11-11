#!/usr/bin/env python3
"""Draws ../images/batching-explainer.svg: why one query for all waiting requests beats one query each."""
from pathlib import Path

HERE = Path(__file__).resolve().parent
IMAGES = HERE.parent / "images"
SURFACE, INK, INK2, MUTED, GRID = "#fcfcfb", "#0b0b0b", "#52514c", "#898781", "#e4e3df"
GREY, BLUE, GREEN = "#c3c2bb", "#2a78d6", "#2e9e5b"
FILL_A, FILL_B, FILL_OK = "#f1f0ec", "#eaf2fc", "#e6f4ea"
FONT = "font-family='-apple-system, Segoe UI, Helvetica, Arial, sans-serif'"
MONO = "font-family='SF Mono, Menlo, Consolas, monospace'"

W, H = 1200, 675
KEYS = ["u17", "u42", "u99", "u05", "u63", "u28"]
NAMES = ["Ada", "Ben", "Cleo", "Dev", "Eli", "Fay"]
LEG, PG, FLY, HOP = 1.0, 0.3, 0.6, 0.3   # seconds: wire one way, Postgres, back to caller, queue to wire
WIRE_X0, WIRE_X1, BOX_X, ANSWER_X = 580, 1040, 280, 222
T = 12.0                                  # loop length

css, out = [], []


def text(x, y, s, size=13, fill=INK, anchor="start", weight="normal", extra=""):
    return (f"<text x='{x:.1f}' y='{y:.1f}' font-size='{size}' fill='{fill}' "
            f"text-anchor='{anchor}' font-weight='{weight}' {extra}>{s}</text>")


def keyframes(frames):
    """frames: (t, opacity, x, y); hidden before the first and after the last."""
    t0, _, x0, y0 = frames[0]
    tn, _, xn, yn = frames[-1]
    frames = [(0, 0, x0, y0)] + list(frames) + [(T, 0, xn, yn)]
    name = f"k{len(css)}"
    steps = "".join(f"{min(max(t / T * 100, 0), 100):.3f}%{{opacity:{o};transform:translate({x:.1f}px,{y:.1f}px)}}"
                    for t, o, x, y in frames)
    css.append(f"@keyframes {name}{{{steps}}}.{name}{{animation:{name} {T}s ease-in-out infinite}}")
    return name


def travel(body, pts):
    """Moves `body` (drawn at the origin) through (t, x, y) points, fading in before and out after."""
    t0, x0, y0 = pts[0]
    tn, xn, yn = pts[-1]
    frames = [(t0 - 0.15, 0, x0, y0)] + [(t, 1, x, y) for t, x, y in pts] + [(tn + 0.15, 0, xn, yn)]
    out.append(f"<g class='{keyframes(frames)}' opacity='0'>{body}</g>")


def show(body, t_on, t_off=T - 0.3):
    travel(body, [(t_on, 0, 0), (t_off, 0, 0)])


def chip(label, fill, stroke, w=None, size=13, color=INK):
    w = w or 14 + 8 * len(label)
    return (f"<rect x='0' y='-11' width='{w}' height='22' rx='5' fill='{fill}' stroke='{stroke}' stroke-width='1.2'/>"
            f"<text x='{w / 2:.1f}' y='4.5' font-size='{size}' fill='{color}' text-anchor='middle' {MONO}>{label}</text>"), w


def cylinder(cx, y, w, h, fill, stroke):
    rx, ry = w / 2, 9
    return (f"<path d='M{cx - rx},{y} v{h} a{rx},{ry} 0 0 0 {w},0 v{-h}' fill='{fill}' stroke='{stroke}' stroke-width='1.5'/>"
            f"<ellipse cx='{cx}' cy='{y}' rx='{rx}' ry='{ry}' fill='{fill}' stroke='{stroke}' stroke-width='1.5'/>")


def counter(oy, events, color):
    """events: (t, +round trips, +answered)."""
    states, trips, done = [(0.3, 0, 0)], 0, 0
    for t, dt, dd in sorted(events):
        trips, done = trips + dt, done + dd
        states.append((t, trips, done))
    for i, (t, trips, done) in enumerate(states):
        t_off = states[i + 1][0] if i + 1 < len(states) else T - 0.3
        show(text(1160, oy + 24, f"round trips: {trips} · answered: {done} / 6", 15, color, "end", "600"), t, t_off)


def panel(oy, title, color, fill, stroke, box_label, pg_label):
    rows = [oy + 62 + i * 28 for i in range(6)]
    lanes = [oy + 100, oy + 150]
    out.append(text(40, oy + 24, title, 19, color, weight="600"))
    for key, y in zip(KEYS, rows):
        body, _ = chip(f"get_user('{key}')", "#ffffff", stroke, 150, 12)
        out.append(f"<g transform='translate(40,{y})'>{body}</g>")
        out.append(f"<circle cx='208' cy='{y}' r='6' fill='none' stroke='{GREY}' stroke-width='1.5'/>")
    out.append(f"<rect x='{BOX_X}' y='{oy + 40}' width='280' height='196' rx='8' fill='{fill}' stroke='{stroke}' stroke-width='1.2'/>")
    out.append(text(BOX_X + 140, oy + 228, box_label, 12, INK2, "middle"))
    for j, y in enumerate(lanes):
        out.append(f"<line x1='{WIRE_X0}' x2='{WIRE_X1}' y1='{y}' y2='{y}' stroke='{GRID}' stroke-width='3'/>")
        out.append(text(WIRE_X0, y - 16, f"connection {j + 1}", 11, MUTED))
    out.append(cylinder(1090, oy + 72, 100, 110, fill, stroke))
    out.append(text(1090, oy + 132, "Postgres", 14, INK, "middle", "600"))
    out.append(text(1090, oy + 228, pg_label, 12, INK2, "middle"))
    return rows, lanes


def flash(oy, t, color):
    show(f"<rect x='1040' y='{oy + 72}' width='100' height='110' fill='{color}' opacity='0.18'/>", t, t + PG)


def check(x, y):
    return (f"<circle cx='{x}' cy='{y}' r='6' fill='{GREEN}'/>"
            f"<path d='M{x - 3},{y} l2,2.5 l4,-5' fill='none' stroke='#fff' stroke-width='1.6'/>")


def one_query_each(oy):
    rows, lanes = panel(oy, "One query per request", INK2, FILL_A, GREY,
                        "queue: waiting for a free connection", "every query: 1 row")
    start = [0.9 + r * (HOP + 2 * LEG + PG) for r in range(4)]
    events = []
    for i, (key, name, y) in enumerate(zip(KEYS, NAMES, rows)):
        r, lane = divmod(i, 2)
        s = start[r]
        body, w = chip(key, FILL_A, GREY)
        travel(body, [(0.3, 200, y), (0.9, BOX_X + 20, y), (s, BOX_X + 20, y), (s + HOP, WIRE_X0, lanes[lane]),
                      (s + HOP + LEG, WIRE_X1 - w, lanes[lane])])
        back = start[r + 1]
        body, w = chip(name, FILL_OK, GREEN)
        travel(body, [(back - LEG, WIRE_X1 - w, lanes[lane]), (back, WIRE_X0, lanes[lane]),
                      (back + FLY, ANSWER_X, y), (T - 0.3, ANSWER_X, y)])
        show(check(208, y), back + FLY)
        if lane == 0:
            flash(oy, s + HOP + LEG, GREY)
            events += [(s + HOP, 1, 0), (back + FLY, 0, 2)]
    counter(oy, events, INK2)
    out.append(text(40, oy + 250, "Every request is its own query and its own trip. With 2 connections, "
                                  "6 requests take 3 trips, one after another.", 14, INK2))


def batched(oy):
    rows, lanes = panel(oy, "Requests waiting at the same time share one query", BLUE, FILL_B, BLUE,
                        "batcher: takes everyone waiting right now", "this query: 6 rows")
    gather, depart = 0.9, 1.3
    for key, y in zip(KEYS, rows):
        body, _ = chip(key, FILL_B, BLUE)
        travel(body, [(0.3, 200, y), (gather, BOX_X + 20, y), (1.15, BOX_X + 20, y)])
    show(text(BOX_X + 140, oy + 108, "… WHERE path =", 13, INK, "middle", extra=MONO), 1.1)
    show(text(BOX_X + 140, oy + 130, "ANY('{" + ",".join(KEYS) + "}')", 13, BLUE, "middle", "600", MONO), 1.1)
    body, w = chip("6 keys", BLUE, BLUE, color="#ffffff")
    travel(body, [(depart, WIRE_X0, lanes[0]), (depart + LEG, WIRE_X1 - w, lanes[0])])
    flash(oy, depart + LEG, BLUE)
    back = depart + 2 * LEG + PG
    body, w = chip("6 rows", GREEN, GREEN, color="#ffffff")
    travel(body, [(back - LEG, WIRE_X1 - w, lanes[0]), (back, WIRE_X0, lanes[0])])
    for name, y in zip(NAMES, rows):
        body, _ = chip(name, FILL_OK, GREEN)
        travel(body, [(back, WIRE_X0, lanes[0]), (back + FLY, ANSWER_X, y), (T - 0.3, ANSWER_X, y)])
        show(check(208, y), back + FLY)
    counter(oy, [(depart, 1, 0), (back + FLY, 0, 6)], BLUE)
    out.append(text(40, oy + 250, "Each caller still asks for one key. The batcher sends the waiting keys "
                                  "as one query and hands each row back to its caller.", 14, INK2))


def main():
    out.extend([text(40, 44, "Why one query for many requests is faster", 26, weight="700"),
            text(40, 72, "Like a bus instead of a taxi per passenger: the trip to the database is the expensive part. "
                         "Finding one row is cheap.", 15, INK2)])
    one_query_each(88)
    out.append(f"<line x1='40' x2='1160' y1='356' y2='356' stroke='{GRID}' stroke-width='1.5'/>")
    batched(364)
    out.extend([text(40, 648, "Simplified: 6 requests, 2 connections. Measured with 4,096 clients on one laptop: "
                          "18,289 → 252,161 req/s. One request alone has nothing to share: both take ~0.3 ms.",
                 13, MUTED)])
    svg = (f"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 {W} {H}' width='{W}' height='{H}' {FONT}>"
           f"<style>{''.join(css)}</style><rect width='{W}' height='{H}' fill='{SURFACE}'/>" + "\n".join(out) + "</svg>")
    (IMAGES / "batching-explainer.svg").write_text(svg)
    print("batching-explainer", W, H, f"{T}s loop")


if __name__ == "__main__":
    main()

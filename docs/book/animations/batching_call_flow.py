#!/usr/bin/env python3
"""Draws ../images/batching-call-flow.svg: how one `users.call(key).await` is parked, batched and resumed."""
import batching_explainer as m
from batching_explainer import text, chip, cylinder, INK, INK2, MUTED, GRID, GREY, BLUE, GREEN, FILL_A, FILL_B, FILL_OK, MONO, FONT

T = m.T = 16.0
END = T - 0.3
AMBER = "#b86e00"
W, H = 1200, 675
out = m.out

CARD_X, CARD_W = 40, 430
LINE_Y = [150, 172, 194, 216, 238]          # handler #1 code lines
SMALL_Y = {2: 314, 3: 374}                  # one-line handlers #2, #3
BATCH_X, BATCH_W = 730, 430
BATCH_Y = [150, 172, 194, 216, 238]
WIRE_Y, PG_X = 400, 1050
SLOT_Y = [318, 346, 374]                    # reply slot rows

STEPS = [
    (0.3, "A request arrives and its handler runs like any other function.",
     "It needs one user, so it calls users.call(key)."),
    (1.5, "call() registers a reply slot under a new id, then puts only the key and the id in the queue.",
     ".await parks the handler on its slot. No thread is blocked: it runs #2 and #3, which do the same."),
    (3.5, "A separate batcher task takes every call that is waiting right now: three keys.",
     "It does not wait for more. Whoever is in the queue at this moment goes into the batch."),
    (5.0, "One query for all three keys: … WHERE path = ANY('{u17,u42,u99}').",
     "Postgres answers with three rows in one round trip instead of three."),
    (8.3, "Each row is matched to its call by key and sent to the reply slot with that call's id.",
     "Sending into the slot wakes the handler that is parked on it. The slot is used up."),
    (10.6, "The handler continues right after .await, with its own row in user.",
     "From its point of view, users.call(key) simply returned Ada."),
]


def swap(body, t_on, t_off):
    """Like show, but fades within [t_on, t_off] so consecutive texts never overlap."""
    m.show(body, t_on + 0.15, t_off - 0.15)


def highlight(x, w, y, color, t_on, t_off):
    m.show(f"<rect x='{x + 6}' y='{y - 15}' width='{w - 12}' height='21' rx='4' fill='{color}' opacity='0.16'/>", t_on, t_off)


def status(x, y, states):
    """states: (t_on, t_off, label, color)."""
    for t_on, t_off, label, color in states:
        swap(text(x, y, label, 12, color, "end", "600"), t_on, t_off)


def code(x, ys, lines, size=13):
    for y, line in zip(ys, lines):
        out.append(f"<text x='{x}' y='{y}' font-size='{size}' fill='{INK}' {MONO} xml:space='preserve'>{line}</text>")


def handlers():
    out.append(f"<rect x='{CARD_X}' y='100' width='{CARD_W}' height='156' rx='8' fill='#ffffff' stroke='{GREY}' stroke-width='1.2'/>")
    out.append(text(CARD_X + 14, 122, "handler #1 · request for u17", 13, INK2, weight="600"))
    code(CARD_X + 16, LINE_Y, ["async fn handle(req) {", "    let key = req.path();",
                               "    let user = users.call(key).await;", "    render(user)", "}"])
    highlight(CARD_X, CARD_W, LINE_Y[1], INK2, 0.3, 1.5)
    highlight(CARD_X, CARD_W, LINE_Y[2], INK2, 1.5, 2.3)
    highlight(CARD_X, CARD_W, LINE_Y[2], AMBER, 2.3, 9.9)
    highlight(CARD_X, CARD_W, LINE_Y[2], GREEN, 9.9, 10.6)
    highlight(CARD_X, CARD_W, LINE_Y[3], INK2, 10.6, END)
    for n, key, (arrive, wake) in [(2, "u42", (2.6, 10.1)), (3, "u99", (2.9, 10.3))]:
        y = SMALL_Y[n]
        out.append(f"<rect x='{CARD_X}' y='{y - 42}' width='{CARD_W}' height='56' rx='8' fill='#ffffff' stroke='{GREY}' stroke-width='1.2'/>")
        out.append(text(CARD_X + 14, y - 20, f"handler #{n} · request for {key}", 13, INK2, weight="600"))
        code(CARD_X + 16, [y], [f"let user = users.call(\"{key}\").await;"], 12)
        highlight(CARD_X, CARD_W, y, AMBER, arrive, wake)
        status(CARD_X + CARD_W - 12, y - 20, [(0.3, arrive, "running", INK2), (arrive, wake, "parked at .await", AMBER),
                                             (wake, END, "running", GREEN)])
    status(CARD_X + CARD_W - 12, 122, [(0.3, 2.3, "running", INK2), (2.3, 9.9, "parked at .await", AMBER),
                                        (9.9, END, "running", GREEN)])


def queue_and_batcher():
    out.append(f"<rect x='500' y='100' width='200' height='150' rx='8' fill='{FILL_A}' stroke='{GREY}' stroke-width='1.2'/>")
    out.append(text(600, 122, "queue: key + id", 13, INK2, "middle", "600"))
    out.append(f"<rect x='500' y='266' width='200' height='134' rx='8' fill='#ffffff' stroke='{AMBER}' stroke-width='1.2'/>")
    out.append(text(600, 288, "reply slots", 13, AMBER, "middle", "600"))
    for i, (t, wake) in enumerate([(1.6, 9.9), (1.9, 10.1), (2.2, 10.3)]):
        m.show(text(514, SLOT_Y[i], f"id {i + 1} → handler #{i + 1}", 12, INK, extra=MONO), t + 0.1, wake + 0.2)
    out.append(f"<rect x='{BATCH_X}' y='100' width='{BATCH_W}' height='156' rx='8' fill='{FILL_B}' stroke='{BLUE}' stroke-width='1.2'/>")
    out.append(text(BATCH_X + 14, 122, "batcher task", 13, BLUE, weight="600"))
    code(BATCH_X + 16, BATCH_Y, ["loop {", "    let batch = queue.take_all_waiting();",
                                 "    let rows = sql(\"… = ANY($1)\", batch.keys()).await;",
                                 "    for (id, row) in pair(batch, rows) { send(id, row) }", "}"], 12)
    highlight(BATCH_X, BATCH_W, BATCH_Y[1], BLUE, 3.5, 5.0)
    highlight(BATCH_X, BATCH_W, BATCH_Y[2], BLUE, 5.0, 8.3)
    highlight(BATCH_X, BATCH_W, BATCH_Y[3], GREEN, 8.3, 10.6)
    status(BATCH_X + BATCH_W - 12, 122, [(0.3, 3.5, "waiting for calls", MUTED), (3.5, 10.6, "running", BLUE),
                                          (10.6, END, "waiting for calls", MUTED)])
    m.show(text(BATCH_X + 16, 284, "batch = [u17 (id 1), u42 (id 2), u99 (id 3)]", 13, BLUE, weight="600", extra=MONO), 4.3, 10.6)

    sources = [(1.6, 300, LINE_Y[2], 1), (1.9, 330, SMALL_Y[2], 2), (2.2, 330, SMALL_Y[3], 3)]
    for i, (t, x, y, n) in enumerate(sources):
        key = ["u17", "u42", "u99"][i]
        body, _ = chip(f"{key} · id {n}", "#ffffff", GREY)
        slot = (548, 155 + i * 35)
        travel_in = 3.7 + i * 0.15
        m.travel(body, [(t, x, y), (t + 0.7, *slot), (travel_in, *slot), (travel_in + 0.6, BATCH_X + 16 + i * 40, 284)])


def database():
    out.append(f"<line x1='{BATCH_X}' x2='{PG_X}' y1='{WIRE_Y}' y2='{WIRE_Y}' stroke='{GRID}' stroke-width='3'/>")
    out.append(cylinder(PG_X + 55, 360, 110, 80, FILL_B, BLUE))
    out.append(text(PG_X + 55, 410, "Postgres", 14, INK, "middle", "600"))
    out.append(text(BATCH_X, WIRE_Y - 14, "1 round trip", 11, MUTED))
    body, w = chip("3 keys", BLUE, BLUE, color="#ffffff")
    m.travel(body, [(5.2, BATCH_X, WIRE_Y), (6.2, PG_X - w, WIRE_Y)])
    m.show(f"<rect x='{PG_X}' y='360' width='110' height='80' fill='{BLUE}' opacity='0.18'/>", 6.2, 6.5)
    body, w = chip("3 rows", GREEN, GREEN, color="#ffffff")
    m.travel(body, [(6.5, PG_X - w, WIRE_Y), (7.5, BATCH_X, WIRE_Y), (8.3, BATCH_X, WIRE_Y)])
    targets = [(8.5, "Ada", 356, LINE_Y[2]), (8.7, "Ben", 330, SMALL_Y[2]), (8.9, "Cleo", 330, SMALL_Y[3])]
    for t, name, x, y in targets:
        body, _ = chip(name, FILL_OK, GREEN)
        i = ["Ada", "Ben", "Cleo"].index(name)
        m.travel(body, [(t, BATCH_X, WIRE_Y), (t + 0.6, 646, SLOT_Y[i]), (t + 0.8, 646, SLOT_Y[i]),
                        (t + 1.4, x, y), (END, x, y)])


def captions():
    for i, (t, line1, line2) in enumerate(STEPS):
        t_off = STEPS[i + 1][0] if i + 1 < len(STEPS) else END
        swap(text(40, 492, f"Step {i + 1} of {len(STEPS)}", 13, BLUE, weight="700"), t, t_off)
        swap(text(40, 524, line1, 18, INK, weight="600"), t, t_off)
        swap(text(40, 554, line2, 15, INK2), t, t_off)


def main():
    out.extend([text(40, 44, "What happens inside users.call(key).await", 26, weight="700"),
                text(40, 72, "Each request keeps its own simple function. A separate task answers all waiting calls "
                             "with one query.", 15, INK2)])
    handlers()
    queue_and_batcher()
    database()
    captions()
    out.append(f"<line x1='40' x2='1160' y1='456' y2='456' stroke='{GRID}' stroke-width='1.5'/>")
    out.append(text(40, 640, "Simplified pseudocode. In mq-bridge, Caller::call is the handler side; "
                             "the batcher is a route with a lookup middleware.", 13, MUTED))
    svg = (f"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 {W} {H}' width='{W}' height='{H}' {FONT}>"
           f"<style>{''.join(m.css)}</style><rect width='{W}' height='{H}' fill='#fcfcfb'/>" + "\n".join(out) + "</svg>")
    (m.IMAGES / "batching-call-flow.svg").write_text(svg)
    print("batching-call-flow", W, H, f"{T}s loop")


if __name__ == "__main__":
    main()

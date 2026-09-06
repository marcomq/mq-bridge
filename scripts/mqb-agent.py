#!/usr/bin/env python3
"""Zero-dependency mailbox for LLM agents sharing one machine.

Each agent owns a directory under $MQB_AGENTS_DIR (default ~/.mqb-agents). A message
is one atomically-renamed `.bin` file, which is exactly the on-disk format of a
mq-bridge `dir_spool` endpoint with `metadata_extension: ""` — so an agent holding the
mq-bridge MCP can tail the same inbox with `start_route` and bridge it anywhere.
Run `mqb-agent.py route` for that route JSON.
"""

import argparse
import json
import os
import pathlib
import sys
import time
import uuid

PAYLOAD_SUFFIX = ".bin"
STAGING_SUFFIX = ".tmp"
PROFILE = "profile.json"
# dir_spool's own lock/sentinel files, never messages.
CONTROL = {"DONE", "PRODUCER", "CONSUMER", PROFILE}


def root() -> pathlib.Path:
    return pathlib.Path(os.environ.get("MQB_AGENTS_DIR", "~/.mqb-agents")).expanduser()


def inbox(name: str) -> pathlib.Path:
    """One agent name is one directory name under the root.

    Mirrors `validate_agent_name` in mcp.rs: both sides share these mailboxes, so a
    name this accepts and the Rust side refuses would be a hole in the Rust check.
    `:` goes with the separators because every Windows path prefix is built from one
    of the three, and joining a prefix restarts from a new root.
    """
    trimmed = (name or "").strip()
    if (
        not trimmed
        or any(c in trimmed for c in "/\\:")
        or trimmed.startswith(".")
        or trimmed in CONTROL
    ):
        die(f"invalid agent name: {name!r}")
    return root() / trimmed


def die(msg: str) -> None:
    print(f"mqb-agent: {msg}", file=sys.stderr)
    raise SystemExit(1)


def whoami(explicit: str | None) -> str:
    name = explicit or os.environ.get("MQB_AGENT")
    if name:
        return name
    known = sorted(p.name for p in root().glob("*") if p.is_dir())
    die(f"no agent name: pass --as or set MQB_AGENT (registered: {known or 'none'})")


def chunks(box: pathlib.Path) -> list[pathlib.Path]:
    if not box.is_dir():
        return []
    return sorted(p for p in box.iterdir() if p.name.endswith(PAYLOAD_SUFFIX))


def cmd_register(args) -> None:
    box = inbox(whoami(args.name))
    box.mkdir(parents=True, exist_ok=True)
    profile = {"name": box.name, "description": args.desc or "", "registered": now_ms()}
    (box / PROFILE).write_text(json.dumps(profile, indent=2))
    print(f"registered {box.name} at {box}")


def now_ms() -> int:
    return int(time.time() * 1000)


def next_seq(box: pathlib.Path) -> int:
    """One past the highest sequence in the directory, matching dir_spool's own
    resume rule. Two writers can pick the same number; the id suffix keeps both."""
    highest = 0
    for path in box.glob("*" + PAYLOAD_SUFFIX):
        head = path.name.split("-", 1)[0]
        if head.isdigit():
            highest = max(highest, int(head))
    return highest + 1


def cmd_send(args) -> None:
    text = args.text if args.text is not None else sys.stdin.read()
    box = inbox(args.to)
    fresh = not box.is_dir()
    box.mkdir(parents=True, exist_ok=True)
    envelope = {
        "id": uuid.uuid4().hex,
        # `unnamed` is the sentinel mcp.rs writes, so a reader filtering on
        # it catches senders from either side.
        "from": args.sender or os.environ.get("MQB_AGENT") or "unnamed",
        "to": box.name,
        "ts": now_ms(),
        "message": text,
    }
    # dir_spool orders a queue lexically and requires a leading zero-padded
    # sequence; the id suffix keeps concurrent writers from colliding on one.
    stem = f"{next_seq(box):09d}-{envelope['id'][:12]}"
    final = box / (stem + PAYLOAD_SUFFIX)
    staging = box / (stem + PAYLOAD_SUFFIX + STAGING_SUFFIX)
    data = json.dumps(envelope).encode()
    with open(staging, "wb") as fh:
        fh.write(data)
        fh.flush()
        os.fsync(fh.fileno())
    os.replace(staging, final)
    if fresh:
        print(f"note: {box.name} had no inbox; created one", file=sys.stderr)
    print(json.dumps({"sent": envelope["id"], "to": box.name, "bytes": len(data)}))


def read_chunks(box: pathlib.Path, limit: int | None, drain: bool) -> list[dict]:
    out = []
    for path in chunks(box)[:limit]:
        try:
            raw = path.read_bytes()
        except FileNotFoundError:
            continue  # another reader won the race
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            msg = {"raw": raw.decode(errors="replace")}
        msg["_file"] = path.name
        out.append(msg)
        if drain:
            path.unlink(missing_ok=True)
    return out


def cmd_recv(args) -> None:
    box = inbox(whoami(args.name))
    print(json.dumps(read_chunks(box, args.limit, drain=True), indent=2))


def cmd_peek(args) -> None:
    box = inbox(whoami(args.name))
    print(json.dumps(read_chunks(box, args.limit, drain=False), indent=2))


def listener_live(box: pathlib.Path) -> bool:
    """Whether a listener is actually running on this inbox.

    A crashed listener leaves its CONSUMER lock behind, so the owner is the question,
    not the file. `os.kill(pid, 0)` is POSIX-only: on Windows it terminates the target
    rather than probing it, so there the file is the best answer available.
    """
    try:
        pid = int((box / "CONSUMER").read_text().strip())
    except (OSError, ValueError):
        return False
    if pid <= 0:
        return False
    if os.name == "nt":
        return True
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # running, owned by another user
    except OSError:
        return False
    return True


def cmd_who(args) -> None:
    base = root()
    if not base.is_dir():
        print(json.dumps([]))
        return
    agents = []
    for box in sorted(p for p in base.iterdir() if p.is_dir()):
        desc = ""
        try:
            desc = json.loads((box / PROFILE).read_text()).get("description", "")
        except (OSError, json.JSONDecodeError):
            pass
        agents.append(
            {
                "name": box.name,
                "description": desc,
                "pending": len(chunks(box)),
                "listener_live": listener_live(box),
            }
        )
    print(json.dumps(agents, indent=2))


def cmd_route(args) -> None:
    box = inbox(whoami(args.name))
    print(
        json.dumps(
            {
                "name": f"inbox-{box.name}",
                "route": {
                    "input": {
                        "dir_spool": {"path": str(box), "metadata_extension": ""}
                    },
                    "output": {"null": None},
                },
                "capture_last": 50,
            },
            indent=2,
        )
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="cmd", required=True)

    reg = sub.add_parser("register", help="create this agent's inbox")
    reg.add_argument("name", nargs="?")
    reg.add_argument("--desc", help="what this agent is for")
    reg.set_defaults(func=cmd_register)

    send = sub.add_parser("send", help="deliver a message to another agent")
    send.add_argument("to")
    send.add_argument("text", nargs="?", help="message text; read from stdin if omitted")
    send.add_argument("--from", dest="sender", help="sender name (default $MQB_AGENT)")
    send.set_defaults(func=cmd_send)

    for name, fn, helptext in (
        ("recv", cmd_recv, "read pending messages and delete them"),
        ("peek", cmd_peek, "read pending messages without deleting"),
    ):
        p = sub.add_parser(name, help=helptext)
        p.add_argument("--as", dest="name", help="agent name (default $MQB_AGENT)")
        p.add_argument("--limit", type=int)
        p.set_defaults(func=fn)

    who = sub.add_parser("who", help="list agents, pending counts and live listeners")
    who.set_defaults(func=cmd_who)

    route = sub.add_parser("route", help="print start_route JSON tailing this inbox")
    route.add_argument("--as", dest="name", help="agent name (default $MQB_AGENT)")
    route.set_defaults(func=cmd_route)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()

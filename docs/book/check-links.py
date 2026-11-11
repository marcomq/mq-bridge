#!/usr/bin/env python3
"""Fails on broken internal links in the built book. Run after `mdbook build docs/book`.

Checks every relative link and #anchor inside page content. A link to a redirect page
(see [output.html.redirect] in book.toml) is followed to its target.
"""
import html
import re
import sys
from pathlib import Path
from urllib.parse import unquote

OUT = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parent / "book")
SKIP = {"print.html", "toc.html", "404.html"}
REFRESH = re.compile(r'http-equiv="refresh"[^>]*url=([^"]+)"', re.I)


def resolve(target):
    """Follows redirect pages; returns the final file."""
    for _ in range(5):
        if target.is_dir():
            target = target / "index.html"
        if target.suffix != ".html" or not target.exists():
            return target
        m = REFRESH.search(target.read_text(errors="ignore"))
        if not m:
            return target
        target = (target.parent / unquote(html.unescape(m.group(1)).split("#")[0])).resolve()
    return target


def main():
    ids = {}
    broken = []
    pages = [p for p in OUT.rglob("*.html") if p.name not in SKIP and "<main>" in p.read_text(errors="ignore")]
    for page in pages:
        body = re.search(r"<main>(.*)</main>", page.read_text(errors="ignore"), re.S).group(1)
        for href in re.findall(r'href="([^"]+)"', body):
            href = html.unescape(href)
            if re.match(r"^[a-z][a-z0-9+.-]*:", href, re.I):
                continue
            path, _, frag = href.partition("#")
            target = resolve((page.parent / unquote(path)).resolve() if path else page.resolve())
            if not target.exists():
                broken.append(f"{page.relative_to(OUT)}: {href} (no such page)")
                continue
            if frag and target.suffix == ".html":
                if target not in ids:
                    ids[target] = set(re.findall(r'\bid="([^"]+)"', target.read_text(errors="ignore")))
                if unquote(frag) not in ids[target]:
                    broken.append(f"{page.relative_to(OUT)}: {href} (no such anchor)")
    for line in sorted(set(broken)):
        print(line)
    print(f"{len(pages)} pages checked, {len(set(broken))} broken links")
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Post-build step for search engines and LLMs. Run after `mdbook build docs/book`.

Adds a canonical URL and a per-page meta description to the built HTML, and writes
sitemap.xml, llms.txt and llms-full.txt into the build directory. A page sets its
description with a first-line comment: <!-- description: ... -->
"""
import html
import re
import sys
from pathlib import Path

SITE = "https://marcomq.github.io/mq-bridge/"
SRC = Path(__file__).resolve().parent
OUT = SRC / "book"
REPO = SRC.parent.parent
DESCRIPTION = re.compile(r"<!--\s*description:\s*(.*?)\s*-->", re.S)
SKIP = {"print.html", "404.html", "toc.html"}
# mdBook also writes the first chapter as the site's index.html.
FRONT_PAGE = "introduction.html"


def chapters():
    """Markdown files in SUMMARY.md order."""
    summary = (SRC / "SUMMARY.md").read_text()
    seen = []
    for path in re.findall(r"\]\(([^)#]+\.md)\)", summary):
        if path not in seen and (SRC / path).is_file():
            seen.append(path)
    return seen


def html_path(md):
    path = Path(md).with_suffix(".html")
    return path.with_name("index.html") if path.name == "README.html" else path


def url(page):
    page = page.as_posix()
    if page == FRONT_PAGE:
        return SITE
    return SITE + (page[: -len("index.html")] if page.endswith("index.html") else page)


def main():
    if not OUT.is_dir():
        sys.exit(f"{OUT} not found: run `mdbook build` first")
    pages = chapters()
    descriptions = {}
    for md in pages:
        match = DESCRIPTION.search((SRC / md).read_text()[:2000])
        if match:
            descriptions[html_path(md)] = " ".join(match.group(1).split())

    urls = []
    for md in pages:
        page = html_path(md)
        file = OUT / page
        if not file.is_file() or page.name in SKIP:
            continue
        # mdBook renders README.md as index.html but leaves links to it as README.html.
        text = re.sub(r'href="([^":]*)README\.html', r'href="\1index.html', file.read_text())
        head = f'<link rel="canonical" href="{url(page)}">'
        if page in descriptions:
            meta = f'<meta name="description" content="{html.escape(descriptions[page])}">'
            text, replaced = re.subn(r'<meta name="description" content="[^"]*">', meta, text, count=1)
            if not replaced:
                head += "\n        " + meta
        if 'rel="canonical"' not in text:
            text = text.replace("</head>", f"    {head}\n    </head>", 1)
        file.write_text(text)
        if page.as_posix() == FRONT_PAGE:
            index = OUT / "index.html"
            index.write_text(index.read_text().replace("</head>", f"    {head}\n    </head>", 1))
            continue
        urls.append(url(page))

    sitemap = "".join(f"  <url><loc>{u}</loc></url>\n" for u in [SITE] + urls)
    (OUT / "sitemap.xml").write_text(
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        f'<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n{sitemap}</urlset>\n'
    )

    llms = (REPO / "llms.txt").read_text()
    (OUT / "llms.txt").write_text(llms)
    full = [llms.split("\n## ", 1)[0].rstrip() + "\n"]
    for md in pages:
        body = DESCRIPTION.sub("", (SRC / md).read_text()).strip()
        full.append(f"\n\n---\nSource: {url(html_path(md))}\n\n{body}\n")
    (OUT / "llms-full.txt").write_text("".join(full))
    print(f"postprocess: {len(urls)} pages, {len(descriptions)} descriptions, sitemap.xml, llms.txt, llms-full.txt")


if __name__ == "__main__":
    main()

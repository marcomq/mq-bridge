#!/usr/bin/env python3
"""Post-build step for search engines and LLMs. Run after `mdbook build docs/book`.

Adds a canonical URL, a per-page meta description and link-preview (Open Graph) tags
to the built HTML, and writes sitemap.xml, llms.txt and llms-full.txt into the build
directory. A page sets its description with a first-line comment:
<!-- description: ... -->; without one, its first paragraph is used.
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
PREVIEW_IMAGE = SITE + "images/social-preview.png"


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


def first_paragraph(markdown):
    """The first prose paragraph as plain text, cut to search-snippet length."""
    in_code = False
    for block in re.split(r"\n\s*\n", DESCRIPTION.sub("", markdown)):
        block = block.strip()
        if block.count("```") % 2:
            in_code = not in_code
            continue
        if in_code or not block or re.match(r"[#<|>!`\-*+\d\[{]", block):
            continue
        text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", block)
        text = " ".join(re.sub(r"[*`]", "", text).split())
        if len(text) < 60 or text.endswith(":"):
            continue
        if len(text) > 160:
            text = text[:159].rsplit(" ", 1)[0].rstrip(",;:—-") + "…"
        return text
    return None


def add_head(file, page, description):
    # mdBook renders README.md as index.html but leaves links to it as README.html.
    text = re.sub(r'href="([^":]*)README\.html', r'href="\1index.html', file.read_text())
    if 'rel="canonical"' in text:
        file.write_text(text)
        return
    head = [f'<link rel="canonical" href="{url(page)}">']
    if description:
        meta = f'<meta name="description" content="{html.escape(description)}">'
        text, replaced = re.subn(r'<meta name="description" content="[^"]*">', meta, text, count=1)
        if not replaced:
            head.append(meta)
    title = re.search(r"<title>(.*?)</title>", text, re.S)
    shown = re.search(r'<meta name="description" content="([^"]+)">', text + "".join(head))
    head += [
        '<meta property="og:type" content="website">',
        f'<meta property="og:url" content="{url(page)}">',
        f'<meta property="og:image" content="{PREVIEW_IMAGE}">',
        '<meta name="twitter:card" content="summary_large_image">',
    ]
    if title:
        head.append(f'<meta property="og:title" content="{title.group(1).strip()}">')
    if shown:
        head.append(f'<meta property="og:description" content="{shown.group(1)}">')
    lines = "".join(f"    {tag}\n    " for tag in head)
    file.write_text(text.replace("</head>", f"{lines}</head>", 1))


def main():
    if not OUT.is_dir():
        sys.exit(f"{OUT} not found: run `mdbook build` first")
    pages = chapters()
    descriptions = {}
    explicit = 0
    for md in pages:
        source = (SRC / md).read_text()
        match = DESCRIPTION.search(source[:2000])
        explicit += bool(match)
        descriptions[html_path(md)] = " ".join(match.group(1).split()) if match else first_paragraph(source)

    urls = []
    for md in pages:
        page = html_path(md)
        file = OUT / page
        if not file.is_file() or page.name in SKIP:
            continue
        add_head(file, page, descriptions[page])
        if page.as_posix() == FRONT_PAGE:
            add_head(OUT / "index.html", page, descriptions[page])
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
    print(f"postprocess: {len(urls)} pages, {explicit} written descriptions, sitemap.xml, llms.txt, llms-full.txt")


if __name__ == "__main__":
    main()

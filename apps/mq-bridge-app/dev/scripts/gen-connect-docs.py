#!/usr/bin/env python3
"""Generates the Connect plugin's component pages in docs/book/connectors/.

The component list and every config field come from the specs Redpanda Connect
registers in the plugin's Go bridge, so the pages describe exactly what the
plugin links. Needs a checkout of mq-bridge-connect and a Go toolchain:

    python3 apps/mq-bridge-app/dev/scripts/gen-connect-docs.py ../mq-bridge-connect

The checkout is not modified: the dump program is compiled through `go -overlay`.
Re-run after a plugin release and commit the three pages.
"""

import json
import os
import re
import subprocess
import sys
import tempfile

DOCS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "..", "docs", "book", "connectors")
UPSTREAM = "https://docs.redpanda.com/redpanda-connect/components"

SPECDUMP = """package main

import (
	"encoding/json"
	"os"

	_ "github.com/marcomq/mq-bridge-connect/go-bridge/internal/components"
	"github.com/redpanda-data/benthos/v4/public/service"
)

func main() {
	env := service.GlobalEnvironment()
	out := map[string][]json.RawMessage{}
	add := func(kind string) func(string, *service.ConfigView) {
		return func(_ string, c *service.ConfigView) {
			if b, err := c.FormatJSON(); err == nil {
				out[kind] = append(out[kind], b)
			}
		}
	}
	env.WalkInputs(add("inputs"))
	env.WalkOutputs(add("outputs"))
	env.WalkProcessors(add("processors"))
	json.NewEncoder(os.Stdout).Encode(out)
}
"""

# Processors the plugin also exports as `connect_<name>` middlewares
# (`export_connect_plugins!` in mq-bridge-connect/src/lib.rs).
MIDDLEWARES_RE = re.compile(r'^\s+\w+ => "(\w+)",$', re.M)

PAGES = {
    "inputs": (
        "connect-inputs.md",
        "Connect plugin: inputs",
        "Use one as a route `input`:",
        "input",
    ),
    "outputs": (
        "connect-outputs.md",
        "Connect plugin: outputs",
        "Use one as a route `output`:",
        "output",
    ),
    "processors": ("connect-processors.md", "Connect plugin: processors", "", ""),
}

# Upstream texts copied from a sibling component, keyed by (kind, component, field);
# a `None` field replaces the component summary.
CORRECTIONS = {
    ("processors", "aws_bedrock_embeddings", "text"): "The text you want to compute vector embeddings for.",
    ("processors", "cohere_rerank", None): "Ranks a list of documents by their relevance to a query, using the Cohere API.",
    ("processors", "ollama_moderation", None): "Classifies an LLM response as safe or unsafe, using the Ollama API.",
}


def dump_specs(repo):
    bridge = os.path.join(repo, "go-bridge")
    with tempfile.TemporaryDirectory() as tmp:
        source = os.path.join(tmp, "specdump.go")
        overlay = os.path.join(tmp, "overlay.json")
        with open(source, "w") as f:
            f.write(SPECDUMP)
        virtual = os.path.join(bridge, "internal", "specdump", "main.go")
        with open(overlay, "w") as f:
            json.dump({"Replace": {virtual: source}}, f)
        out = subprocess.run(
            ["go", "run", "-overlay", overlay, "./internal/specdump"],
            cwd=bridge, check=True, capture_output=True, text=True,
        ).stdout
    return json.loads(out)


def connect_version(repo):
    with open(os.path.join(repo, "go-bridge", "go.mod")) as f:
        match = re.search(r"redpanda-data/connect/v4 (v[\d.]+)", f.read())
    return match.group(1) if match else "unknown"


def middleware_names(repo):
    with open(os.path.join(repo, "src", "lib.rs")) as f:
        text = f.read()
    block = text[text.index("export_connect_plugins! {"):]
    return set(MIDDLEWARES_RE.findall(block[: block.index("}")]))


def clean(text):
    """AsciiDoc fragment -> one line that is safe inside a Markdown table cell."""
    text = re.sub(r"(?:xref|link):[^\[\s]*\[([^\]]*)\]", r"\1", text or "")
    text = re.sub(r"https?://[^\[\s]*\[([^\]]*)\]", r"\1", text)
    text = re.sub(r"<<(?:[^,>]*,\s*)?([^>]*)>>", r"\1", text)
    text = re.sub(r"(?<=[\w\]])\^", "", text)
    text = re.sub(r"\s+", " ", text).strip()
    return text.replace("|", "\\|").replace("<", "&lt;").replace(">", "&gt;")


def clean_summary(text):
    text = text or ""
    if "```" in text:
        return first_sentence(text)
    # Upstream glues a second component's summary on without a space (amqp_0_9 output).
    return clean(re.sub(r"(?<=[A-Za-z]\.)(?=[A-Z][a-z]).*", "", text, flags=re.S))


def first_sentence(text):
    text = (text or "").strip().split("\n\n")[0]
    match = re.match(r"(.*?[.!?])(\s|$)", text, re.S)
    return clean(match.group(1) if match else text)


def field_type(field):
    kind, name = field.get("kind", "scalar"), field.get("type", "")
    if kind == "array":
        return f"list of {name}"
    if kind == "map":
        return f"map of {name}"
    return name


def field_default(field):
    if "default" not in field:
        # An object's own fields carry the defaults; "required" would mislead.
        optional = field.get("is_optional") or field.get("children")
        return "" if optional else "required"
    text = json.dumps(field["default"])
    if len(text) > 40:
        text = text[:37] + "…"
    return "`" + text.replace("|", "\\|").replace("`", "'") + "`"


def render_component(kind, spec, middlewares):
    name = spec["name"]
    lines = [f"## `{name}`", ""]
    summary = (
        CORRECTIONS.get((kind, name, None))
        or clean_summary(spec.get("summary"))
        or first_sentence(spec.get("description"))
    )
    status = spec.get("status", "stable")
    if status != "stable":
        summary = f"**{status.capitalize()}.** {summary}"
    if summary:
        lines += [summary, ""]
    if kind == "processors":
        if name in middlewares:
            lines += [f"Middleware: `connect_{name}`, or inside a `connect` middleware.", ""]
        else:
            lines += ["Use it inside a `connect` middleware or a `connect` endpoint's `pipeline`.", ""]
    else:
        lines += [f"`connector: {name}` · URI `connect+{name.replace('_', '-')}://`", ""]

    fields = [f for f in spec["config"].get("children", []) if not f.get("is_deprecated")]
    common = [f for f in fields if not f.get("is_advanced")]
    advanced = [f for f in fields if f.get("is_advanced")]
    if common:
        lines += ["| Field | Type | Default | Description |", "|---|---|---|---|"]
        for f in common:
            description = CORRECTIONS.get((kind, name, f["name"])) or first_sentence(f.get("description"))
            lines.append(
                f"| `{f['name']}` | {field_type(f)} | {field_default(f)} | {description} |"
            )
        lines.append("")
    elif not fields:
        value = spec["config"]
        if value.get("type") not in (None, "object"):
            lines += [f"Takes a single {field_type(value)} value, not a map of fields.", ""]
    if advanced:
        lines += ["Advanced: " + ", ".join(f"`{f['name']}`" for f in advanced) + ".", ""]
    lines += [f"[Upstream documentation]({UPSTREAM}/{kind}/{name}/)", ""]
    return lines


def render_page(kind, specs, version, middlewares):
    filename, title, usage, end = PAGES[kind]
    specs = sorted(specs, key=lambda s: s["name"])
    lines = [
        "<!--",
        "  AUTO-GENERATED — DO NOT EDIT THIS FILE.",
        "  Regenerate with apps/mq-bridge-app/dev/scripts/gen-connect-docs.py.",
        "-->",
        "",
        f"# {title}",
        "",
        f"The {len(specs)} Redpanda Connect {kind} the [Connect plugin](connect.md) links, generated "
        f"from the component specs of Redpanda Connect {version}. Field descriptions are Redpanda "
        "Connect's own, shortened to one sentence; follow *Upstream documentation* for the rest. "
        "Install and configuration forms are on the [plugin page](connect.md).",
        "",
    ]
    if kind == "processors":
        lines += [
            f"{len(middlewares & {s['name'] for s in specs})} of them are also exported as their "
            "own `connect_<name>` middleware and run on any endpoint, native ones included. Every "
            "other processor that keeps, rewrites or drops each message runs inside a `connect` "
            "middleware; one that splits or merges messages belongs in a `connect` endpoint's "
            "`pipeline`. Middleware needs plugin 0.1.1 or newer.",
            "",
            "Own middleware: "
            + ", ".join(f"[`connect_{n}`](#{n})" for n in sorted(middlewares)) + ".",
            "",
            "```yaml",
            "input:",
            "  kafka: { url: localhost:9092, topic: orders }",
            "  middlewares:",
            "    - connect_mapping: 'root = this.merge({\"received_at\": now()})'",
            "```",
            "",
        ]
    else:
        lines += [
            usage,
            "",
            "```yaml",
            f"{end}:",
            "  connect:",
            "    connector: mqtt          # the component name below",
            "    urls: [\"tcp://localhost:1883\"]",
            "    " + ("topics: [\"orders\"]" if kind == "inputs" else "topic: \"orders\""),
            "```",
            "",
        ]

    by_category = {}
    for spec in specs:
        for category in spec.get("categories") or ["Other"]:
            by_category.setdefault(category, []).append(spec["name"])
    if kind == "processors":
        lines += ["Processors in **bold** are also their own middleware.", ""]
    lines += ["| Category | Components |", "|---|---|"]
    for category in sorted(by_category):
        links = ", ".join(
            f"**[`{n}`](#{n})**" if kind == "processors" and n in middlewares else f"[`{n}`](#{n})"
            for n in by_category[category]
        )
        lines.append(f"| {category} | {links} |")
    lines.append("")

    for spec in specs:
        lines += render_component(kind, spec, middlewares)
    with open(os.path.join(DOCS, filename), "w") as f:
        f.write("\n".join(lines).rstrip() + "\n")
    print(f"wrote connectors/{filename}: {len(specs)} {kind}")


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    repo = os.path.abspath(sys.argv[1])
    specs = dump_specs(repo)
    version, middlewares = connect_version(repo), middleware_names(repo)
    for kind in PAGES:
        render_page(kind, specs[kind], version, middlewares)


if __name__ == "__main__":
    main()

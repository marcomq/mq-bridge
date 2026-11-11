#!/usr/bin/env python3
"""Renders the animated SVGs in ../images to MP4 and GIF in out/ (git-ignored), for sites that take no SVG.

Needs Google Chrome and ffmpeg. Run: uv run --with playwright python render_video.py
"""
import subprocess
import sys
import tempfile
from pathlib import Path

from playwright.sync_api import sync_playwright

HERE = Path(__file__).resolve().parent
IMAGES, OUT = HERE.parent / "images", HERE / "out"
FPS = 25
ANIMATIONS = {"batching-explainer": 12.0, "batching-call-flow": 16.0}   # name: loop length in seconds


def frames(page, svg, outdir, seconds):
    page.goto(svg.as_uri())
    page.evaluate("document.getAnimations().forEach(a => a.pause())")
    for i in range(int(seconds * FPS)):
        page.evaluate(f"document.getAnimations().forEach(a => a.currentTime = {i * 1000 / FPS})")
        page.screenshot(path=str(outdir / f"f{i:04d}.png"))


def encode(outdir, name):
    src = ["ffmpeg", "-loglevel", "error", "-y", "-framerate", str(FPS), "-i", str(outdir / "f%04d.png")]
    subprocess.run(src + ["-vf", "scale=1920:-2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "18",
                          "-movflags", "+faststart", str(OUT / f"{name}.mp4")], check=True)
    subprocess.run(src + ["-vf", "fps=15,scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=64[p];"
                                 "[b][p]paletteuse=dither=none", str(OUT / f"{name}.gif")], check=True)


def main(names):
    OUT.mkdir(exist_ok=True)
    with sync_playwright() as p:
        browser = p.chromium.launch(channel="chrome")
        page = browser.new_page(viewport={"width": 1200, "height": 675}, device_scale_factor=2)
        for name in names or ANIMATIONS:
            with tempfile.TemporaryDirectory() as tmp:
                frames(page, IMAGES / f"{name}.svg", Path(tmp), ANIMATIONS[name])
                encode(Path(tmp), name)
            print(name, "mp4 + gif")
        browser.close()


if __name__ == "__main__":
    main(sys.argv[1:])

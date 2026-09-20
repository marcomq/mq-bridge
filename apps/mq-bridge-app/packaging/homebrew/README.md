# Homebrew distribution

Two artifacts, both served from the tap repo **`marcomq/homebrew-tap`** (a
separate GitHub repo — Homebrew requires the `homebrew-` name prefix and clones
it whole; it is *not* a folder of this repo):

| Install | Command | What it is |
| --- | --- | --- |
| CLI / MCP server (**formula**) | `brew install marcomq/tap/mq-bridge-app` | Headless binary, installed as both `mq-bridge-app` and the short `mqb`, same asset `cargo binstall` uses |
| Desktop UI (**cask**) | `brew install --cask marcomq/tap/mq-bridge` | The Tauri `.app`, dragged into `/Applications` |

The tap repo is the **only** home for the formula and cask. This repo holds no
`.rb` files — [`render.sh`](render.sh) generates them (with real checksums pulled
from the release assets) and the release CI job pushes them to the tap. Nothing
to keep in sync, nothing to drift.

```text
marcomq/homebrew-tap          ← the tap (installs read from here)
├── Formula/mq-bridge-app.rb  ← CLI / MCP server
└── Casks/mq-bridge.rb        ← desktop UI
```

## Endpoint plugins

The tap also carries one formula per mq-bridge endpoint plugin —
`mq-bridge-pulsar`, `mq-bridge-meilisearch`, `mq-bridge-redpanda` — each a
prebuilt shared library installed into `lib/mq-bridge`, which mq-bridge probes
under `HOMEBREW_PREFIX` by default. They declare no dependency on
`mq-bridge-app`: the same library serves the brewed CLI, the desktop app and a
Python or Node host.

These are rendered and pushed **by the tap, not by this repo or the plugin
repos**. `marcomq/homebrew-tap` runs `.github/workflows/sync-plugins.yml` daily
(and on demand), reads each plugin's latest public release, renders it with
`bin/render-plugin-formula.sh` and commits to itself. The plugin repositories
therefore need no tap credential: a write-enabled deploy key lives in exactly
one place, this repo, for the formula and cask above.

The renderer reads each release's `.sha256` sidecar instead of downloading the
archive to hash it, which is why the plugin releases publish one. A plugin that
has not cut a release yet renders no formula, and the sync job says so.

## macOS vs Linux

Homebrew runs on **both macOS and Linux** ("Homebrew on Linux"), but the two
artifact types don't: **formulae work on macOS and Linux; casks are macOS-only.**
So the CLI is `brew install`-able on both platforms; the desktop UI is a cask and
therefore Mac-only — Linux desktop users take the `.deb` / `.AppImage` from the
GitHub release directly.

## Coverage / limits

- **Formula (CLI)** covers macOS Apple Silicon and Linux x86_64 (the two
  `mq-bridge-cli-*.tar.gz` assets the release builds). Intel macOS / arm64 Linux
  aren't covered until the release matrix builds those targets.
- **Cask (UI)** is macOS Apple Silicon only — casks are macOS-only and the `.dmg`
  is built for `aarch64` only. Its `zap` stanza uses the app's real bundle id
  (`com.marcomq.mqbridgeapp`).

Both installs are a single ~20 MB prebuilt download (no compilation) and complete
in seconds — the formula copies the released binary, the cask drops the `.app`
into `/Applications`.

## Bootstrapping / updating the tap by hand

`render.sh <version>` writes `Formula/mq-bridge-app.rb` and `Casks/mq-bridge.rb`
into an output dir with real checksums. To (re)populate the tap for a given
release:

```bash
# from this repo root
apps/mq-bridge-app/packaging/homebrew/render.sh 0.4.8 marcomq/mq-bridge /tmp/tap-out

git clone https://github.com/marcomq/homebrew-tap
mkdir -p homebrew-tap/Formula homebrew-tap/Casks
cp /tmp/tap-out/Formula/mq-bridge-app.rb homebrew-tap/Formula/
cp /tmp/tap-out/Casks/mq-bridge.rb       homebrew-tap/Casks/
cd homebrew-tap && git add Formula Casks \
  && git commit -m "mq-bridge 0.2.8" && git push
```

Verify:

```bash
brew install marcomq/tap/mq-bridge-app        # CLI formula
mqb --version
brew test marcomq/tap/mq-bridge-app           # runs the formula's test block
brew install --cask marcomq/tap/mq-bridge      # desktop UI cask
```

> Current tap state (0.2.8): both `Formula/mq-bridge-app.rb` and
> `Casks/mq-bridge.rb` are present and install. The formula still carries an older
> header comment; it self-corrects on the next release once CI runs, or re-render
> and push the formula to refresh it now.

## Automating the bump on release

The `update-homebrew-tap` job below is **already in**
`.github/workflows/release.yml`. It `needs` the jobs that produce the CLI
tarballs *and* the Tauri bundle, runs `render.sh` against the freshly published
assets, and pushes both files to the tap — so the formula text lives in exactly
one place (`render.sh`), never duplicated in YAML.

It authenticates to the tap with an **SSH deploy key** (deploy keys don't expire,
unlike fine-grained PATs which are capped at ~1 year — so there's no yearly
rotation that can silently break the release). One-time setup:

```bash
# 1. Generate a keypair (no passphrase)
ssh-keygen -t ed25519 -C "homebrew-tap deploy" -f tap_deploy_key -N ""

# 2. Add the PUBLIC key to the tap as a write-enabled deploy key:
#    github.com/marcomq/homebrew-tap → Settings → Deploy keys → Add deploy key
#    → paste tap_deploy_key.pub, tick "Allow write access"

# 3. Store the PRIVATE key as a secret on THIS repo:
gh secret set HOMEBREW_TAP_DEPLOY_KEY -R marcomq/mq-bridge < tap_deploy_key

# 4. Delete the local copies
rm tap_deploy_key tap_deploy_key.pub
```

Until the key is set, the job runs but its push step fails.

```yaml
  update-homebrew-tap:
    name: Update Homebrew tap
    needs: [metadata, build-release, build-tauri-release]
    runs-on: ubuntu-latest
    steps:
      - name: Checkout repository
        uses: actions/checkout@v4

      - name: Render formula + cask from release assets
        env:
          VERSION: ${{ needs.metadata.outputs.release_tag }}
          REPO: ${{ github.repository }}
        run: packaging/homebrew/render.sh "$VERSION" "$REPO" out

      - name: Push to tap
        env:
          DEPLOY_KEY: ${{ secrets.HOMEBREW_TAP_DEPLOY_KEY }}
          VERSION: ${{ needs.metadata.outputs.release_tag }}
        run: |
          mkdir -p ~/.ssh
          printf '%s\n' "$DEPLOY_KEY" > ~/.ssh/tap_key
          chmod 600 ~/.ssh/tap_key
          ssh-keyscan github.com >> ~/.ssh/known_hosts 2>/dev/null
          export GIT_SSH_COMMAND="ssh -i ~/.ssh/tap_key -o IdentitiesOnly=yes"
          git clone git@github.com:marcomq/homebrew-tap.git tap
          mkdir -p tap/Formula tap/Casks
          cp out/Formula/mq-bridge-app.rb tap/Formula/mq-bridge-app.rb
          cp out/Casks/mq-bridge.rb       tap/Casks/mq-bridge.rb
          cd tap
          git config user.name "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
          git add Formula/mq-bridge-app.rb Casks/mq-bridge.rb
          git commit -m "mq-bridge ${VERSION}" || exit 0
          git push
```

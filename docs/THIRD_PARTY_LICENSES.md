# Third-party license files

The repository currently has no maintained generator for its third-party
license files. They were produced with temporary local scripts and then
reviewed and compacted. Do not assume that `cargo update` or `npm update`
refreshes them automatically.

The distributed files are:

- `apps/mq-bridge-app/crates/core/THIRD_PARTY_LICENSES.txt` for the CLI, its
  embedded web UI, the Tauri application, release archives, Docker, Homebrew,
  and Conda packages.
- `node/mq-bridge-node/THIRD_PARTY_LICENSES.txt` for the platform-specific npm
  packages containing the native addon. The JavaScript-only root npm package
  does not ship this file.
- `python/mq-bridge-py/THIRD_PARTY_LICENSES.txt` for Python wheels containing
  the native extension.

## Current generation policy

- Rust inventories contain the normal transitive dependency closure with all
  product features enabled. Build-only and development-only dependencies are
  excluded. Platform-specific dependencies are included for every supported
  release target.
- The app inventory covers all three app workspace crates. Its web UI inventory
  contains non-development packages from `apps/mq-bridge-app/package-lock.json`
  and `tslib`, which `@tauri-apps/api` bundles.
- The Node and Python inventories are separate because their native binding
  layers use `napi-rs` and PyO3 respectively.
- Complete SPDX expressions are retained. An expression such as
  `MIT OR Apache-2.0` is not reduced to an automatic MIT selection.
- The common MIT and Apache-2.0 license bodies occur once per file. Required
  copyright and attribution notices remain associated with their components.
- Upstream `NOTICE` content and non-standard license terms are retained.
- The app identifies the source locations of its MPL-2.0 components.

## Dependency-update checklist

After either Cargo lockfile or the app package lock changes:

1. Run `cargo deny check licenses bans sources` in the repository root and in
   `apps/mq-bridge-app`.
2. Recalculate the affected runtime dependency closure. Treat the app, Node
   binding, and Python binding as separate distributions.
3. Compare component names, versions, SPDX expressions, upstream license files,
   and upstream `NOTICE` files with the applicable tracked notice file.
4. Add new or changed copyright/attribution text. Preserve full `OR`
   expressions; do not silently choose a preferred alternative.
5. Keep only one common MIT body and one common Apache-2.0 body in each output.
6. Review the resulting diff before committing it. In particular, investigate
   missing license metadata rather than generating an incomplete entry.

`cargo-deny` checks whether resolved licenses are allowed, but it does not
verify that these distributed notice files are current.

## Future automation

A maintained generator should use Cargo metadata plus the app package lock,
support both update and `--check` modes, preserve complete SPDX expressions,
deduplicate common license bodies, retain component-specific notices, and fail
when licensing information cannot be resolved safely. CI can enforce freshness
once that generator is reproducible on a clean checkout.

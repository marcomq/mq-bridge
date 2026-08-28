# Security Policy

## Reporting a vulnerability

Please report suspected vulnerabilities privately via
[GitHub Security Advisories](https://github.com/marcomq/mq-bridge/security/advisories/new)
rather than opening a public issue.

## Rust supply chain

`cargo-deny` runs against all three workspace members with `--all-features`,
because the shipped application enables `mq-bridge/full`. Licenses, bans, and
sources are hard-gated in CI; RustSec advisories are informational and every
current suppression is justified inline in [`deny.toml`](deny.toml).

The application is the redistributor: its `Cargo.lock` describes the dependency
graph shipped in released binaries. The detailed per-path analysis for the
shared `mq-bridge` dependency is maintained in the [engine repository's
SECURITY.md](https://github.com/marcomq/mq-bridge/blob/main/SECURITY.md), rather
than duplicated here.

One important app-specific consequence is definite: `mq-bridge/full` enables
the AWS transport in released builds. The AWS SDK's legacy `rustls 0.21`
connector therefore brings `rustls-webpki 0.101.7` into released binaries, and
RUSTSEC-2026-0098 and RUSTSEC-2026-0099 are reachable during ordinary AWS
certificate validation. Exploitation requires a name-constrained CA in the
trust store to issue a certificate outside its constraints (including an
out-of-scope wildcard for -0099). The issue remains tracked pending the AWS
SDK migration off that connector; it is not claimed to be absent from the
binary. The CRL findings and the rumqttc path are explained in the linked
analysis and suppressed with their app-specific reasoning in `deny.toml`.

`cargo audit` also reports transitive unmaintained crates that
`unmaintained = "workspace"` intentionally does not gate in cargo-deny. The
current audit-only findings include the GTK3 binding family
(RUSTSEC-2024-0411 through RUSTSEC-2024-0420), `proc-macro-error`
(RUSTSEC-2024-0370), `fxhash` and `instant` (RUSTSEC-2025-0057 and
RUSTSEC-2024-0384), the `unic-*` family (RUSTSEC-2025-0075, -0080, -0081,
-0098, and -0100), and `rustls-pemfile` (RUSTSEC-2025-0134). These are
maintenance follow-ups, not claims that the crates are vulnerability-free.

## Open work outside cargo-deny

Cargo-deny does not inspect the served JavaScript bundle or release container
image. Dependabot now proposes weekly Cargo, npm, and GitHub Actions updates
through [`.github/dependabot.yml`](.github/dependabot.yml); it does not replace
vulnerability or license scanning. The remaining supply-chain work is
intentionally separate:

- add npm dependency auditing and JavaScript license checks for the runtime UI;
- add Trivy or Grype scanning for Docker base-image CVEs;

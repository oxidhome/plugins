# Contributing to OxidHome Plugins

Thanks for your interest in contributing plugins to OxidHome. This repo holds first-party plugins — device integrations, protocols, and AI components — that ship as part of the official OxidHome platform.

For changes to the core platform (host runtime, Rust SDK, WIT interface), see [`oxidhome/oxidhome`](https://github.com/oxidhome/oxidhome).

## What belongs in this repo

This repo is for **first-party plugins** maintained by the OxidHome project. That means plugins that are:

- Broadly useful (covering common protocols or device categories)
- Maintained on a long-term basis
- Reviewed and tested to a higher standard than community plugins
- Eligible to ship as defaults in OxidHome distributions

If you're building a plugin for a specific device or service that fits this profile, contributions are welcome. If you're building something narrower or more experimental, consider publishing it as a standalone repository — third-party plugins are a first-class part of the ecosystem and don't need to live here.

When in doubt, open an issue describing the plugin you're considering before starting work.

## How to contribute a plugin

### 1. Discuss first

For new plugins, open an issue with:

- What device, service, or capability the plugin covers
- Which plugin world it targets (`plugin`, `streaming-plugin`, `ai-plugin`, `streaming-ai-plugin`)
- Roughly what permissions and capabilities it needs from the host
- Whether the protocol/API it uses is documented, reverse-engineered, or vendor-supported

This avoids duplicate work and lets us flag issues (licensing concerns, scope mismatch) before you've invested time.

### 2. Build it

Each plugin lives in its own subdirectory under `crates/`. The structure for a Rust plugin:

```
crates/your-plugin/
├── Cargo.toml
├── README.md          # what it does, what devices it supports, configuration
├── plugin.toml        # plugin manifest (capabilities, network targets, etc.)
├── src/
│   └── lib.rs
├── tests/
│   └── integration.rs
└── examples/          # optional: example configurations
```

Plugins should:

- Build to a `.wasm` component using `wasm32-wasip2` target.
- Pass `cargo clippy --target wasm32-wasip2 -- -D warnings`.
- Include integration tests using the OxidHome test host (see [`oxidhome/oxidhome`](https://github.com/oxidhome/oxidhome)).
- Declare all required capabilities in `plugin.toml` — the host enforces these, but explicit declarations are also documentation.
- Request only the capabilities they actually need. A plugin that asks for `network = "*"` when it only talks to one IP will be rejected in review.

### 3. Document it

Every plugin needs a `README.md` covering:

- **What it does** (one paragraph)
- **Supported devices or services** (be specific — model numbers, firmware versions, API versions)
- **Configuration options** (every field, what it does, what's required vs. optional)
- **Required capabilities** (network access, models, etc.) and why
- **Known limitations** (devices that don't work, edge cases, missing features)
- **Credits** if the plugin is based on prior work or reverse-engineering by others

Honest limitation sections are especially valued. "Works with the X3 firmware 2.1+, has not been tested on X3 Pro" is much more useful than vague claims of universal support.

### 4. Submit a PR

PR reviews check:

- Plugin works as documented
- Manifest declares appropriate (and minimal) capabilities
- Code is sandboxing-friendly — no attempts to escape the WASM boundary or bypass capability checks
- README is complete
- Tests cover the main flows
- License headers match the repo's licensing

## Plugin standards

### Capabilities

Plugins must declare what they need:

```toml
[plugin]
name = "example-camera"
world = "streaming-plugin"
multi-instance = true

[capabilities]
network = ["http-out", "rtsp"]
network-targets = "configured-only"  # only the IP/host from instance config
memory-mb = 32
cpu-percent = 5

[capabilities.config-schema]
# JSON schema or similar describing what the user must configure
```

Plugins that request broad capabilities (`network-targets = "any"`, large memory/CPU budgets, model registry access) will get extra scrutiny. The default expectation is **least privilege**.

### Stability

Plugins in this repo follow semver from 1.0 onwards. Pre-1.0, breaking changes are allowed but should be flagged in the changelog. Once a plugin reaches 1.0, breaking changes to its config schema or behavior need a major version bump.

### Behavior

- **Don't crash on bad input.** A malformed device response should be logged and ignored, not panic.
- **Don't poll aggressively.** If your plugin polls a device, default to a sensible interval (>= 30s for most things) and let users tune it.
- **Don't block.** Use async patterns. The host may suspend your plugin instance if it doesn't yield.
- **Persist what's needed, nothing more.** The KV storage interface has quotas. Don't store sensor history there — that's the host's job.
- **Log usefully.** `tracing` with appropriate levels: `info` for lifecycle, `debug` for normal operations, `warn` for recoverable issues, `error` for things the user needs to know about.

### Security expectations

These are not optional:

- **No secrets in code.** Credentials go through the configuration interface, never hardcoded.
- **No data exfiltration.** Plugins must not phone home, send telemetry to third parties, or transmit user data anywhere not declared in the manifest.
- **Sanitize inputs.** Device responses, network data, user config — assume hostile and validate.
- **Document threat model.** If your plugin handles cameras, locks, or other security-sensitive devices, the README should explicitly call out the security model.

PRs that violate these are not negotiable — they will be rejected, even if the plugin is otherwise excellent.

## Reverse-engineered protocols

Many smart-home plugins necessarily build on reverse-engineered protocols. This is fine, but please:

- **Credit prior work.** If your plugin builds on someone else's reverse-engineering effort, link to their repo or write-up in the README.
- **Don't include vendor binary blobs.** Closed-source SDK shims, decompiled vendor code, or firmware extracted from devices doesn't belong in this repo. If your plugin genuinely needs that, it's a candidate for the native bridge plugin model (see core architecture docs) and not appropriate for this repo.
- **Be honest about brittleness.** Reverse-engineered protocols break when vendors push updates. Document expected lifespan.

## Licensing of contributions

Plugins in this repo are dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.

If your plugin includes content under different terms (e.g. ML model weights with their own license), declare it in the plugin's README and ensure compatibility with dual MIT/Apache.

## Code of conduct

Be respectful, be patient, assume good faith. The maintainers reserve the right to remove comments, close issues, or restrict participation if needed to keep the project healthy.

## Questions

For implementation questions, open a GitHub Discussion. For questions about whether a plugin belongs here, open an issue.

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

## Repository layout

The repo is one Cargo workspace. Each plugin is a member crate under `crates/`:

```
Cargo.toml             # workspace: members, shared package fields, lints, every dependency version
crates/your-plugin/
├── Cargo.toml         # `crate-type = ["cdylib"]`, deps as `<dep>.workspace = true`
├── README.md          # what it does, what devices it supports, configuration
├── manifest.toml      # plugin manifest (identity, capabilities, config schema)
├── src/
│   └── lib.rs         # implements `oxidhome_sdk::Plugin`, ends with `oxidhome_sdk::plugin!(...)`
└── tests/             # native tests, e.g. manifest validation
```

[`crates/template`](crates/template) is the starting point: copy it and follow its README.

Workspace conventions, matching `oxidhome/oxidhome`:

- Every dependency version lives in `[workspace.dependencies]` in the root `Cargo.toml`; member crates only write `<dep>.workspace = true`.
- Every crate has `publish = false`. Plugins ship as `.wasm` components next to their `manifest.toml`, not as crates.
- Every crate has `[lints] workspace = true`: clippy `all` + `pedantic`, `unsafe_code = "deny"`, and cargo's `unused_dependencies = "deny"`.
- The toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (Rust 1.100.0 with the `wasm32-wasip2` target), and `rust-version` matches it.

## The SDK dependency

Plugins build on `oxidhome-sdk`, which brings in the WIT bindings (`oxidhome:plugin@0.1.0`) through `oxidhome-wit`. The SDK is not on crates.io yet, so the root `Cargo.toml` takes it from git, pinned to one `oxidhome/oxidhome` commit:

```toml
oxidhome-sdk = { git = "https://github.com/oxidhome/oxidhome", rev = "<commit>", default-features = false }
oxidhome-manifest = { git = "https://github.com/oxidhome/oxidhome", rev = "<commit>" }
```

`oxidhome-manifest` is the host's manifest parser and validator; plugins use it as a dev-dependency to test their `manifest.toml`. Both entries must name the same commit.

The SDK binds one plugin world per build, picked by feature, so each plugin names its world in its own `Cargo.toml`, matching `world` in its `manifest.toml`:

```toml
oxidhome-sdk = { workspace = true, features = ["plugin"] }           # world = "plugin"
oxidhome-sdk = { workspace = true, features = ["streaming-plugin"] } # world = "streaming-plugin"
```

Cargo unifies features across the packages of one invocation, and a build with both worlds fails, so build and check one package at a time (`-p <package>`), never `--workspace`. [`.github/each-package.sh`](.github/each-package.sh) runs a cargo subcommand once per member, which is what CI does.

Don't add `wit-bindgen` or a copy of the WIT files to a plugin. `oxidhome-wit` pins `wit-bindgen` to an exact version, and going through the SDK keeps every plugin on the bindings the host was tested with.

To move to a newer `oxidhome/oxidhome` commit:

1. Set both `rev`s in the root `Cargo.toml` to the new full commit SHA (a commit on `main`).
2. Run `cargo update -p oxidhome-sdk -p oxidhome-manifest` and commit the `Cargo.lock` change with it.
3. Build, lint and test everything (see below). Fix what the new contract breaks; the commits between the two revs and `examples/` in `oxidhome/oxidhome` show how the API moved, and set `sdk_version` in each `manifest.toml` to the pinned `oxidhome-sdk` crate version (`cargo pkgid oxidhome-sdk`).
4. Open the bump as its own PR.

Dependabot doesn't move a `rev` pinned to a full SHA, so bumps are always manual.

## Building and testing

You'll need `rustup`; it reads `rust-toolchain.toml` and installs the pinned toolchain and the `wasm32-wasip2` target on first use. `cargo install-tools` (an alias in `.cargo/config.toml`) installs the same tools CI uses: `cargo-deny`, `cargo-llvm-cov`, `cargo-machete`, `cargo-nextest` and `cargo-action-fmt`.

Build one plugin's component:

```sh
cargo build -p <package> --target wasm32-wasip2 --release
# -> target/wasm32-wasip2/release/<package_with_underscores>.wasm
```

Run what CI runs:

```sh
cargo fmt --all --check
.github/each-package.sh clippy --all-features --all-targets -- -D warnings
.github/each-package.sh clippy --target wasm32-wasip2 --all-features --lib -- -D warnings
.github/each-package.sh nextest run --all-features --all-targets --no-tests=pass
RUSTDOCFLAGS="-D warnings" .github/each-package.sh doc --all-features --no-deps
.github/each-package.sh build --target wasm32-wasip2 --locked
cargo machete
cargo deny check all
```

Tests run natively. Host imports (`host::register_device`, `host::config::get_typed`, …) only work inside the host, so keep protocol parsing and state logic in plain functions you can unit-test, and test the manifest with `oxidhome-manifest` the way the template does. For end-to-end tests, `oxidhome/oxidhome` has `oxidhome-test-host`, which loads a built component into the real runtime inside a `#[test]`.

To try a plugin in the host, put `manifest.toml` and the built `.wasm` in one directory (named by `runtime.wasm` in the manifest) and pass that directory to the daemon from an `oxidhome/oxidhome` checkout: `cargo run -p oxidhome-core -- <dir>`. That shortcut starts the plugin with its manifest defaults only, so a plugin with required settings (for example a `secret` without a default) fails config validation there; install it through the host API and start it with config overrides instead.

## How to contribute a plugin

### 1. Discuss first

For new plugins, open an issue with:

- What device, service, or capability the plugin covers
- Which plugin world it targets (`plugin`, `streaming-plugin`, `ai-plugin`, `streaming-ai-plugin`)
- Roughly what permissions and capabilities it needs from the host
- Whether the protocol/API it uses is documented, reverse-engineered, or vendor-supported

This avoids duplicate work and lets us flag issues (licensing concerns, scope mismatch) before you've invested time.

### 2. Build it

Start from [`crates/template`](crates/template). Plugins should:

- Build to a `.wasm` component for the `wasm32-wasip2` target with `--locked`.
- Pass every check listed under [Building and testing](#building-and-testing).
- Declare all required capabilities in `manifest.toml`. The host enforces them at each call site, and the declarations double as documentation.
- Request only the capabilities they actually need. A plugin that allows any host when it only talks to devices on the LAN will be rejected in review.

### 3. Document it

Every plugin needs a `README.md` covering:

- **What it does** (one paragraph)
- **Supported devices or services** (be specific — model numbers, firmware versions, API versions)
- **Configuration options** (every field, what it does, what's required vs. optional)
- **Required capabilities** (network access, storage, etc.) and why
- **Known limitations** (devices that don't work, edge cases, missing features)
- **Credits** if the plugin is based on prior work or reverse-engineering by others

Honest limitation sections are especially valued. "Works with the X3 firmware 2.1+, has not been tested on X3 Pro" is much more useful than vague claims of universal support.

Add the plugin to the table in [README.md](README.md).

### 4. Submit a PR

Commits use short scoped subjects (`feat(kasa): …`, `fix: …`, `ci: …`) and carry a `Signed-off-by` trailer (`git commit -s`). PR reviews check:

- Plugin works as documented
- Manifest declares appropriate (and minimal) capabilities
- Code is sandboxing-friendly — no attempts to escape the WASM boundary or bypass capability checks
- README is complete
- Tests cover the main flows
- License headers match the repo's licensing

The required status check is **All checks**, which aggregates every CI job except the informational Codecov upload.

## Plugin standards

### Capabilities

Plugins declare what they need in `manifest.toml`. Capability-gated host calls (device registration, event subscription, storage, blob store, services) return `permission-denied` unless the manifest declares them:

```toml
[capabilities]
# Outbound network rules: proto://host[:port], with CIDRs and wildcards.
# Only the streaming worlds may declare them; see below.
network = ["udp://192.168.0.0/16:9999", "tcp://192.168.0.0/16:9999"]
storage_quota_kb = 64              # KV storage; 0 or absent = no storage
declares_devices = ["switch", "sensor"]
subscribes_events = false

[config.host]
type = "string"
description = "Device address."

[config.password]
type = "secret"                    # never has a default
description = "Cloud account password."
```

The standard `plugin` world has no raw I/O: no sockets, and the host leaves WASI networking off, so validation refuses `network` rules on a `plugin` (or `ai-plugin`) manifest; only `streaming-plugin` and `streaming-ai-plugin` may declare them. A plugin that talks to devices targets `streaming-plugin` (the SDK's `streaming-plugin` feature, `StreamingPlugin` and `streaming_plugin!`): `std::net` sockets and outgoing `wasi:http` work there, and the host checks every connect, send, name lookup and HTTP request against the granted rules, on the resolved address. LAN addresses need an IP or CIDR rule, or a name under `.local`, `.lan`, `.home.arpa` or `.internal`. Broadcast and multicast are refused, `wasi:http` serves plain `http://` only, and `host-media` answers `unavailable` until OxidHome's media phase. With `tick_interval_ms` set, the host also ticks a streaming plugin early when a socket it holds is readable.

A `secret` field's value reaches the owning plugin in plain text through `host::config`; the host masks it in its API and scrubs it from plugin log output on a best-effort basis. Never log a credential or put one in an error message, event or device state.

Plugins that request broad capabilities (any host, large storage quotas, other plugins' services) will get extra scrutiny. The default expectation is **least privilege**. CPU and memory aren't manifest settings: the host applies the same fixed resource ceilings to every instance.

### Stability

Plugins in this repo follow semver from 1.0 onwards. Pre-1.0, breaking changes are allowed but should be flagged in the changelog. Once a plugin reaches 1.0, breaking changes to its config schema or behavior need a major version bump.

### Behavior

- **Don't crash on bad input.** A malformed device response should be logged and ignored, not panic.
- **Don't poll aggressively.** If your plugin polls a device, do it from `tick` with a sensible `tick_interval_ms` (>= 30s for most things) and let users tune it.
- **Don't block.** Callbacks are synchronous; return promptly. The host's liveness watchdog reclaims an instance whose call doesn't return, but it can't interrupt a blocking socket call waiting inside the host: in `streaming-plugin`, use non-blocking sockets or set read and write timeouts.
- **Report command outcomes honestly.** Return `err` only when the device was not touched, `unknown` when a command may have taken effect but can't be confirmed, and `accepted` plus a later `host::complete_command` when the device answers asynchronously.
- **Persist what's needed, nothing more.** The KV storage interface has quotas. Don't store sensor history there — that's the host's job.
- **Log usefully.** Use `oxidhome_sdk::tracing` after `oxidhome_sdk::logging::init()`, with appropriate levels: `info` for lifecycle, `debug` for normal operations, `warn` for recoverable issues, `error` for things the user needs to know about.

### Security expectations

These are not optional:

- **No secrets in code.** Credentials go through `type = "secret"` config fields, never hardcoded.
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

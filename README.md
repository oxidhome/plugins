# OxidHome Plugins

First-party plugins for [OxidHome](https://github.com/oxidhome/oxidhome).
Each plugin is a WebAssembly component (`wasm32-wasip2`) built on the
OxidHome Rust SDK, shipped with a `manifest.toml` that declares what it
needs from the host.

## Plugins

| Plugin                         | Status | What it does                                                          |
|--------------------------------|--------|-----------------------------------------------------------------------|
| [template](crates/template)    | Ready  | A software switch to copy when starting a new plugin.                 |
| Kasa                           | Draft  | TP-Link Kasa devices on the local network (in progress).              |

The SDK is not on crates.io yet, so plugins depend on it from git at a
pinned oxidhome/oxidhome commit. See [CONTRIBUTING.md](CONTRIBUTING.md)
for the layout, how to build and test a plugin, and how to bump the SDK.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  http://opensource.org/licenses/MIT)

at your option.

## Contribution

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.

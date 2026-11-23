# Template plugin

A starting point for a new OxidHome plugin. It is a software-only switch,
built on `oxidhome-sdk` the same way as `examples/simulated-switch` in
[oxidhome/oxidhome](https://github.com/oxidhome/oxidhome):

- On `init`: reads `default_state: bool` from per-instance config
  (manifest default `false`) and registers one device, `switch-1`, with
  the `switch` capability.
- On `execute_command`: `switch::set { state: bool }` sets the state,
  `switch::toggle` flips it. Both publish a `state-changed` event and
  answer `ok-with-state` with the new `state`. Any other capability or action is refused with
  `invalid-argument`; a command for a device this instance doesn't own
  gets `not-found`.
- On `shutdown`: removes the device again.

## Start a new plugin from it

1. Copy `crates/template` to `crates/<your-plugin>` and add it to
   `members` in the root `Cargo.toml`.
2. Rename the package in its `Cargo.toml`, and update `id`, `name`,
   `description`, `source`, `keywords` and `runtime.wasm` in
   `manifest.toml`. `runtime.wasm` is the component's file name: the
   package name with `-` replaced by `_`, plus `.wasm`.
3. Declare only the capabilities the plugin uses in `[capabilities]` and
   its settings under `[config.*]`. Credentials use `type = "secret"`,
   which never has a `default`.
4. Replace this README with one that follows the checklist in
   [CONTRIBUTING.md](../../CONTRIBUTING.md#3-document-it).

## Build and test

From the repository root:

```sh
cargo build -p oxidhome-template-plugin --target wasm32-wasip2 --release
cargo nextest run -p oxidhome-template-plugin
```

The test parses and validates `manifest.toml` with the host's
`oxidhome-manifest` crate at the pinned oxidhome revision, so schema and
validation errors fail here first. The host's `sdk_version`
compatibility check runs only when it loads the plugin.

## Run it in the host

The host loads a plugin from an install directory that holds
`manifest.toml` and the component named by `runtime.wasm`:

```sh
mkdir -p /tmp/template-plugin
cp crates/template/manifest.toml /tmp/template-plugin/
cp target/wasm32-wasip2/release/oxidhome_template_plugin.wasm /tmp/template-plugin/
# from an oxidhome/oxidhome checkout
cargo run -p oxidhome-core -- /tmp/template-plugin
```

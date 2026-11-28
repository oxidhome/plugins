//! `manifest.toml` must parse and validate with the same
//! `oxidhome-manifest` the host uses, at the pinned `oxidhome` rev.

use oxidhome_manifest::{PluginManifest, validate};

#[test]
fn manifest_is_valid() {
    let manifest: PluginManifest =
        toml::from_str(include_str!("../manifest.toml")).expect("manifest.toml parses");
    if let Err(errors) = validate(&manifest) {
        panic!("manifest.toml is invalid: {errors:?}");
    }
}

//! Template `OxidHome` plugin: one software switch.
//!
//! Copy this crate to start a new plugin. It shows the pieces most
//! device plugins need, built on `oxidhome-sdk` the same way as
//! `examples/simulated-switch` in oxidhome/oxidhome:
//!
//! - `init` reads per-instance config declared in `manifest.toml` and
//!   registers a device with the host;
//! - `execute_command` matches `(capability, action)`, updates the
//!   device, publishes a `state-changed` event and answers with an
//!   explicit outcome;
//! - `shutdown` removes the device again.

use oxidhome_sdk::bindings::oxidhome::plugin::capabilities::{
    CapabilitySpec, CapabilityState, Switchable,
};
use oxidhome_sdk::bindings::oxidhome::plugin::devices::{Command, CommandResult};
use oxidhome_sdk::bindings::oxidhome::plugin::types::{DeviceId, Error, KeyValue, Value};
use oxidhome_sdk::{Device, Plugin, host};

#[derive(Default)]
struct Template {
    /// Host-assigned id of the device registered in `init`.
    device_id: Option<DeviceId>,
    /// Current switch state, kept in sync with what the host was told.
    state: bool,
}

impl Plugin for Template {
    fn init(&mut self) -> Result<(), String> {
        // Route `tracing` macros to the host's `logging` import. It only
        // fails when a subscriber is already set, which is harmless.
        let _ = oxidhome_sdk::logging::init();

        // `[config.default_state]` in manifest.toml. Only a missing entry
        // falls back to `false`; a type mismatch or host error fails
        // `init` so manifest drift doesn't pass silently.
        self.state = host::config::get_typed::<bool>("default_state")
            .map_err(|e| format!("reading default_state config: {e}"))?
            .unwrap_or(false);

        let id = host::register_device(
            Device::new("switch-1", "Template Switch")
                .manufacturer("OxidHome")
                .model("template")
                .capability(CapabilitySpec::Switch)
                .initial_state(CapabilityState::Switch(Switchable { state: self.state })),
        )
        .map_err(|e| format!("register-device failed: {e:?}"))?;
        self.device_id = Some(id);
        oxidhome_sdk::tracing::info!(initial_state = self.state, "template plugin ready");
        Ok(())
    }

    fn shutdown(&mut self) {
        if let Some(id) = self.device_id.take() {
            // Best effort: the host may already have dropped the device.
            let _ = host::remove_device(&id);
        }
        oxidhome_sdk::tracing::info!("template plugin stopped");
    }

    /// Return `Err` only when the device was not touched (callers may
    /// retry), `Unknown` when the command may have taken effect but
    /// can't be confirmed, and `Accepted` when the result arrives later:
    /// keep `cmd.id` and report it with `host::complete_command`.
    fn execute_command(&mut self, device: DeviceId, cmd: Command) -> CommandResult {
        if self.device_id.as_deref() != Some(device.as_str()) {
            return CommandResult::Err(Error::NotFound(format!(
                "device {device} not owned by this plugin"
            )));
        }
        if cmd.capability != "switch" {
            return CommandResult::Err(Error::InvalidArgument(format!(
                "unsupported capability {}",
                cmd.capability
            )));
        }
        self.state = match cmd.action.as_str() {
            "toggle" => !self.state,
            "set" => match arg_bool(&cmd, "state") {
                Some(state) => state,
                None => {
                    return CommandResult::Err(Error::InvalidArgument(
                        "switch::set requires a `state: bool` arg".into(),
                    ));
                }
            },
            other => {
                return CommandResult::Err(Error::InvalidArgument(format!(
                    "unsupported action switch::{other}"
                )));
            }
        };
        let fields = vec![KeyValue {
            key: "state".to_string(),
            value: Value::BoolVal(self.state),
        }];
        // The reply updates the host's stored device state; the event tells
        // subscribers about the change. Publishing is best effort: the
        // command already succeeded.
        if let Err(e) = host::publish_state_change(device, "switch", fields.clone()) {
            oxidhome_sdk::tracing::warn!("publish_state_change failed: {e:?}");
        }
        oxidhome_sdk::tracing::info!(state = self.state, "switch state changed");
        CommandResult::OkWithState(fields)
    }
}

/// Looks up a `bool` argument of `cmd` by key.
fn arg_bool(cmd: &Command, key: &str) -> Option<bool> {
    cmd.args.iter().find_map(|kv| match kv.value {
        Value::BoolVal(b) if kv.key == key => Some(b),
        _ => None,
    })
}

oxidhome_sdk::plugin!(Template);

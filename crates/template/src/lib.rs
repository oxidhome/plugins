#[allow(clippy::pedantic, dead_code, unused_imports, clippy::all)]
mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        generate_all,
    });
}

use bindings::Guest;
use bindings::oxidhome::plugin::devices::{Command, CommandResult};
use bindings::oxidhome::plugin::events::Event;
use bindings::oxidhome::plugin::logging::{self, Level};
use bindings::oxidhome::plugin::types::DeviceId;

struct Component;

impl Guest for Component {
    fn init() -> Result<(), String> {
        logging::log(Level::Info, "template-plugin: init");
        Ok(())
    }

    fn shutdown() {
        logging::log(Level::Info, "template-plugin: shutdown");
    }

    fn on_event(_ev: Event) {}

    fn execute_command(_device: DeviceId, _cmd: Command) -> CommandResult {
        CommandResult::Ok
    }

    fn tick() {}
}

bindings::export!(Component with_types_in bindings);

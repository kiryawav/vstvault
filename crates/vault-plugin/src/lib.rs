mod editor;
pub mod events;
mod foreground;
mod host_objects;
mod identity;
mod module;
mod plugin;

pub use editor::Editor;
pub use identity::read_identity;
pub use module::Module;
pub use plugin::{EventSink, Plugin, PluginAudio};
pub use vst3::Steinberg::Vst::{Event, ProcessContext};

use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

pub fn end_process(code: u32) -> ! {
    unsafe { TerminateProcess(GetCurrentProcess(), code) };
    unreachable!("TerminateProcess does not return for the current process")
}

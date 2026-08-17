mod executor;
mod install;
mod manifest;
pub mod process;

pub use executor::ShellExecutor;
pub use manifest::{
    DEFAULT_COMMANDS, InstallMethod, MANIFEST_FILENAME, ShellCommand, ShellInvocation, ShellOption,
    ShellSecret, ShellToolManifest,
};

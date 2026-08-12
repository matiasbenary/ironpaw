mod executor;
mod install;
mod manifest;
pub mod process;
pub mod proxy_bridge;

pub use executor::ShellExecutor;
pub use manifest::{
    DEFAULT_COMMANDS, InstallMethod, MANIFEST_FILENAME, ShellCommand, ShellGitCredentialHelper,
    ShellInvocation, ShellOption, ShellSecret, ShellToolManifest,
};

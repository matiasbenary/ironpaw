use std::{
    env,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct RuntimeLayout {
    root: PathBuf,
}

impl RuntimeLayout {
    pub fn discover() -> Self {
        let root = env::var_os("WARDEN_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(default_runtime_root);
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn channels_dir(&self) -> PathBuf {
        self.root.join("channels")
    }
}

fn default_runtime_root() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

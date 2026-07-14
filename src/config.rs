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

    pub fn from_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn channels_dir(&self) -> PathBuf {
        self.root.join("channels")
    }

    pub fn tools_dir(&self) -> PathBuf {
        self.root.join("tools")
    }

    pub fn files_dir(&self) -> PathBuf {
        self.root.join("files")
    }

    pub fn download_dir(&self) -> PathBuf {
        self.root.join("download")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }
}

fn default_runtime_root() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

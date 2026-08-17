pub(crate) mod client;
pub(crate) mod email;
pub mod http;
pub(crate) mod oauth;
pub(crate) mod policy;
pub(crate) mod proxy;
pub mod proxy_bridge;
pub(crate) mod public;
pub(crate) mod server;

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    fn files(path: &Path, output: &mut Vec<std::path::PathBuf>) {
        for entry in fs::read_dir(path).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                files(&path, output);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                output.push(path);
            }
        }
    }

    #[test]
    fn outbound_boundary_is_preserved() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut source = Vec::new();
        files(&root.join("src"), &mut source);
        for path in source {
            if !path.starts_with(root.join("src/network")) {
                let text = fs::read_to_string(&path).unwrap();
                for primitive in [
                    "reqwest::Client",
                    "reqwest::blocking::Client",
                    "reqwest::blocking::get",
                    "imap::ClientBuilder",
                    "SmtpTransport",
                ] {
                    assert!(
                        !text.contains(primitive),
                        "{primitive} escaped into {}",
                        path.display()
                    );
                }
            }
        }
    }
}

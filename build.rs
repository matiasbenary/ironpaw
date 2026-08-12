use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=GOOGLE_CLIENT_ID");
    println!("cargo:rerun-if-env-changed=GOOGLE_CLIENT_SECRET");
    println!("cargo:rerun-if-env-changed=MICROSOFT_CLIENT_ID");
    println!("cargo:rerun-if-changed=.env");

    let dotenv =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory")).join(".env");
    compile_client_id(&dotenv, "GOOGLE_CLIENT_ID", "IRONPAW_GOOGLE_CLIENT_ID");
    compile_client_id(
        &dotenv,
        "GOOGLE_CLIENT_SECRET",
        "IRONPAW_GOOGLE_CLIENT_SECRET",
    );
    compile_client_id(
        &dotenv,
        "MICROSOFT_CLIENT_ID",
        "IRONPAW_MICROSOFT_CLIENT_ID",
    );
}

fn compile_client_id(dotenv: &std::path::Path, source: &str, target: &str) {
    if let Some(client_id) = env::var(source)
        .ok()
        .or_else(|| dotenv_value(dotenv, source))
    {
        assert!(
            !client_id.is_empty() && !client_id.chars().any(char::is_whitespace),
            "{source} must be a non-empty single-line value"
        );
        println!("cargo:rustc-env={target}={client_id}");
    }
}

fn dotenv_value(path: &std::path::Path, name: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .find_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            (key.trim() == name).then(|| {
                value
                    .trim()
                    .trim_matches(|character| character == '\'' || character == '"')
                    .to_string()
            })
        })
}

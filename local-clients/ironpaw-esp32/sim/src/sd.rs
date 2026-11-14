//! The TF card, as the `sdcard/` folder next to this crate: the offline copies and the error
//! log, over plain `std::fs`.

pub const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/sdcard");
pub const LOG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/sdcard/ironpaw.log");

/// Writes `text` to `sdcard/<name>` (a whole-file replace).
pub fn save(name: &str, text: &str) {
    if let Err(e) = std::fs::write(format!("{ROOT}/{name}"), text) {
        log::warn!("sd: write {name}: {e}");
    }
}

pub fn load(name: &str) -> Option<String> {
    std::fs::read_to_string(format!("{ROOT}/{name}")).ok()
}

/// One line at the end of the error log.
pub fn log_line(line: &str) {
    use std::io::Write as _;
    let file = std::fs::OpenOptions::new().create(true).append(true).open(LOG);
    if let Err(e) = file.and_then(|mut f| writeln!(f, "{line}")) {
        log::warn!("sd: log: {e}");
    }
}

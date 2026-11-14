//! Host simulator of the board: the firmware's `app.rs` (and the screen, pet and
//! protocol code) running on the PC against a real Ironpaw server, with the hardware faked.
//! `IRONPAW_CLIENT_TOKEN=... cargo +stable run`, then type commands (see `board::USAGE`) and
//! look at `out/screen.ppm`. See "Simulator" in the firmware's README.

#[path = "../../src/app.rs"]
mod app;
#[path = "../../src/clock.rs"]
mod clock;
#[path = "../../src/pet.rs"]
mod pet;
#[path = "../../src/pet_data.rs"]
mod pet_data;
#[path = "../../src/proto.rs"]
mod proto;
#[path = "../../src/ui.rs"]
mod ui;
#[path = "../../src/wav.rs"]
mod wav;

mod audio;
mod board;
mod net;
mod nvs;
mod sd;
mod web;

use board::Board;
use std::{sync::OnceLock, time::Instant};

static START: OnceLock<Instant> = OnceLock::new();

fn uptime_ms() -> u32 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u32
}

/// Prints the log to stderr: `log` has no sink of its own and no dependency is worth adding.
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!("[{}] {}", record.level(), record.args());
    }
    fn flush(&self) {}
}

fn main() -> anyhow::Result<()> {
    START.get_or_init(Instant::now);
    let _ = log::set_logger(&Logger);
    log::set_max_level(log::LevelFilter::Info);
    std::fs::create_dir_all(sd::ROOT)?;
    let mut board = Board::new();
    let mut app = app::App::new();
    app.show(&mut board);
    if let Err(e) = net::init() {
        app.blocked(&mut board, "Not set up", e.to_string());
        eprintln!("{e}");
        std::process::exit(1);
    }
    app.restore();
    let (tx, rx) = std::sync::mpsc::channel();
    net::spawn_events(tx)?;
    app.connecting();
    println!("{}", board::USAGE);
    web::spawn()?;
    board::spawn_stdin();
    app::run(&mut app, &mut board, rx)
}

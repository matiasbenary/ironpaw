//! The settings the board keeps in flash, kept in memory here (they reset with every run).

use std::{sync::Mutex, time::Instant};

#[derive(Default)]
struct State {
    offset: i32,
    /// The Unix time Ironpaw last gave, and when.
    clock: Option<(i64, Instant)>,
}

static STATE: Mutex<State> = Mutex::new(State { offset: 0, clock: None });

pub fn saved_offset() -> i32 {
    STATE.lock().unwrap().offset
}

/// Sets the clock to `unix` (it keeps running from there) and remembers the zone offset.
pub fn set_time(unix: i64, offset: i32) {
    let mut state = STATE.lock().unwrap();
    state.clock = Some((unix, Instant::now()));
    state.offset = offset;
}

/// Seconds since the Unix epoch, UTC.
pub fn unix_now() -> i64 {
    if let Some((unix, at)) = STATE.lock().unwrap().clock {
        return unix + at.elapsed().as_secs() as i64;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

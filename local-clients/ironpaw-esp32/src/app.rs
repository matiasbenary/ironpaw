//! The app: what the conversation is doing and what each input does to it. Everything with a pin
//! is behind `Board`, so this file has no ESP-IDF in it and also runs in the host simulator.

use crate::{
    audio, clock,
    net::{self, Event, Incoming},
    nvs,
    pet::{self, Mood},
    sd,
    ui::{self, Link},
    uptime_ms, Board,
};
use anyhow::Result;
use std::{
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

// ponytail: fixed recording cap. 10 s = 320 KB in PSRAM; raise it if longer speech is needed.
const MAX_SECS: usize = 10;
pub const MAX_BYTES: usize = audio::RATE as usize * 2 * MAX_SECS;
/// Anything shorter is treated as an accidental press.
const MIN_SECS: f32 = 0.4;
const READY: &str = "Hold + and talk";
// ponytail: every line fits in 28 columns (FONT_8X13), so the help is one page.
const HELP: &str = "Hold + and talk.\n\
    Everything goes to Ironpaw:\n\
    notes and reminders.\n\
    BOOT: your notes.\n\
    Tap: top back, mid read\n\
    aloud, bottom next.";
/// Hold PWR this long to power off.
const PWR_HOLD: Duration = Duration::from_millis(2500);
/// How long the pet stays happy; sad lasts twice as long.
const MOOD_TIME: Duration = Duration::from_secs(3);
const DIM_AFTER: Duration = Duration::from_secs(30);
/// Idle this long after the last reply or touch and the pet takes the whole screen.
const PET_AFTER: Duration = Duration::from_secs(20);
/// Backlight at all times, in percent (the pet still sleeps when idle). Some panels flicker below ~5%.
pub const BRIGHTNESS: u32 = 8;
/// Longest the board waits for the agent before giving up.
const THINKING_TIMEOUT: Duration = Duration::from_secs(120);
const WIFI_CHECK: Duration = Duration::from_secs(10);
/// Often enough to notice a cable going in.
const BATTERY_CHECK: Duration = Duration::from_secs(10);
/// How often the events are re-read from Ironpaw (they're also re-read after every reply, when
/// the agent may have edited them).
const EVENTS_REFRESH: Duration = Duration::from_secs(10 * 60);
/// A reminder older than this is missed, not rung: it keeps a restart from replaying the day.
const FIRE_WINDOW: i64 = 120;
const EVENTS_CACHE: &str = "events.json";
const NOTES_CACHE: &str = "notes.json";
/// The mic is read in blocks of this many bytes (`audio::Chunk`).
pub const CHUNK: usize = 1024;

/// What the conversation is doing right now. Exactly one at a time.
enum Mode {
    Idle,
    /// Waiting on the agent, since the last sign of life from it.
    Thinking(Instant),
    /// A question with options on screen: `(label, value)` pairs. Answering sends the value.
    Choosing(Vec<(String, String)>),
    /// The paged list view (notes or events); `list` is "Note" or "Event".
    Reading { list: &'static str, i: usize },
    /// A short press cut the reply short; "Stopped" shows for a moment.
    Stopped(Instant),
}

/// What the board knows about its power: the battery's charge and whether USB power is on.
pub struct Power {
    pub percent: Option<u8>,
    pub plugged: bool,
}

/// The conversation's state and what each input does to it.
pub struct App {
    status: String,
    /// What you said, or a hint (the undo window).
    you: String,
    reply: String,
    page: usize,
    link: Link,
    /// The raw error behind a friendly message, shown on a middle tap.
    error: Option<String>,
    mood: Mood,
    mode: Mode,
    /// Resting: the pet fills the screen. A tap brings the text back.
    rest: bool,
    /// What `Mode::Reading` pages through.
    notes: Vec<String>,
    /// Reminders from Ironpaw (or the SD copy), soonest first.
    events: Vec<Event>,
    /// Local times already rung, so a refresh inside `FIRE_WINDOW` doesn't ring twice.
    rung: Vec<i64>,
    events_checked: Instant,
    active: Instant,
    dimmed: bool,
    pages: usize,
    /// What is on the panel: the screen and the pet's animation frame (which `ui::draw` takes
    /// from the clock, so it isn't in the screen). Redrawn only when it changes.
    last: (ui::Screen, usize),
    /// The mood as the last tick saw it, and since when, so happy and sad can wear off.
    mood_at: (Mood, Instant),
    battery: Option<u8>,
    plugged: bool,
}

impl App {
    /// The state at power-on, before WiFi.
    pub fn new() -> Self {
        App {
            status: "Connecting WiFi...".into(),
            you: String::new(),
            reply: String::new(),
            page: 0,
            link: Link::default(),
            error: None,
            mood: Mood::Idle,
            mode: Mode::Idle,
            rest: false,
            notes: Vec::new(),
            events: Vec::new(),
            rung: Vec::new(),
            events_checked: Instant::now(),
            active: Instant::now(),
            dimmed: false,
            pages: 1,
            last: Default::default(),
            mood_at: (Mood::Idle, Instant::now()),
            battery: None,
            plugged: false,
        }
    }

    /// Startup that could not go on: says why on screen. The caller then bails out.
    pub fn blocked(&mut self, board: &mut Board, status: &str, reply: String) {
        self.status = status.into();
        self.reply = reply;
        self.show(board);
    }

    /// The reminders from the last time Ironpaw was reached, so an offline start still rings.
    /// The clock keeps running across restarts while the board has power.
    pub fn restore(&mut self) {
        if let Some(json) = sd::load(EVENTS_CACHE).and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok()) {
            self.events = net::parse_events(&json);
            log::info!("events: {} from the SD copy", self.events.len());
        }
    }

    /// WiFi is up: the help stays on screen until Ironpaw answers.
    pub fn connecting(&mut self) {
        self.status = "Connecting to Ironpaw...".into();
        self.reply = HELP.into();
    }

    /// Any input or reply counts as activity. Returns whether the screen was dim, so the
    /// tap that wakes it doesn't also act.
    fn wake(&mut self, board: &mut Board) -> bool {
        self.active = Instant::now();
        let was = self.dimmed;
        if was {
            self.dimmed = false;
            self.mood = Mood::Idle;
            board.backlight(BRIGHTNESS);
        }
        was
    }

    /// The screen for the current mode: the notes list, the resting pet with the clock, or the chat.
    fn render(&self) -> ui::Screen {
        match (&self.mode, self.rest) {
            (Mode::Reading { list, i }, _) => ui::Screen { battery: self.battery, plugged: self.plugged, ..note_screen(list, &self.notes, *i, self.link) },
            // Resting, the header shows the clock (when there is one).
            (_, true) => ui::Screen {
                status: self.now().map(clock::hhmm).unwrap_or_else(|| self.status.clone()),
                link: self.link,
                pet: self.mood,
                battery: self.battery,
                plugged: self.plugged,
                full: true,
                ..Default::default()
            },
            (mode, false) => ui::Screen {
                status: self.status.clone(),
                you: self.you.clone(),
                reply: self.reply.clone(),
                page: self.page,
                link: self.link,
                options: match mode {
                    Mode::Choosing(choice) => choice.iter().map(|(label, _)| label.clone()).collect(),
                    _ => Vec::new(),
                },
                pet: self.mood,
                error: self.error.clone(),
                battery: self.battery,
                plugged: self.plugged,
                ..Default::default()
            },
        }
    }

    /// `render()` plus the pet's frame. The notes list has no animation.
    fn frame(&self) -> (ui::Screen, usize) {
        let screen = self.render();
        let pose = if matches!(self.mode, Mode::Reading { .. }) { 0 } else { pet::pose(screen.pet, uptime_ms()) };
        (screen, pose)
    }

    /// Draws now, for the middle of an operation, and remembers it.
    pub fn show(&mut self, board: &mut Board) {
        let frame = self.frame();
        self.pages = board.show(&frame.0);
        self.last = frame;
    }

    /// Like `Board::say_paged` on a copy of the screen; the page it ends on is kept.
    fn say_paged(&mut self, board: &mut Board, text: &str) -> Result<()> {
        let mut screen = self.render();
        let spoken = board.say_paged(&mut screen, self.pages, text);
        self.page = screen.page;
        spoken
    }

    /// Shows an error as what to do about it. The raw one goes to the serial log.
    fn fail(&mut self, error: &anyhow::Error) {
        log::warn!("ironpaw: {error}");
        let when = clock::local(nvs::unix_now(), nvs::saved_offset()).map(clock::stamp).unwrap_or_else(|| format!("+{}s", uptime_ms() / 1000));
        sd::log_line(&format!("{when} {error}"));
        let (title, hint, link) = friendly(&error.to_string());
        self.status = title.into();
        self.reply = hint.into();
        self.error = Some(error.to_string());
        self.mood = Mood::Sad;
        if let Some(link) = link {
            self.link = link;
        }
    }

    /// Back to the plain chat view. A pending note or undo window carries on.
    fn to_chat(&mut self) {
        self.rest = false;
        if matches!(self.mode, Mode::Thinking(_) | Mode::Choosing(_) | Mode::Reading { .. } | Mode::Stopped(_)) {
            self.mode = Mode::Idle;
        }
    }

    /// Reads the power. Plugging in makes the pet eat and the screen say how full the battery is.
    fn poll_power(&mut self, board: &mut Board) {
        let power = board.power();
        self.battery = power.percent;
        if power.plugged && !self.plugged {
            self.wake(board);
            self.mood = Mood::Eating;
            self.status = match power.percent {
                Some(percent) => format!("Charging {percent}%"),
                None => "Charging".into(),
            };
        }
        self.plugged = power.plugged;
    }

    fn stop_thinking(&mut self) {
        if matches!(self.mode, Mode::Thinking(_)) {
            self.mode = Mode::Idle;
        }
    }

    fn on_event(&mut self, board: &mut Board, incoming: Incoming) -> Result<()> {
        match incoming {
            Incoming::Connected => {
                log::info!("ironpaw: connected");
                self.link = Link::Online;
                self.status = READY.into();
                self.refresh_events();
            }
            Incoming::Disconnected(e) => {
                log::warn!("ironpaw: disconnected: {e}");
                self.link = Link::Offline;
                self.status = "Ironpaw offline".into();
                self.reply = "Check that it's running on the PC and on this WiFi.".into();
                // A rejected token is worth saying: retrying won't fix it.
                if let ("Wrong token", hint, _) = friendly(&e) {
                    self.status = "Wrong token".into();
                    self.reply = hint.into();
                }
                self.stop_thinking();
            }
            Incoming::Thinking => {
                self.mood = Mood::Thinking;
                // Each activity restarts the clock: the timeout is for silence, not for long turns.
                // A pending note, question or notes list keeps its place.
                if matches!(self.mode, Mode::Idle | Mode::Thinking(_) | Mode::Stopped(_)) {
                    self.mode = Mode::Thinking(Instant::now());
                }
            }
            Incoming::Reply(text) => {
                log::info!("ironpaw: reply of {} characters", text.len());
                self.show_reply(board, text, Vec::new())?;
                // The agent may have added or moved a reminder.
                self.refresh_events();
            }
            Incoming::Failed(text) => {
                self.fail(&anyhow::anyhow!(text));
                self.stop_thinking();
            }
            Incoming::Choice(question, options) => {
                log::info!("ironpaw: question with {} options", options.len());
                let options = options.into_iter().take(ui::MAX_OPTIONS).collect();
                self.show_reply(board, question, options)?;
            }
        }
        Ok(())
    }

    fn show_reply(&mut self, board: &mut Board, text: String, options: Vec<(String, String)>) -> Result<()> {
        self.wake(board);
        self.to_chat();
        self.status = "Speaking...".into();
        self.mood = Mood::Talking;
        self.reply = text;
        self.error = None;
        self.page = 0;
        let labels: Vec<String> = options.iter().map(|(label, _)| label.clone()).collect();
        let spoken = match labels.as_slice() {
            [] => self.reply.clone(),
            labels => format!("{} Options: {}.", self.reply, labels.join(", ")),
        };
        if !options.is_empty() {
            self.mode = Mode::Choosing(options);
        }
        self.show(board);
        let spoken = self.say_paged(board, &spoken);
        self.mood = Mood::Idle;
        self.active = Instant::now();
        spoken?;
        self.status = if matches!(self.mode, Mode::Choosing(_)) { "Tap an answer or say it" } else { READY }.into();
        Ok(())
    }

    fn on_tap(&mut self, board: &mut Board, x: i32, y: i32) -> Result<()> {
        if self.wake(board) {
            return Ok(());
        }
        // The resting view: any tap shows the text. In the text, tapping the pet rests again.
        if self.rest {
            self.rest = false;
            return Ok(());
        }
        let choosing = matches!(self.mode, Mode::Choosing(_));
        if !choosing && !matches!(self.mode, Mode::Reading { .. }) && ui::pet_hit(x, y) {
            self.rest = true;
            return Ok(());
        }
        if let Mode::Choosing(choice) = &self.mode {
            if let Some(i) = ui::option_at(y, choice.len()) {
                let (label, value) = choice[i].clone();
                return self.send(&label, &value);
            }
        }
        let third = y * 3 / ui::H as i32;
        if let Mode::Reading { i, .. } = &mut self.mode {
            let n = self.notes.len();
            match third {
                1 => {
                    if let Some(note) = self.notes.get(*i) {
                        board.say(note)?;
                    }
                }
                0 if n > 0 => *i = (*i + n - 1) % n,
                _ if n > 0 => *i = (*i + 1) % n,
                _ => {}
            }
            return Ok(());
        }
        let pages = self.pages;
        match third {
            // On an error, the middle shows the raw text instead of reading the hint aloud.
            1 if self.error.is_some() => {
                self.status = "Error details".into();
                self.reply = self.error.take().unwrap_or_default();
            }
            1 if !self.reply.is_empty() => {
                let text = self.reply.clone();
                self.say_paged(board, &text)?;
            }
            0 => self.page = (self.page + pages - 1) % pages,
            _ => self.page = (self.page + 1) % pages,
        }
        Ok(())
    }

    /// BOOT: toggle chat and notes.
    fn on_boot(&mut self, board: &mut Board) -> Result<()> {
        if self.wake(board) {
            return Ok(());
        }
        if matches!(self.mode, Mode::Reading { .. }) {
            self.mode = Mode::Idle;
        } else {
            self.rest = false;
            self.status = "Loading notes...".into();
            self.show(board);
            self.open_notes();
        }
        Ok(())
    }

    fn open_notes(&mut self) {
        let list = match net::list_notes() {
            Ok(list) => {
                sd::save(NOTES_CACHE, &serde_json::Value::from(list.clone()).to_string());
                self.status = READY.into();
                list
            }
            // Offline: the copy on the SD card, if there is one.
            Err(e) => match sd::load(NOTES_CACHE).and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok()) {
                Some(list) => {
                    log::warn!("notes: {e}; showing the SD copy");
                    self.status = "Offline copy".into();
                    list
                }
                None => return self.fail(&e),
            },
        };
        self.show_list("Note", list);
    }

    /// The paged list view (notes or events), newest or soonest first.
    fn show_list(&mut self, title: &'static str, list: Vec<String>) {
        self.notes = list;
        self.rest = false;
        self.mode = Mode::Reading { list: title, i: 0 };
    }


    /// Local time in seconds, once Ironpaw has set the clock.
    fn now(&self) -> Option<i64> {
        clock::local(nvs::unix_now(), nvs::saved_offset())
    }

    /// Re-reads the reminders from Ironpaw (which also sets the clock) and keeps a copy on
    /// the SD card. Quiet on failure: the last list stands.
    fn refresh_events(&mut self) {
        self.events_checked = Instant::now();
        match net::list_events() {
            Ok((events, json)) => {
                log::info!("events: {} upcoming, clock {}", events.len(), self.now().map(clock::stamp).unwrap_or_default());
                self.events = events;
                sd::save(EVENTS_CACHE, &json);
            }
            Err(e) => log::warn!("events: {e}"),
        }
    }

    /// The reminder that is due now, if any.
    fn due(&mut self) -> Option<Event> {
        let now = self.now()?;
        let due = |e: &Event| (now - FIRE_WINDOW..=now).contains(&e.at);
        let event = self.events.iter().find(|e| due(e) && !self.rung.contains(&e.at))?.clone();
        self.rung.push(event.at);
        self.rung.retain(|&at| at >= now - 2 * FIRE_WINDOW);
        Some(event)
    }

    /// Rings (a dog barking), then reads the text. The text stays on screen.
    fn ring(&mut self, board: &mut Board, event: Event) -> Result<()> {
        log::info!("reminder: {}", event.text);
        self.wake(board);
        self.to_chat();
        self.status = "Reminder".into();
        self.you = clock::hhmm(event.at);
        self.reply = event.text.clone();
        self.error = None;
        self.page = 0;
        self.mood = Mood::Happy;
        self.show(board);
        board.bark(6);
        board.say(&event.text)?;
        self.mood = Mood::Idle;
        self.status = READY.into();
        self.active = Instant::now();
        Ok(())
    }

    fn on_talk(&mut self, board: &mut Board, pcm: &mut Vec<u8>) -> Result<bool> {
        self.wake(board);
        log::info!("button: pressed, recording");
        self.rest = false;
        if matches!(self.mode, Mode::Reading { .. } | Mode::Stopped(_)) {
            self.mode = Mode::Idle;
        }
        self.status = "Listening...".into();
        self.mood = Mood::Listening;
        board.beep(880);
        // The level bar goes on a copy of the screen: `record` draws it between mic reads.
        let released = board.record(&mut self.render(), pcm);
        board.beep(440);
        let speech = audio::trim_silence(pcm);
        let secs = pcm.len() as f32 / (audio::RATE as f32 * 2.0);
        log::info!("recording: {secs:.1} s, {:.1} s after trimming, released: {released}", speech.len() as f32 / (audio::RATE as f32 * 2.0));
        if secs < MIN_SECS {
            self.mood = Mood::Idle;
            // A short press while a reply is shown means "stop talking": keep the reply.
            if self.reply.is_empty() {
                self.status = "Too short".into();
                self.reply = "Keep + held down the whole time you talk.".into();
            } else if matches!(self.mode, Mode::Idle) {
                self.mode = Mode::Stopped(Instant::now());
                self.status = "Stopped".into();
            }
            return Ok(released);
        }
        self.you = String::new();
        self.reply = String::new();
        self.error = None;
        self.page = 0;
        let speech = &pcm[speech];
        self.status = "Transcribing...".into();
        self.mood = Mood::Thinking;
        self.show(board);
        match net::transcribe(&audio::wav_header(speech.len()), speech) {
            Err(e) => self.fail(&e),
            Ok(text) => {
                log::info!("transcript: {text}");
                self.link = Link::Online;
                self.send(&text, &text)?;
                // Done unless the agent is still working on it.
                if self.mood == Mood::Thinking && !matches!(self.mode, Mode::Thinking(_)) {
                    self.mood = Mood::Idle;
                }
            }
        }
        Ok(released)
    }

    /// Sends a message to the agent. `shown` is what goes on screen (an option's label).
    fn send(&mut self, shown: &str, text: &str) -> Result<()> {
        self.you = shown.to_string();
        if matches!(self.mode, Mode::Choosing(_)) {
            self.mode = Mode::Idle;
        }
        match net::send_message(text) {
            Ok(()) => self.mode = Mode::Thinking(Instant::now()),
            Err(e) => self.fail(&e),
        }
        Ok(())
    }

    /// Everything time-based, then the redraw.
    fn tick(&mut self, board: &mut Board) {
        if let Some(event) = self.due() {
            if let Err(e) = self.ring(board, event) {
                log::warn!("reminder: {e}");
            }
        }
        if self.link == Link::Online && self.events_checked.elapsed() >= EVENTS_REFRESH {
            self.refresh_events();
        }
        match self.mode {
            Mode::Thinking(since) if since.elapsed() >= THINKING_TIMEOUT => {
                self.mode = Mode::Idle;
                self.status = "No reply".into();
                self.reply = "Ironpaw did not answer. Try again.".into();
                self.mood = Mood::Sad;
            }
            // Waiting on the agent: dots and seconds, so it doesn't look frozen.
            Mode::Thinking(since) => {
                let dots = ".".repeat(since.elapsed().as_millis() as usize / 400 % 4);
                let status = format!("Thinking{dots:<3} {}s", since.elapsed().as_secs());
                self.status = status;
            }
            Mode::Stopped(since) if since.elapsed() >= Duration::from_secs(2) => {
                self.mode = Mode::Idle;
                self.status = READY.into();
            }
            _ => {}
        }
        let busy = matches!(self.mode, Mode::Thinking(_) | Mode::Choosing(_));
        if !self.rest && !busy && !matches!(self.mode, Mode::Reading { .. }) && self.active.elapsed() >= PET_AFTER {
            self.rest = true;
        }
        if !self.dimmed && !matches!(self.mode, Mode::Thinking(_)) && self.active.elapsed() >= DIM_AFTER {
            self.dimmed = true;
            self.mood = Mood::Sleep;
            board.backlight(BRIGHTNESS);
        }
        // Happy and sad wear off; anything else moves the pet on by itself.
        if self.mood != self.mood_at.0 {
            self.mood_at = (self.mood, Instant::now());
        }
        let calm = match self.mood {
            Mood::Happy => Some(MOOD_TIME),
            Mood::Sad | Mood::Eating => Some(2 * MOOD_TIME),
            _ => None,
        };
        if calm.is_some_and(|t| self.mood_at.1.elapsed() >= t) {
            self.mood = Mood::Idle;
            if self.status.starts_with("Charging") {
                self.status = READY.into();
            }
        }
        // The clock in the resting view and the pet's frame change the screen by themselves.
        let frame = self.frame();
        if frame != self.last {
            self.pages = board.show(&frame.0);
            self.last = frame;
        }
    }
}

/// The main loop: events, taps, buttons, the link check, the battery and the clock, forever.
pub fn run(app: &mut App, board: &mut Board, rx: Receiver<Incoming>) -> Result<()> {
    // One block of headroom so the last one doesn't force a 320 KB reallocation.
    let mut pcm = Vec::with_capacity(MAX_BYTES + CHUNK);
    let mut was_pressed = false;
    let mut was_boot = false;
    let mut pwr_since: Option<Instant> = None;
    let mut was_touching = false;
    let mut wifi_checked = Instant::now();
    let mut battery_checked = Instant::now();
    // Silent first read: starting on USB is not "plugging in".
    let power = board.power();
    (app.battery, app.plugged) = (power.percent, power.plugged);
    loop {
        board.feed();
        for incoming in rx.try_iter() {
            app.on_event(board, incoming)?;
        }

        // Taps: on finger down only. The CST816 sometimes drops a held finger for a read,
        // which shows up as a second tap at the same spot: harmless here.
        let touch = board.touch();
        if let (Some((x, y)), false) = (touch, was_touching) {
            app.on_tap(board, x, y)?;
        }
        was_touching = touch.is_some();

        let boot_down = board.boot_down();
        if boot_down && !was_boot {
            app.on_boot(board)?;
        }
        was_boot = boot_down;

        // PWR: hold to power off. The latch only matters on battery: on USB the board stays
        // up, so say so instead of pretending.
        pwr_since = match (board.pwr_down(), pwr_since) {
            (true, None) => Some(Instant::now()),
            (true, Some(since)) if since.elapsed() >= PWR_HOLD => {
                app.status = "Bye!".into();
                app.mood = Mood::Sleep;
                app.show(board);
                board.power_off()?;
                std::thread::sleep(Duration::from_millis(300));
                app.status = "On USB: unplug to turn off".into();
                app.show(board);
                None
            }
            (true, since) => since,
            (false, _) => None,
        };

        // Only on press: recording on level would make the 10 s cap with the button held
        // trigger recordings in a loop.
        let held = board.key_down();
        if held && !was_pressed {
            // If the cap hit with the button held, don't record again until it's released.
            was_pressed = !app.on_talk(board, &mut pcm)?;
        } else {
            was_pressed = held;
        }

        if wifi_checked.elapsed() >= WIFI_CHECK {
            wifi_checked = Instant::now();
            if !board.link_up() {
                log::warn!("wifi: down, reconnecting");
                app.link = Link::Offline;
                app.status = "Reconnecting WiFi...".into();
                app.show(board);
                match board.reconnect() {
                    Ok(()) => app.status = READY.into(),
                    Err(e) => log::warn!("wifi: {e}"),
                }
            }
        }
        if battery_checked.elapsed() >= BATTERY_CHECK {
            battery_checked = Instant::now();
            app.poll_power(board);
        }
        app.tick(board);
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn friendly(error: &str) -> (&'static str, &'static str, Option<Link>) {
    let e = error.to_lowercase();
    if e.contains("locked") || e.contains("api_key") {
        ("Ironpaw is locked", "Unlock it in the WebUI, then try again.", Some(Link::Locked))
    } else if e.contains("no speech") {
        ("Didn't catch that", "Hold + and talk a bit closer to the board.", None)
    } else if ["401", "403"].iter().any(|code| e.starts_with(code) || e.contains(&format!("http {code}"))) {
        ("Wrong token", "Set the board up again with Ironpaw's current token.", None)
    } else if e.starts_with("404") || e.starts_with("405") {
        ("Ironpaw is out of date", "Restart Ironpaw so it has the latest routes.", None)
    } else if e.contains("connect") || e.contains("timeout") || e.contains("timed out") || e.contains("eagain") {
        ("Can't reach Ironpaw", "Check that it's running on the PC and on this WiFi.", Some(Link::Offline))
    } else {
        ("Something went wrong", "Try again. Tap the middle of the screen for the details.", None)
    }
}

/// One note (or event) per page: "2026-10-29 10:40 buy milk" shows the date above the text.
fn note_screen(title: &str, notes: &[String], i: usize, link: Link) -> ui::Screen {
    let mut s = ui::Screen { link, ..Default::default() };
    match notes.get(i) {
        None if title == "Event" => {
            s.status = "Nothing coming up".into();
            s.reply = "Ask for a reminder, like \"Remind me of the dentist on Thursday at 10\", or say \"20 minutes\" for a timer.\n\nBOOT: back to chat".into();
        }
        None => {
            s.status = "No notes yet".into();
            s.reply = "Hold + and say \"Note:\" and what to remember, like \"Note: buy milk\".\n\nBOOT: back to chat".into();
        }
        Some(n) => {
            s.status = format!("{title} {}/{}", i + 1, notes.len());
            s.zones = notes.len() > 1;
            let mut parts = n.splitn(3, ' ');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(date), Some(time), Some(text)) if date.len() == 10 && time.len() == 5 => {
                    s.you = format!("{date} {time}");
                    s.reply = text.into();
                }
                _ => s.reply = n.clone(),
            }
        }
    }
    s
}

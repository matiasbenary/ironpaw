//! The board without the hardware: the screen goes to `out/screen.ppm` and to the terminal,
//! the speaker prints what it would do, and the buttons and the touch panel are commands typed
//! on stdin. Same methods as the firmware's `Board`.

use crate::{app::Power, ui, uptime_ms};
use anyhow::Result;
use std::{io::Write, sync::Mutex};

pub const USAGE: &str = "commands: talk <text> | tap <x> <y> | boot | pwr | plug | unplug | q";

/// What was typed and not acted on yet.
#[derive(Default)]
struct Input {
    tap: Option<(i32, i32)>,
    boot: bool,
    /// PWR is held from `pwr` until the app acts on it.
    pwr: bool,
    /// The + button is down, with what is said while it is.
    talk: Option<String>,
    plugged: bool,
}

static INPUT: Mutex<Input> = Mutex::new(Input { tap: None, boot: false, pwr: false, talk: None, plugged: false });

/// The last frame as 240x240 RGBA and the screen's text, for the web page.
pub static FRAME: Mutex<Vec<u8>> = Mutex::new(Vec::new());
pub static TEXT: Mutex<String> = Mutex::new(String::new());

/// One typed command, from stdin or the web page.
pub fn command(line: &str) {
    let mut input = INPUT.lock().unwrap();
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["q"] => std::process::exit(0),
        ["boot"] => input.boot = true,
        ["pwr"] => input.pwr = true,
        ["plug"] => input.plugged = true,
        ["unplug"] => input.plugged = false,
        ["tap", x, y] => match (x.parse(), y.parse()) {
            (Ok(x), Ok(y)) => input.tap = Some((x, y)),
            _ => println!("{USAGE}"),
        },
        ["talk", text @ ..] => input.talk = Some(text.join(" ")),
        _ => println!("{USAGE}"),
    }
}

/// Reads commands from stdin, in a thread of its own.
pub fn spawn_stdin() {
    std::thread::spawn(|| {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            command(&line);
        }
        // stdin closed: nothing more to type.
        std::process::exit(0);
    });
}

pub struct Board {
    fb: ui::Frame,
    /// The screen's text as last printed, so only changes are.
    printed: String,
}

impl Board {
    pub fn new() -> Self {
        Board { fb: ui::Frame::new(), printed: String::new() }
    }

    /// Draws the screen into `out/screen.ppm` and prints its text if that changed.
    pub fn show(&mut self, screen: &ui::Screen) -> usize {
        let pages = ui::draw(&mut self.fb, screen, uptime_ms());
        if let Err(e) = self.save() {
            log::warn!("screen: {e}");
        }
        let text = describe(screen, pages);
        *TEXT.lock().unwrap() = text.clone();
        // The seconds and dots of "Thinking..." change all the time: one line is enough.
        let key = if screen.status.starts_with("Thinking") { text.replace(&screen.status, "Thinking...") } else { text.clone() };
        if key != self.printed {
            println!("{text}");
            self.printed = key;
        }
        pages
    }

    /// The frame as a P6 image, through a temporary file so a viewer never sees half of it.
    fn save(&self) -> std::io::Result<()> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/out");
        std::fs::create_dir_all(dir)?;
        let tmp = format!("{dir}/screen.ppm.tmp");
        let mut out = std::fs::File::create(&tmp)?;
        write!(out, "P6 {} {} 255\n", ui::W, ui::H)?;
        let mut rgba = Vec::with_capacity(ui::W * ui::H * 4);
        for px in self.fb.0.chunks_exact(2) {
            let v = u16::from_be_bytes([px[0], px[1]]);
            let (r, g, b) = ((v >> 11) & 31, (v >> 5) & 63, v & 31);
            let rgb = [(r * 255 / 31) as u8, (g * 255 / 63) as u8, (b * 255 / 31) as u8];
            out.write_all(&rgb)?;
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        *FRAME.lock().unwrap() = rgba;
        std::fs::rename(tmp, format!("{dir}/screen.ppm"))
    }

    pub fn backlight(&mut self, percent: u32) {
        println!("backlight: {percent}%");
    }

    pub fn beep(&mut self, hz: u32) {
        println!("beep: {hz} Hz");
    }

    pub fn bark(&mut self, barks: u32) {
        println!("bark x{barks}");
    }

    pub fn say(&mut self, text: &str) -> Result<()> {
        println!("say: {text}");
        Ok(())
    }

    /// The pages flip instantly: nobody is listening.
    pub fn say_paged(&mut self, _screen: &mut ui::Screen, _pages: usize, text: &str) -> Result<()> {
        self.say(text)
    }

    /// "Records" what was typed after `talk`, then silence, so it is past the minimum length.
    /// Returns whether + was released, which it always is.
    pub fn record(&mut self, _screen: &mut ui::Screen, pcm: &mut Vec<u8>) -> bool {
        pcm.clear();
        pcm.extend(INPUT.lock().unwrap().talk.take().unwrap_or_default().as_bytes());
        pcm.resize(pcm.len() + crate::audio::RATE as usize, 0);
        true
    }

    /// A tap, once.
    pub fn touch(&mut self) -> Option<(i32, i32)> {
        INPUT.lock().unwrap().tap.take()
    }

    /// 80%, plugged in after `plug` and until `unplug`.
    pub fn power(&mut self) -> Power {
        Power { percent: Some(80), plugged: INPUT.lock().unwrap().plugged }
    }

    /// + is down from `talk` until `record` has taken the text.
    pub fn key_down(&self) -> bool {
        INPUT.lock().unwrap().talk.is_some()
    }

    /// BOOT, once.
    pub fn boot_down(&self) -> bool {
        std::mem::take(&mut INPUT.lock().unwrap().boot)
    }

    pub fn pwr_down(&self) -> bool {
        INPUT.lock().unwrap().pwr
    }

    /// On USB the real board stays up; here `plug` stands for USB.
    pub fn power_off(&mut self) -> Result<()> {
        INPUT.lock().unwrap().pwr = false;
        println!("power off");
        if !INPUT.lock().unwrap().plugged {
            std::process::exit(0);
        }
        Ok(())
    }

    pub fn feed(&mut self) {}

    pub fn link_up(&mut self) -> bool {
        true
    }

    pub fn reconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The screen as text.
fn describe(s: &ui::Screen, pages: usize) -> String {
    let link = match s.link {
        ui::Link::Offline => "offline",
        ui::Link::Online => "online",
        ui::Link::Locked => "locked",
    };
    let battery = s.battery.map(|b| format!(", {}{b}%", if s.plugged { "+" } else { "" })).unwrap_or_default();
    let mut out = format!("--- {} ({link}{battery}, pet {:?}{})", s.status, s.pet, if s.full { ", resting" } else { "" });
    if !s.you.is_empty() {
        out += &format!("\n  you: {}", s.you);
    }
    if !s.reply.is_empty() {
        out += &format!("\n  reply: {}", s.reply.replace('\n', "\n         "));
        if pages > 1 {
            out += &format!("\n  page {}/{pages}", s.page + 1);
        }
    }
    if !s.options.is_empty() {
        out += &format!("\n  options: {}", s.options.join(" | "));
    }
    out
}

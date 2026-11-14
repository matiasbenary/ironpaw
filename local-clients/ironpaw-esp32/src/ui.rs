//! The one screen: status and Ironpaw's state on top, what you said and the reply (or a note) below.

use core::convert::Infallible;
use embedded_graphics::{
    mono_font::{
        iso_8859_1::{FONT_6X10, FONT_7X13_BOLD, FONT_8X13},
        MonoTextStyle,
    },
    pixelcolor::{IntoStorage, Rgb565},
    prelude::*,
    primitives::{Circle, PrimitiveStyle, Rectangle, RoundedRectangle, Triangle},
    text::{Baseline, Text},
};

pub const W: usize = 240;
pub const H: usize = 240;

const BG: Rgb565 = Rgb565::new(2, 5, 9);
/// Header bar; tinted by the pet's mood (red on an error, green after a saved note).
const BAR: Rgb565 = Rgb565::new(4, 10, 16);
const BAR_SAD: Rgb565 = Rgb565::new(18, 10, 8);
const BAR_HAPPY: Rgb565 = Rgb565::new(3, 24, 10);
const TITLE: Rgb565 = Rgb565::WHITE;
const ACCENT: Rgb565 = Rgb565::new(31, 44, 4);
/// What you said, hints and page marks: readable but clearly secondary.
const MUTED: Rgb565 = Rgb565::new(14, 32, 22);
const REPLY: Rgb565 = Rgb565::new(29, 59, 29);
const BUTTON: Rgb565 = Rgb565::new(5, 16, 22);
const MARGIN: i32 = 8;
const BAR_H: i32 = 26;
/// Reply text: FONT_8X13 plus 1 px.
const LINE: i32 = 14;
const COLS: usize = (W - 2 * MARGIN as usize) / 8;
/// Secondary text: FONT_6X10 plus 1 px.
const SMALL: i32 = 11;
/// Room kept at the right edge for the page chevrons.
const CHEVRON: i32 = 10;
/// The pet's cell (40 px) at scale 2.
const PET: i32 = 80;
/// Where the small pet sits: tapping it opens the resting view.
pub fn pet_hit(x: i32, y: i32) -> bool {
    (MARGIN..MARGIN + PET).contains(&x) && (BAR_H + 4..BAR_H + 4 + PET).contains(&y)
}

#[derive(Default, Clone, PartialEq)]
pub struct Screen {
    pub status: String,
    pub you: String,
    pub reply: String,
    /// Mic level 0..1 while recording; `None` when not recording.
    pub level: Option<f32>,
    /// Reply page when it doesn't fit on one screen.
    pub page: usize,
    pub link: Link,
    /// Battery charge in percent, shown next to the dot.
    pub battery: Option<u8>,
    /// Charging: the percentage is shown as "+85%".
    pub plugged: bool,
    /// Answers to a question from the agent, drawn as buttons at the bottom.
    pub options: Vec<String>,
    pub pet: crate::pet::Mood,
    /// The raw error behind a friendly message, shown on a middle tap.
    pub error: Option<String>,
    /// Show the page chevrons even on one page (the notes view, with several notes).
    pub zones: bool,
    /// The resting view: only the header and the pet, big.
    pub full: bool,
}

const OPTION_ROW: i32 = 24;
const OPTION_GAP: i32 = 4;
/// At most this many buttons fit under the question.
pub const MAX_OPTIONS: usize = 4;

/// Top of the option buttons, given how many there are.
fn options_top(n: usize) -> i32 {
    H as i32 - MARGIN - n.min(MAX_OPTIONS) as i32 * (OPTION_ROW + OPTION_GAP) + OPTION_GAP
}

/// Which option button is at screen height `y`, if any.
pub fn option_at(y: i32, n: usize) -> Option<usize> {
    let offset = y - options_top(n);
    let i = offset.div_euclid(OPTION_ROW + OPTION_GAP) as usize;
    (offset >= 0 && offset.rem_euclid(OPTION_ROW + OPTION_GAP) < OPTION_ROW && i < n.min(MAX_OPTIONS)).then_some(i)
}

/// Ironpaw's state, as a dot in the top-right corner.
#[derive(Default, Clone, Copy, PartialEq)]
pub enum Link {
    #[default]
    Offline,
    Online,
    Locked,
}

/// Full framebuffer in big-endian RGB565, the format the panel takes over SPI:
/// draw here and send it as is, so there's no flicker and nothing to convert.
pub struct Frame(pub Vec<u8>);

impl Frame {
    pub fn new() -> Self {
        Self(vec![0; W * H * 2])
    }
}

impl OriginDimensions for Frame {
    fn size(&self) -> Size {
        Size::new(W as u32, H as u32)
    }
}

impl DrawTarget for Frame {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I: IntoIterator<Item = Pixel<Rgb565>>>(&mut self, px: I) -> Result<(), Infallible> {
        for Pixel(p, c) in px {
            if (0..W as i32).contains(&p.x) && (0..H as i32).contains(&p.y) {
                let i = (p.y as usize * W + p.x as usize) * 2;
                self.0[i..i + 2].copy_from_slice(&c.into_storage().to_be_bytes());
            }
        }
        Ok(())
    }

    // Filled circles and rectangles arrive here row by row: painting spans instead of
    // pixel by pixel is the biggest drawing speedup
    fn fill_solid(&mut self, area: &Rectangle, c: Rgb565) -> Result<(), Infallible> {
        let area = area.intersection(&self.bounding_box());
        let Some(br) = area.bottom_right() else { return Ok(()) };
        let b = c.into_storage().to_be_bytes();
        for y in area.top_left.y..=br.y {
            let row = y as usize * W;
            for px in self.0[(row + area.top_left.x as usize) * 2..(row + br.x as usize + 1) * 2].chunks_exact_mut(2) {
                px.copy_from_slice(&b);
            }
        }
        Ok(())
    }

    fn clear(&mut self, c: Rgb565) -> Result<(), Infallible> {
        let b = c.into_storage().to_be_bytes();
        for px in self.0.chunks_exact_mut(2) {
            px.copy_from_slice(&b);
        }
        Ok(())
    }
}

/// Draws the screen and returns how many pages the reply has. `ms` drives the pet's animation.
///
/// Layout: a header bar (status, link dot); the pet, always at the same spot under it, with
/// a column beside it for what matters right now (the mic level while recording, what you
/// said, or the agent's question when there are buttons); then the reply in a larger font,
/// or the answer buttons. Chevrons at the right edge show there are more pages.
pub fn draw(f: &mut Frame, s: &Screen, ms: u32) -> usize {
    use crate::pet::Mood;
    f.clear(BG).ok();
    let bar = match s.pet {
        Mood::Sad => BAR_SAD,
        Mood::Happy => BAR_HAPPY,
        _ => BAR,
    };
    f.fill_solid(&Rectangle::new(Point::zero(), Size::new(W as u32, BAR_H as u32)), bar).ok();
    let title = MonoTextStyle::new(&FONT_7X13_BOLD, TITLE);
    Text::with_baseline(&s.status, Point::new(MARGIN, (BAR_H - 13) / 2), title, Baseline::Top).draw(f).ok();
    let dot = match s.link {
        Link::Online => Rgb565::new(6, 52, 12),
        Link::Locked => ACCENT,
        Link::Offline => Rgb565::new(31, 10, 6),
    };
    Circle::new(Point::new(W as i32 - MARGIN - 9, (BAR_H - 9) / 2), 9)
        .into_styled(PrimitiveStyle::with_fill(dot))
        .draw(f)
        .ok();
    if let Some(battery) = s.battery {
        let label = format!("{}{battery}%", if s.plugged { "+" } else { "" });
        let style = MonoTextStyle::new(&FONT_6X10, MUTED);
        let x = W as i32 - MARGIN - 14 - 6 * label.len() as i32;
        Text::with_baseline(&label, Point::new(x, (BAR_H - 10) / 2), style, Baseline::Top).draw(f).ok();
    }

    if s.full {
        let size = 4 * PET / 2;
        let top = BAR_H + (H as i32 - BAR_H - size) / 2;
        crate::pet::draw(f, s.pet, ms, Point::new((W as i32 - size) / 2, top), 4);
        return 1;
    }
    // The pet, where it always is, and the column beside it.
    let top = BAR_H + 4;
    crate::pet::draw(f, s.pet, ms, Point::new(MARGIN, top), 2);
    let col_x = MARGIN + PET + 6;
    let col_w = W as i32 - MARGIN - col_x;
    let small = MonoTextStyle::new(&FONT_6X10, MUTED);
    if let Some(level) = s.level {
        let y = top + PET / 2 - 4;
        f.fill_solid(&Rectangle::new(Point::new(col_x, y), Size::new(col_w as u32, 8)), BUTTON).ok();
        f.fill_solid(&Rectangle::new(Point::new(col_x, y), Size::new((col_w as f32 * level) as u32, 8)), ACCENT).ok();
    } else {
        let (text, style) = if s.options.is_empty() {
            (&s.you, small)
        } else {
            (&s.reply, MonoTextStyle::new(&FONT_6X10, REPLY))
        };
        let lines = wrap(&clean(text), col_w as usize / 6);
        let lines = &lines[..lines.len().min((PET / SMALL) as usize)];
        let mut y = top + (PET - SMALL * lines.len() as i32).max(0) / 2;
        for line in lines {
            Text::with_baseline(line, Point::new(col_x, y), style, Baseline::Top).draw(f).ok();
            y += SMALL;
        }
    }
    let y = top + PET + 6;

    // Answer buttons, pinned to the bottom: the question is already beside the pet.
    if !s.options.is_empty() {
        let label = MonoTextStyle::new(&FONT_7X13_BOLD, TITLE);
        for (i, option) in s.options.iter().take(MAX_OPTIONS).enumerate() {
            let top = options_top(s.options.len()) + i as i32 * (OPTION_ROW + OPTION_GAP);
            RoundedRectangle::with_equal_corners(
                Rectangle::new(Point::new(MARGIN, top), Size::new(W as u32 - 2 * MARGIN as u32, OPTION_ROW as u32)),
                Size::new(6, 6),
            )
            .into_styled(PrimitiveStyle::with_fill(BUTTON))
            .draw(f)
            .ok();
            let text: String = clean(option).trim().chars().take((W - 2 * MARGIN as usize - 20) / 7).collect();
            Text::with_baseline(&text, Point::new(MARGIN + 10, top + (OPTION_ROW - 13) / 2), label, Baseline::Top).draw(f).ok();
        }
        return 1;
    }

    // The reply, paged.
    let bottom = H as i32 - MARGIN;
    let max_lines = ((bottom - y) / LINE).max(1) as usize;
    // When the reply pages, a column is kept for the chevrons and a line for the page mark.
    let full = wrap(&clean(&s.reply), COLS);
    let paged = full.len() > max_lines;
    let reply = if paged { wrap(&clean(&s.reply), COLS - 1) } else { full };
    let room = if paged { max_lines.saturating_sub(1).max(1) } else { max_lines };
    let pages = reply.len().div_ceil(room).max(1);
    let start = (s.page % pages) * room;
    let shown = &reply[start..reply.len().min(start + room)];
    let style = MonoTextStyle::new(&FONT_8X13, REPLY);
    let mut ty = y;
    for line in shown {
        Text::with_baseline(line, Point::new(MARGIN, ty), style, Baseline::Top).draw(f).ok();
        ty += LINE;
    }
    if pages > 1 || s.zones {
        // Chevrons at the right edge: up at the top of the text, down at the bottom.
        let x = W as i32 - MARGIN - 7;
        let chevron = PrimitiveStyle::with_fill(MUTED);
        Triangle::new(Point::new(x, y + 6), Point::new(x + 7, y + 6), Point::new(x + 3, y + 1)).into_styled(chevron).draw(f).ok();
        Triangle::new(Point::new(x, bottom - 7), Point::new(x + 7, bottom - 7), Point::new(x + 3, bottom - 2)).into_styled(chevron).draw(f).ok();
    }
    if pages > 1 {
        let label = format!("{}/{}", s.page % pages + 1, pages);
        let x = W as i32 - MARGIN - CHEVRON - 6 * label.len() as i32;
        Text::with_baseline(&label, Point::new(x, bottom - 10), MonoTextStyle::new(&FONT_6X10, ACCENT), Baseline::Top).draw(f).ok();
    }
    pages
}

/// Strips the agent's markdown and whatever the font (ISO-8859-1) lacks: emojis,
/// dashes, curly quotes. Without this, stray `**` and `?` show up.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let line = line.trim_start_matches('#').trim_start();
        let line = match line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            Some(rest) => format!("· {rest}"),
            None => line.to_string(),
        };
        for c in line.replace("**", "").replace("__", "").replace('`', "").chars() {
            match c {
                '—' | '–' => out.push('-'),
                '“' | '”' => out.push('"'),
                '‘' | '’' => out.push('\''),
                '…' => out.push_str("..."),
                c if (c as u32) < 0x100 => out.push(c),
                _ => {}
            }
        }
        out.push('\n');
    }
    out
}

/// Word-wraps to `cols` characters per line. Keeps the `\n`s.
// ponytail: no scrolling. Whatever doesn't fit gets cut; add touch scrolling if it gets annoying.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let mut word = word;
            while word.chars().count() > cols {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                let cut = word.char_indices().nth(cols).map_or(word.len(), |(i, _)| i);
                lines.push(word[..cut].to_string());
                word = &word[cut..];
            }
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > cols {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    #[test]
    fn option_hit_test_skips_gaps() {
        // Two buttons: 232 - 2 * 28 + 4 = 180..204 and 208..232.
        assert_eq!(super::option_at(179, 2), None);
        assert_eq!(super::option_at(180, 2), Some(0));
        assert_eq!(super::option_at(205, 2), None);
        assert_eq!(super::option_at(231, 2), Some(1));
        assert_eq!(super::option_at(200, 0), None);
    }

    #[test]
    fn clean_strips_markdown_and_unsupported_chars() {
        assert_eq!(
            super::clean("Café! 👋\n- **NEAR_API_KEY** — “x”…"),
            "Café! \n· NEAR_API_KEY - \"x\"...\n"
        );
    }
}

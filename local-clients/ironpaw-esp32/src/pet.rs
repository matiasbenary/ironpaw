//! The pet: the 3D dog's sprite frames from `assets/pet` (made by `m3.py`).

use crate::pet_data::{self as data, Frame};
use embedded_graphics::{pixelcolor::Rgb565, prelude::*, primitives::Rectangle};

static SHEET: &[u8] = include_bytes!("../assets/pet/sheet.idx");

#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub enum Mood {
    #[default]
    Idle,
    Happy,
    Eating,
    Sleep,
    Sad,
    Listening,
    Thinking,
    Talking,
}

impl Mood {
    fn frames(self) -> &'static [Frame] {
        match self {
            Mood::Idle => data::IDLE,
            Mood::Happy => data::HAPPY,
            Mood::Eating => data::EATING,
            Mood::Sleep => data::SLEEP,
            Mood::Sad => data::SAD,
            Mood::Listening => data::LISTENING,
            Mood::Thinking => data::THINKING,
            Mood::Talking => data::TALKING,
        }
    }
}

/// Which frame shows at `ms`: redraw when it changes.
pub fn pose(mood: Mood, ms: u32) -> usize {
    let frames = mood.frames();
    let total: u32 = frames.iter().map(|f| f.ms as u32).sum();
    let mut t = ms % total;
    frames.iter().position(|f| t.checked_sub(f.ms as u32).map(|rest| t = rest).is_none()).unwrap_or(0)
}

fn color(i: u8) -> Rgb565 {
    let (r, g, b) = data::PALETTE[i as usize];
    Rgb565::new(r >> 3, g >> 2, b >> 3)
}

/// Draws the pet's 40x40 cell scaled by `scale` with its top-left at `at`.
pub fn draw<D: DrawTarget<Color = Rgb565>>(d: &mut D, mood: Mood, ms: u32, at: Point, scale: u32) {
    let f = &mood.frames()[pose(mood, ms)];
    let cell = data::CELL as usize;
    for y in 0..cell {
        let row = (f.y as usize + y) * data::SHEET_W as usize + f.x as usize;
        for (x, &c) in SHEET[row..row + cell].iter().enumerate() {
            if c != 0 {
                let r = Rectangle::new(at + Point::new(x as i32, y as i32) * scale as i32, Size::new(scale, scale));
                d.fill_solid(&r, color(c)).ok();
            }
        }
    }
}

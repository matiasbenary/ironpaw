//! Ironpaw voice client for the Waveshare ESP32-S3-Touch-LCD-1.54. No menu: everything is voice.
//! Hold +/KEY and talk. The board gets the transcript and decides: "Note: ..." saves a note,
//! "My notes" lists them, "louder"/"quieter" set the volume, anything else goes to the agent,
//! whose reply is shown and spoken.
//! Reminders ("events") come from Ironpaw's `events.json` (the agent edits it) and ring on the
//! board itself (a synthesized bark); timers are local. The SD card keeps offline copies of
//! the notes and events and the error log.
//! BOOT toggles chat and notes, or undoes a note just saved. Taps: top third goes back a page
//! (or note), the middle reads it aloud, the bottom third goes forward. When the agent asks
//! a question with options, they show as buttons to tap.
//! Pins from the official Waveshare BSP.

mod app;
mod audio;
mod clock;
mod lcd;
mod net;
mod nvs;
mod pet;
mod pet_data;
mod proto;
mod sd;
mod ui;
mod wav;

use anyhow::Result;
use esp_idf_svc::hal::{
    adc::{
        attenuation::DB_11,
        oneshot::{
            config::{AdcChannelConfig, Calibration},
            AdcChannelDriver, AdcDriver,
        },
        ADC1,
    },
    delay::FreeRtos,
    gpio::{AnyIOPin, Gpio0, Gpio1, Gpio2, Gpio4, Gpio5, Gpio7, Input, Output, OutputPin, PinDriver, Pull},
    i2c::{I2cConfig, I2cDriver},
    i2s::{
        config::{DataBitWidth, StdConfig},
        I2sDriver,
    },
    ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver},
    prelude::*,
    spi::{config::Config as SpiConfig, Dma, SpiDeviceDriver, SpiDriver, SpiDriverConfig},
    task::watchdog::{TWDTConfig, TWDTDriver},
};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use std::{sync::mpsc, time::{Duration, Instant}};

const CST816: u8 = 0x15;
const LCD_MHZ: u32 = 80;
/// While speaking, long replies flip pages on their own every this often.
const PAGE_TIME: Duration = Duration::from_secs(6);
/// The longest blocking call (a 60 s HTTP timeout) must fit; anything longer is a hang.
const WATCHDOG: Duration = Duration::from_secs(90);
// The documented divider of this Waveshare family: 200K/100K, so VBAT is the pin's mV times 3.
const BATTERY_DIVIDER: u32 = 3;

type Battery = AdcChannelDriver<'static, Gpio1, AdcDriver<'static, ADC1>>;

/// Everything with a pin.
struct Board {
    i2s: audio::I2s,
    chunk: audio::Chunk,
    pa: PinDriver<'static, Gpio7, Output>,
    key: PinDriver<'static, Gpio4, Input>,
    boot: PinDriver<'static, Gpio0, Input>,
    pwr: PinDriver<'static, Gpio5, Input>,
    /// High keeps the battery path on; low powers the board off (on battery; USB keeps it up).
    latch: PinDriver<'static, Gpio2, Output>,
    i2c: I2cDriver<'static>,
    backlight: LedcDriver<'static>,
    lcd: lcd::Lcd,
    fb: ui::Frame,
    battery: Battery,
    /// Set once WiFi is up, after the settings are read.
    wifi: Option<BlockingWifi<EspWifi<'static>>>,
}

fn uptime_ms() -> u32 {
    (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000) as u32
}

/// Feeds the task watchdog from deep inside long calls (playback, downloads).
fn feed() {
    unsafe { esp_idf_svc::sys::esp_task_wdt_reset() };
}

fn draw(lcd: &mut lcd::Lcd, fb: &mut ui::Frame, screen: &ui::Screen) -> usize {
    let pages = ui::draw(fb, screen, uptime_ms());
    if let Err(e) = lcd.frame(&fb.0) {
        log::warn!("lcd: {e}");
    }
    pages
}

impl Board {
    fn show(&mut self, screen: &ui::Screen) -> usize {
        draw(&mut self.lcd, &mut self.fb, screen)
    }

    fn backlight(&mut self, percent: u32) {
        let duty = self.backlight.get_max_duty() * percent / 100;
        if let Err(e) = self.backlight.set_duty(duty) {
            log::warn!("backlight: {e}");
        }
    }

    /// A short tone, so you know when the mic opens and closes without looking.
    fn beep(&mut self, hz: u32) {
        let _ = self.pa.set_high();
        if let Err(e) = audio::beep(&mut self.i2s, &mut self.chunk, hz, 60) {
            log::warn!("beep: {e}");
        }
        let _ = audio::flush(&mut self.i2s, &mut self.chunk);
        let _ = self.pa.set_low();
    }

    /// Reads `text` aloud. Pressing + cuts it (and records on the next loop).
    fn say(&mut self, text: &str) -> Result<()> {
        let key = &self.key;
        say(&mut self.i2s, &mut self.chunk, &mut self.pa, || key.is_low(), text)
    }

    /// Like `say`, but flips `screen`'s pages as it goes if the text spans several.
    fn say_paged(&mut self, screen: &mut ui::Screen, pages: usize, text: &str) -> Result<()> {
        let Self { i2s, chunk, pa, key, lcd, fb, .. } = self;
        let mut flipped = Instant::now();
        let stop = || {
            if pages > 1 && flipped.elapsed() >= PAGE_TIME {
                screen.page = (screen.page + 1) % pages;
                draw(lcd, fb, screen);
                flipped = Instant::now();
            }
            key.is_low()
        };
        say(i2s, chunk, pa, stop, text)
    }

    /// Records while + is held, up to `MAX_SECS`, showing the level. Returns whether it
    /// was released (as opposed to hitting the cap).
    fn record(&mut self, screen: &mut ui::Screen, pcm: &mut Vec<u8>) -> bool {
        // Drop whatever piled up in the DMA while nobody was reading.
        for _ in 0..4 {
            let _ = self.i2s.read(&mut self.chunk, 100);
        }
        pcm.clear();
        let mut reads = 0;
        while pcm.len() < app::MAX_BYTES {
            let level = match audio::record_chunk(&mut self.i2s, &mut self.chunk, pcm) {
                Ok(level) => level,
                Err(e) => {
                    log::warn!("mic: {e}");
                    return true;
                }
            };
            reads += 1;
            // Button and screen every ~100 ms; the rest of the time it only records.
            if reads % 6 == 0 {
                if self.key.is_high() {
                    screen.level = None;
                    return true;
                }
                screen.level = Some(level);
                self.show(screen);
            }
        }
        screen.level = None;
        false
    }

    /// CST816: register 0x02 holds the finger count, 0x03..0x06 the 12-bit X and Y.
    fn touch(&mut self) -> Option<(i32, i32)> {
        let mut b = [0u8; 5];
        self.i2c.write_read(CST816, &[0x02], &mut b, 10).ok()?;
        (b[0] & 0x0F > 0).then(|| {
            let x = ((b[1] as i32 & 0x0F) << 8) | b[2] as i32;
            let y = ((b[3] as i32 & 0x0F) << 8) | b[4] as i32;
            (x, y)
        })
    }

    /// The battery's charge and whether the board is on USB power.
    fn power(&mut self) -> app::Power {
        let usb = unsafe { esp_idf_svc::sys::usb_serial_jtag_is_connected() };
        match self.battery.read() {
            Ok(mv) => {
                let vbat = mv as u32 * BATTERY_DIVIDER;
                // ponytail: plugged = a USB host is enumerating (a PC) or the battery net sits at
                // charger level. A dumb charger on a full battery (4.2 V) may read as unplugged;
                // a charge-status pin would settle it, if the schematic has one.
                let plugged = usb || vbat >= 4300;
                log::info!("battery: {mv} mV at the pin, {vbat} mV, plugged: {plugged}");
                app::Power { percent: Some(battery_percent(vbat)), plugged }
            }
            Err(e) => {
                log::warn!("battery: {e}");
                app::Power { percent: None, plugged: usb }
            }
        }
    }

    fn key_down(&self) -> bool {
        self.key.is_low()
    }

    fn boot_down(&self) -> bool {
        self.boot.is_low()
    }

    fn pwr_down(&self) -> bool {
        self.pwr.is_low()
    }

    fn power_off(&mut self) -> Result<()> {
        Ok(self.latch.set_low()?)
    }

    /// A dog barking, for reminders without a sound of their own.
    fn bark(&mut self, barks: u32) {
        let _ = self.pa.set_high();
        if let Err(e) = audio::bark(&mut self.i2s, &mut self.chunk, barks) {
            log::warn!("bark: {e}");
        }
        let _ = audio::flush(&mut self.i2s, &mut self.chunk);
        let _ = self.pa.set_low();
    }

    /// Feeds the task watchdog from deep inside long calls.
    fn feed(&mut self) {
        feed();
    }

    /// Whether WiFi is connected.
    fn link_up(&mut self) -> bool {
        self.wifi.as_ref().map_or(true, |wifi| wifi.is_connected().unwrap_or(false))
    }

    fn reconnect(&mut self) -> Result<()> {
        match self.wifi.as_mut() {
            Some(wifi) => net::reconnect(wifi),
            None => Ok(()),
        }
    }
}

/// Li-ion, roughly linear between empty (3.3 V) and full (4.2 V).
fn battery_percent(mv: u32) -> u8 {
    (mv.clamp(3300, 4200) - 3300).div_ceil(9) as u8
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let p = Peripherals::take()?;

    // Battery latch: high keeps the board powered without USB
    let mut latch = PinDriver::output(p.pins.gpio2)?;
    latch.set_high()?;

    // A hung HTTP call or codec resets the board instead of freezing it.
    let mut twdt = TWDTDriver::new(
        p.twdt,
        &TWDTConfig { duration: WATCHDOG, panic_on_trigger: true, ..Default::default() },
    )?;
    let _watchdog = twdt.watch_current_task()?;

    // --- LCD, with a PWM backlight so it can dim when idle ---
    let timer = LedcTimerDriver::new(p.ledc.timer0, &TimerConfig::default().frequency(5.kHz().into()))?;
    let backlight = LedcDriver::new(p.ledc.channel0, timer, p.pins.gpio46)?;
    let spi = SpiDriver::new(
        p.spi2,
        p.pins.gpio38,    // SCLK
        p.pins.gpio39,    // MOSI
        None::<AnyIOPin>, // no MISO
        &SpiDriverConfig::new().dma(Dma::Auto(ui::W * ui::H * 2 / 4)),
    )?;
    let dev = SpiDeviceDriver::new(spi, Some(p.pins.gpio21), &SpiConfig::new().baudrate(LCD_MHZ.MHz().into()))?;
    let dc = PinDriver::output(p.pins.gpio45.downgrade_output())?;
    let rst = PinDriver::output(p.pins.gpio40.downgrade_output())?;
    let lcd = lcd::Lcd::new(dev, dc, rst)?;

    // --- Buttons: +/KEY is hold to talk, BOOT toggles views. Both pull to ground. ---
    let mut key = PinDriver::input(p.pins.gpio4)?;
    key.set_pull(Pull::Up)?;
    let mut boot = PinDriver::input(p.pins.gpio0)?;
    boot.set_pull(Pull::Up)?;
    // PWR is part of the power circuit and also reads low here while pressed.
    let mut pwr = PinDriver::input(p.pins.gpio5)?;
    pwr.set_pull(Pull::Up)?;

    // --- I2C: touch and codecs ---
    let mut i2c = I2cDriver::new(p.i2c0, p.pins.gpio42, p.pins.gpio41, &I2cConfig::new().baudrate(400.kHz().into()))?;
    let mut tp_rst = PinDriver::output(p.pins.gpio47)?;
    tp_rst.set_low()?;
    FreeRtos::delay_ms(10);
    tp_rst.set_high()?;
    FreeRtos::delay_ms(50);
    // No auto-sleep: asleep, the CST816 doesn't answer over I2C
    if let Err(e) = i2c.write(CST816, &[0xFE, 0x01], esp_idf_svc::hal::delay::BLOCK) {
        log::warn!("touch: {e}");
    }

    // --- Audio: no mic means no voice client, so here we do bail out ---
    let mut i2s = I2sDriver::new_std_bidir(
        p.i2s0,
        &StdConfig::philips(audio::RATE, DataBitWidth::Bits16),
        p.pins.gpio9,       // BCLK
        p.pins.gpio11,      // DIN  <- ES7210
        p.pins.gpio12,      // DOUT -> ES8311
        Some(p.pins.gpio8), // MCLK
        p.pins.gpio10,      // WS
    )?;
    // The codecs need MCLK running before they can be configured
    i2s.tx_enable()?;
    i2s.rx_enable()?;
    audio::init_es8311(&mut i2c)?;
    audio::init_es7210(&mut i2c)?;
    // Amplifier off except while speaking: no background hiss or beeps.
    let mut pa = PinDriver::output(p.pins.gpio7)?;
    pa.set_low()?;

    // --- TF card: optional. Without one, the offline copies and the error log are off. ---
    match sd::mount(p.sdmmc1, p.pins.gpio15, p.pins.gpio16, p.pins.gpio17, p.pins.gpio18, p.pins.gpio13, p.pins.gpio14) {
        Ok(()) => log::info!("sd: mounted at {}", sd::ROOT),
        Err(e) => log::warn!("sd: no card: {e}"),
    }

    let battery = AdcChannelDriver::new(
        AdcDriver::new(p.adc1)?,
        p.pins.gpio1,
        &AdcChannelConfig { attenuation: DB_11, calibration: Calibration::Curve, ..Default::default() },
    )?;

    let mut board = Board {
        i2s,
        chunk: [0; 1024],
        pa,
        key,
        boot,
        pwr,
        latch,
        i2c,
        backlight,
        lcd,
        fb: ui::Frame::new(),
        battery,
        wifi: None,
    };
    board.backlight(app::BRIGHTNESS);
    let mut app = app::App::new();
    app.show(&mut board);

    // --- Settings and network ---
    let partition = EspDefaultNvsPartition::take()?;
    if let Err(e) = net::init(partition.clone()) {
        app.blocked(&mut board, "Not set up", format!("{e}\n\nSee provision.sh in the firmware's README."));
        return Err(e);
    }
    app.restore();
    board.wifi = Some(match net::connect_wifi(p.modem, partition) {
        Ok(wifi) => wifi,
        Err(e) => {
            app.blocked(&mut board, "No WiFi", format!("Check the WiFi name and password the board was set up with.\n\n{e}"));
            return Err(e);
        }
    });
    let (tx, rx) = mpsc::channel();
    net::spawn_events(tx)?;
    app.connecting();
    app::run(&mut app, &mut board, rx)
}

/// Reads the text aloud with Piper from Ironpaw; if that fails the reply stays on screen only.
/// `stop` is polled as it plays (the button, to cut it short).
fn say(
    i2s: &mut audio::I2s,
    chunk: &mut audio::Chunk,
    pa: &mut PinDriver<'_, Gpio7, Output>,
    mut stop: impl FnMut() -> bool,
    text: &str,
) -> Result<()> {
    pa.set_high()?;
    let mut blocks = 0;
    let piper = net::speech(&ui::clean(text), |pcm| {
        audio::play_mono(i2s, pcm, chunk)?;
        blocks += 1;
        if blocks % 16 != 0 {
            return Ok(true);
        }
        feed();
        Ok(!stop())
    });
    if let Err(e) = piper {
        log::warn!("piper: {e}; reply shown on screen only");
    }
    if let Err(e) = audio::flush(i2s, chunk) {
        log::warn!("i2s: {e}");
    }
    pa.set_low()?;
    Ok(())
}

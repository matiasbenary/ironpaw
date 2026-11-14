//! Speaker (ES8311) and microphones (ES7210) on a single bidirectional I2S.
//! Codec drivers copied from the pokesp firmware; recording and WAV building added here.
//! Register sequences ported from esp_codec_dev (esp-adf), slave mode,
//! MCLK = 256 × 16 kHz. The ES7210 runs plain I2S with MIC1+MIC2 (no TDM) to
//! share the standard channel with the ES8311.

use anyhow::{anyhow, Result};
use esp_idf_svc::hal::{delay::BLOCK, i2c::I2cDriver, i2s::I2sBiDir, i2s::I2sDriver};

pub use crate::wav::{trim_silence, wav_header, RATE};
/// DAC volume: 0xBF = 0 dB, each step is 0.5 dB.
const DAC_VOLUME: u8 = 0xBF;

const ES8311: u8 = 0x18;
const ES7210: u8 = 0x40;
// Mic PGA gain: 0x0A = 30 dB (same as the Waveshare BSP)
const MIC_GAIN: u8 = 0x0A;

pub type I2s = I2sDriver<'static, I2sBiDir>;
/// 16 ms of 16-bit stereo audio.
pub type Chunk = [u8; 1024];

struct Reg<'a, 'd> {
    i2c: &'a mut I2cDriver<'d>,
    addr: u8,
}

impl Reg<'_, '_> {
    fn w(&mut self, reg: u8, val: u8) -> Result<()> {
        self.i2c
            .write(self.addr, &[reg, val], BLOCK)
            .map_err(|e| anyhow!("i2c 0x{:02x} w 0x{reg:02x}: {e}", self.addr))
    }
    fn r(&mut self, reg: u8) -> Result<u8> {
        let mut b = [0u8];
        self.i2c
            .write_read(self.addr, &[reg], &mut b, BLOCK)
            .map_err(|e| anyhow!("i2c 0x{:02x} r 0x{reg:02x}: {e}", self.addr))?;
        Ok(b[0])
    }
    /// Read-modify-write: sets `val` in the bits of `mask`.
    fn upd(&mut self, reg: u8, mask: u8, val: u8) -> Result<()> {
        let v = self.r(reg)?;
        self.w(reg, (v & !mask) | (val & mask))
    }
}

/// es8311_open + set_fs(16 bit, I2S, 16 kHz) + enable in DAC mode.
pub fn init_es8311(i2c: &mut I2cDriver) -> Result<()> {
    let mut c = Reg { i2c, addr: ES8311 };
    // open
    if c.r(0x0D)? != 0xFA {
        c.w(0x0D, 0xFA)?;
    }
    c.w(0x44, 0x08)?;
    c.w(0x44, 0x08)?; // the first write sometimes fails, Espressif repeats it
    for (r, v) in [
        (0x01, 0x30), (0x02, 0x00), (0x03, 0x10), (0x16, 0x24), (0x04, 0x10),
        (0x05, 0x00), (0x0B, 0x00), (0x0C, 0x00), (0x10, 0x1F), (0x11, 0x7F),
        (0x00, 0x80),
    ] {
        c.w(r, v)?;
    }
    c.upd(0x00, 0x40, 0x00)?; // slave
    c.w(0x01, 0x3F)?; // MCLK from the pin, not inverted
    c.upd(0x06, 0x20, 0x00)?; // SCLK not inverted
    c.w(0x13, 0x10)?;
    c.w(0x1B, 0x0A)?;
    c.w(0x1C, 0x6A)?;
    c.w(0x44, 0x58)?;
    // set_fs: 16 bits, plain I2S format
    for r in [0x09, 0x0A] {
        let v = c.r(r)?;
        c.w(r, (v | 0x0C) & 0xFC)?;
    }
    // Coefficients for MCLK 4.096 MHz / 16 kHz: pre_div 1, mult 1, adc/dac div 1,
    // lrck 0x00ff, bclk_div 4, adc_osr 0x10, dac_osr 0x20
    c.upd(0x02, 0xF8, 0x00)?;
    c.w(0x05, 0x00)?;
    c.upd(0x03, 0x7F, 0x10)?;
    c.upd(0x04, 0x7F, 0x20)?;
    c.upd(0x07, 0x3F, 0x00)?;
    c.w(0x08, 0xFF)?;
    c.upd(0x06, 0x1F, 4 - 1)?;
    // enable (es8311_start in DAC mode)
    c.w(0x00, 0x80)?;
    c.w(0x01, 0x3F)?;
    c.upd(0x09, 0x40, 0x00)?;
    c.upd(0x0A, 0x40, 0x00)?;
    for (r, v) in [(0x17, 0xBF), (0x0E, 0x02), (0x12, 0x00), (0x14, 0x1A)] {
        c.w(r, v)?;
    }
    c.upd(0x14, 0x40, 0x00)?; // no digital mic
    for (r, v) in [(0x0D, 0x01), (0x15, 0x40), (0x37, 0x08), (0x45, 0x00)] {
        c.w(r, v)?;
    }
    c.upd(0x31, 0x60, 0x00)?; // unmute
    c.w(0x32, DAC_VOLUME)?;
    Ok(())
}

/// es7210_open + set_fs + enable, slave, MIC1+MIC2 on plain 16-bit I2S.
pub fn init_es7210(i2c: &mut I2cDriver) -> Result<()> {
    let mut c = Reg { i2c, addr: ES7210 };
    for (r, v) in [
        (0x00, 0xFF), (0x00, 0x41), (0x01, 0x3F), (0x09, 0x30), (0x0A, 0x30),
        (0x23, 0x2A), (0x22, 0x0A), (0x20, 0x0A), (0x21, 0x2A),
    ] {
        c.w(r, v)?;
    }
    c.upd(0x08, 0x01, 0x00)?; // slave
    for (r, v) in [(0x40, 0x43), (0x41, 0x70), (0x42, 0x70), (0x07, 0x20), (0x02, 0xC1)] {
        c.w(r, v)?;
    }
    mic_select(&mut c)?;
    let off_reg = c.r(0x01)?;
    // set_fs: 16 bits, plain I2S
    c.upd(0x11, 0xE0, 0x60)?;
    c.upd(0x11, 0x03, 0x00)?;
    // enable (es7210_start)
    c.w(0x01, off_reg)?;
    c.w(0x06, 0x00)?;
    c.w(0x40, 0x43)?;
    for r in 0x47..=0x4A {
        c.w(r, 0x08)?;
    }
    mic_select(&mut c)?;
    c.w(0x40, 0x43)?;
    c.w(0x00, 0x71)?;
    c.w(0x00, 0x41)?;
    // unmute
    c.upd(0x14, 0x03, 0x00)?;
    c.upd(0x15, 0x03, 0x00)?;
    Ok(())
}

fn mic_select(c: &mut Reg) -> Result<()> {
    for r in 0x43..=0x46 {
        c.upd(r, 0x10, 0x00)?;
    }
    c.w(0x4B, 0xFF)?;
    c.w(0x4C, 0xFF)?;
    c.upd(0x01, 0x0B, 0x00)?;
    c.w(0x4B, 0x00)?;
    for r in [0x43, 0x44] {
        c.upd(r, 0x10, 0x10)?;
        c.upd(r, 0x0F, MIC_GAIN)?;
    }
    c.w(0x12, 0x00) // no TDM
}

/// Reads 16 ms from the mic and appends the left channel (MIC1) to `pcm` as mono s16le PCM.
/// Returns the block's 0..1 level, for the VU meter.
pub fn record_chunk(i2s: &mut I2s, buf: &mut Chunk, pcm: &mut Vec<u8>) -> Result<f32> {
    let n = i2s.read(buf, 100)?;
    for frame in buf[..n].chunks_exact(4) {
        pcm.extend_from_slice(&frame[..2]);
    }
    Ok(level(&buf[..n]))
}

/// Left-channel RMS on a log scale: -60 dBFS → 0, 0 dBFS → 1.
fn level(pcm: &[u8]) -> f32 {
    let (mut sum, mut n) = (0f32, 0f32);
    for frame in pcm.chunks_exact(4) {
        let s = i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32768.0;
        sum += s * s;
        n += 1.0;
    }
    if n == 0.0 {
        return 0.0;
    }
    let db = 20.0 * (sum / n).sqrt().max(1e-6).log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Sends mono s16le PCM at `RATE` to the speaker (the I2S is stereo: same on both sides).
pub fn play_mono(i2s: &mut I2s, mono: &[u8], buf: &mut Chunk) -> Result<()> {
    let mut used = 0;
    for s in mono.chunks_exact(2) {
        buf[used..used + 4].copy_from_slice(&[s[0], s[1], s[0], s[1]]);
        used += 4;
        if used == buf.len() {
            i2s.write_all(buf, BLOCK)?;
            used = 0;
        }
    }
    i2s.write_all(&buf[..used], BLOCK)?;
    Ok(())
}

/// A sine at `hz` for `ms` milliseconds, at a quarter of full scale.
pub fn beep(i2s: &mut I2s, buf: &mut Chunk, hz: u32, ms: u32) -> Result<()> {
    let samples = RATE * ms / 1000;
    let mut used = 0;
    for i in 0..samples {
        let t = i as f32 / RATE as f32;
        // Short fade in and out so it doesn't click.
        let edge = (i.min(samples - i) as f32 / 80.0).min(1.0);
        let s = ((t * hz as f32 * core::f32::consts::TAU).sin() * 8192.0 * edge) as i16;
        let b = s.to_le_bytes();
        buf[used..used + 4].copy_from_slice(&[b[0], b[1], b[0], b[1]]);
        used += 4;
        if used == buf.len() {
            i2s.write_all(buf, BLOCK)?;
            used = 0;
        }
    }
    i2s.write_all(&buf[..used], BLOCK)?;
    Ok(())
}

/// A dog barking: `barks` short "woof"s with a falling pitch, a formant around 900 Hz and a
/// breathy edge, in pairs. The reminder sound.
// ponytail: a formula, not a recording. Embed a real bark with include_bytes! if this one doesn't convince.
pub fn bark(i2s: &mut I2s, buf: &mut Chunk, barks: u32) -> Result<()> {
    const WOOF_MS: u32 = 160;
    let mut phase = 0f32;
    let mut noise = 0x1234_5678u32;
    let mut used = 0;
    for n in 0..barks {
        // Pair rhythm: short gap inside a pair, long one between pairs.
        let gap_ms = if n % 2 == 1 { 420 } else { 140 };
        for i in 0..RATE * (WOOF_MS + gap_ms) / 1000 {
            let t = i as f32 / RATE as f32;
            let woof = t < WOOF_MS as f32 / 1000.0;
            let s = if woof {
                let f = 190.0 + 430.0 * (-t * 14.0).exp();
                phase = (phase + f * core::f32::consts::TAU / RATE as f32) % core::f32::consts::TAU;
                let mut v = 0f32;
                for k in 1..=6 {
                    let harmonic = k as f32 * f;
                    let formant = 1.0 / (1.0 + ((harmonic - 900.0) / 350.0).powi(2));
                    v += (phase * k as f32).sin() * formant / k as f32;
                }
                noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let breath = (noise >> 16) as f32 / 32768.0 - 1.0;
                let env = (1.0 - (-t * 400.0).exp()) * (-t * 11.0).exp();
                ((v * 2.2 + breath * 0.25) * env * 14000.0).clamp(-32000.0, 32000.0) as i16
            } else {
                0
            };
            let b = s.to_le_bytes();
            buf[used..used + 4].copy_from_slice(&[b[0], b[1], b[0], b[1]]);
            used += 4;
            if used == buf.len() {
                i2s.write_all(buf, BLOCK)?;
                used = 0;
            }
        }
    }
    i2s.write_all(&buf[..used], BLOCK)?;
    Ok(())
}

/// Silence to drain the DMA: otherwise it repeats the last block when the amp turns off.
pub fn flush(i2s: &mut I2s, buf: &mut Chunk) -> Result<()> {
    buf.fill(0);
    for _ in 0..4 {
        i2s.write_all(buf, BLOCK)?;
    }
    Ok(())
}


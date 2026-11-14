//! Hand-rolled ST7789 240x240: minimal init and the whole frame as raw bytes over SPI+DMA.
//! No mipidsi: it converted pixel by pixel and that ate half of every frame.

use anyhow::Result;
use esp_idf_svc::hal::{
    delay::FreeRtos,
    gpio::{AnyOutputPin, Output, PinDriver},
    spi::{SpiDeviceDriver, SpiDriver},
};

type Pin = PinDriver<'static, AnyOutputPin, Output>;

pub struct Lcd {
    spi: SpiDeviceDriver<'static, SpiDriver<'static>>,
    dc: Pin,
    _rst: Pin, // if the pin is dropped it floats and can reset the panel
}

impl Lcd {
    pub fn new(spi: SpiDeviceDriver<'static, SpiDriver<'static>>, dc: Pin, mut rst: Pin) -> Result<Self> {
        rst.set_low()?;
        FreeRtos::delay_ms(10);
        rst.set_high()?;
        FreeRtos::delay_ms(120);
        let mut lcd = Self { spi, dc, _rst: rst };
        lcd.cmd(0x11, &[])?; // exit sleep
        FreeRtos::delay_ms(120);
        lcd.cmd(0x3A, &[0x55])?; // RGB565
        lcd.cmd(0x36, &[0x00])?; // normal orientation, RGB order
        lcd.cmd(0x21, &[])?; // inverted colors: this panel needs them
        lcd.cmd(0x2A, &[0, 0, 0, 239])?; // window: columns 0..239
        lcd.cmd(0x2B, &[0, 0, 0, 239])?; // rows 0..239
        lcd.cmd(0x29, &[])?; // display on
        Ok(lcd)
    }

    fn cmd(&mut self, c: u8, args: &[u8]) -> Result<()> {
        self.dc.set_low()?;
        self.spi.write(&[c])?;
        if !args.is_empty() {
            self.dc.set_high()?;
            self.spi.write(args)?;
        }
        Ok(())
    }

    /// Sends a whole frame (W×H×2 bytes, big-endian RGB565) to the full window.
    pub fn frame(&mut self, px: &[u8]) -> Result<()> {
        self.cmd(0x2C, &[])?; // RAMWR: starts at the beginning of the window
        self.dc.set_high()?;
        self.spi.write(px)?;
        Ok(())
    }
}

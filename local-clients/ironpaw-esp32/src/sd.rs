//! The TF card, mounted at `/sdcard` so `std::fs` works on it. It holds offline copies of the
//! notes and events (`notes.json`, `events.json`) and the error log (`ironpaw.log`). Without a
//! card everything here fails quietly and the board works as before.
//! Pins from Waveshare's `07_sd_card_test` for this board: CLK 16, CMD 15, D0 17, D1 18, D2 13, D3 14.

use anyhow::{anyhow, Result};
use esp_idf_svc::fs::fatfs::Fatfs;
use esp_idf_svc::hal::{
    gpio::{AnyIOPin, Gpio13, Gpio14, Gpio15, Gpio16, Gpio17, Gpio18},
    sd::{
        mmc::{SdMmcHostDriver, SDMMC1},
        SdCardConfiguration, SdCardDriver,
    },
};
use std::sync::atomic::{AtomicBool, Ordering};

pub const ROOT: &str = "/sdcard";
pub const LOG: &str = "/sdcard/ironpaw.log";
static MOUNTED: AtomicBool = AtomicBool::new(false);

/// Mounts the card (4-bit SDMMC, FAT). Call once; the filesystem lives as long as the board.
pub fn mount(
    slot: SDMMC1,
    cmd: Gpio15,
    clk: Gpio16,
    d0: Gpio17,
    d1: Gpio18,
    d2: Gpio13,
    d3: Gpio14,
) -> Result<()> {
    let host = SdMmcHostDriver::new_4bits(
        slot,
        cmd,
        clk,
        d0,
        d1,
        d2,
        d3,
        None::<AnyIOPin>,
        None::<AnyIOPin>,
        &Default::default(),
    )?;
    let card = SdCardDriver::new_mmc(host, &SdCardConfiguration::new())?;
    let fatfs = Fatfs::new_sdcard(0, card)?;
    // esp_vfs_fat_sdmmc_mount by hand: register drive "0:" under /sdcard, then mount it.
    unsafe {
        use esp_idf_svc::sys::{esp, esp_vfs_fat_register, f_mount, FATFS, FRESULT_FR_OK};
        let mut fs: *mut FATFS = std::ptr::null_mut();
        esp!(esp_vfs_fat_register(c"/sdcard".as_ptr(), c"0:".as_ptr(), 5, &mut fs))?;
        let res = f_mount(fs, c"0:".as_ptr(), 1);
        if res != FRESULT_FR_OK {
            return Err(anyhow!("f_mount: {res}"));
        }
    }
    // Lives for the rest of the run: dropping it would unregister the disk under the VFS.
    std::mem::forget(fatfs);
    MOUNTED.store(true, Ordering::Relaxed);
    Ok(())
}

pub fn ok() -> bool {
    MOUNTED.load(Ordering::Relaxed)
}

/// Writes `text` to `/sdcard/<name>` (a whole-file replace), if there is a card.
pub fn save(name: &str, text: &str) {
    if !ok() {
        return;
    }
    if let Err(e) = std::fs::write(format!("{ROOT}/{name}"), text) {
        log::warn!("sd: write {name}: {e}");
    }
}

pub fn load(name: &str) -> Option<String> {
    ok().then(|| std::fs::read_to_string(format!("{ROOT}/{name}")).ok()).flatten()
}

/// One line at the end of the error log, if there is a card.
pub fn log_line(line: &str) {
    use std::io::Write as _;
    if !ok() {
        return;
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(LOG);
    if let Err(e) = file.and_then(|mut f| writeln!(f, "{line}")) {
        log::warn!("sd: log: {e}");
    }
}

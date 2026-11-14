//! Settings kept in flash (NVS): the connection settings and the time-zone
//! offset. Also sets the system clock, since the offset is what makes it usable offline.

use anyhow::Result;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use std::sync::Mutex;

const NVS_NAMESPACE: &str = "ironpaw";

static NVS: Mutex<Option<EspNvs<NvsDefault>>> = Mutex::new(None);

/// Opens the namespace. Call once, before anything else here.
pub fn open(partition: EspDefaultNvsPartition) -> Result<()> {
    *NVS.lock().unwrap() = Some(EspNvs::new(partition, NVS_NAMESPACE, true)?);
    Ok(())
}

/// A saved string setting, if there is one.
pub fn get_str(key: &str) -> Result<Option<String>> {
    let mut buf = [0u8; 256];
    let nvs = NVS.lock().unwrap();
    Ok(nvs.as_ref().expect("nvs::open runs first").get_str(key, &mut buf)?.map(str::to_string))
}

/// Zone offset in seconds, from the last time Ironpaw told it; 0 until then.
pub fn saved_offset() -> i32 {
    NVS.lock().unwrap().as_ref().and_then(|nvs| nvs.get_i32("offset").ok().flatten()).unwrap_or(0)
}

/// Sets the system clock (it keeps running across restarts while the board has power) and
/// remembers the zone offset, so local time works offline too.
pub fn set_time(unix: i64, offset: i32) {
    let tv = esp_idf_svc::sys::timeval { tv_sec: unix as _, tv_usec: 0 };
    unsafe { esp_idf_svc::sys::settimeofday(&tv, std::ptr::null()) };
    if let Some(nvs) = NVS.lock().unwrap().as_mut() {
        if nvs.get_i32("offset").ok().flatten() != Some(offset) {
            if let Err(e) = nvs.set_i32("offset", offset) {
                log::warn!("nvs: {e}");
            }
        }
    }
}

/// Seconds since the Unix epoch, UTC.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

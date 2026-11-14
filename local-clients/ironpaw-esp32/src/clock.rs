//! Local time on the board, without a time-zone database: Ironpaw sends the Unix time and the
//! zone offset, the board sets its system clock (which keeps running across restarts while
//! powered) and does the rest here. No ESP-IDF, so it tests on the host:
//! `rustc --test --edition 2021 src/clock.rs -o /tmp/clock && /tmp/clock`.

/// Seconds of local time, from a Unix time and the zone offset. `None` before the clock is set
/// (ESP-IDF boots in 1970).
pub fn local(unix: i64, offset: i32) -> Option<i64> {
    (unix > 1_600_000_000).then_some(unix + offset as i64)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) for days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (era * 400 + yoe + (m <= 2) as i64, m, d)
}

/// "YYYY-MM-DDTHH:MM" (what Ironpaw sends for an event) as local seconds.
pub fn parse(at: &str) -> Option<i64> {
    let b = at.as_bytes();
    if b.len() < 16 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| at.get(r)?.parse::<i64>().ok();
    let (y, m, d, h, min) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 {
        return None;
    }
    Some(days_from_civil(y, m as u32, d as u32) * 86_400 + h * 3600 + min * 60)
}

/// "10:42"
pub fn hhmm(local: i64) -> String {
    let s = local.rem_euclid(86_400);
    format!("{:02}:{:02}", s / 3600, s % 3600 / 60)
}

/// "2026-11-02 10:42"
pub fn stamp(local: i64) -> String {
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02} {}", hhmm(local))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_round_trip() {
        let t = parse("2026-11-02T10:42").unwrap();
        assert_eq!(stamp(t), "2026-11-02 10:42");
        assert_eq!(hhmm(t), "10:42");
        assert_eq!(parse("2000-02-29T00:00").unwrap(), 951_782_400);
        assert_eq!(stamp(0), "1970-01-01 00:00");
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["soon", "2026-13-01T10:00", "2026-11-02 10:00", "2026-11-02T24:00", ""] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn unset_clock_is_none() {
        assert_eq!(local(0, -10_800), None);
        assert_eq!(local(1_767_225_600, -10_800), Some(1_767_214_800));
    }
}

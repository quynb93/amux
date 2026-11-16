//! Just enough calendar math for ISO-8601 timestamps, without a date crate.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    to_ms(SystemTime::now())
}

pub fn to_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe as i64 + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Formats like JavaScript's `Date.prototype.toISOString`: `2026-01-02T03:04:05.678Z`.
pub fn iso(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// Parses `YYYY-MM-DD[THH:MM[:SS[.fff]]][Z|±HH:MM]` into milliseconds since the epoch (UTC when no offset).
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.trim().as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = b.get(r)?;
        if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    let y = num(0..4)?;
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let (mo, d) = (num(5..7)? as u32, num(8..10)? as u32);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, mo, d) * 86_400_000;
    let mut i = 10;
    if matches!(b.get(i), Some(b'T') | Some(b' ')) {
        let h = num(i + 1..i + 3)?;
        if b.get(i + 3) != Some(&b':') {
            return None;
        }
        let mi = num(i + 4..i + 6)?;
        ms += h * 3_600_000 + mi * 60_000;
        i += 6;
        if b.get(i) == Some(&b':') {
            ms += num(i + 1..i + 3)? * 1000;
            i += 3;
            if b.get(i) == Some(&b'.') {
                let start = i + 1;
                let mut end = start;
                while b.get(end).is_some_and(u8::is_ascii_digit) {
                    end += 1;
                }
                let frac = std::str::from_utf8(&b[start..end]).ok()?;
                let padded: String = frac.chars().chain("000".chars()).take(3).collect();
                ms += padded.parse::<i64>().ok()?;
                i = end;
            }
        }
        match b.get(i) {
            None | Some(b'Z') | Some(b'z') => {}
            Some(sign @ (b'+' | b'-')) => {
                let oh = num(i + 1..i + 3)?;
                let om = if b.get(i + 3) == Some(&b':') {
                    num(i + 4..i + 6)?
                } else {
                    num(i + 3..i + 5).unwrap_or(0)
                };
                let off = (oh * 60 + om) * 60_000;
                ms += if *sign == b'+' { -off } else { off };
            }
            _ => return None,
        }
    }
    Some(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for ms in [0, 1_700_000_000_123, 951_782_400_000, -86_400_000] {
            assert_eq!(parse_iso(&iso(ms)), Some(ms));
        }
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            parse_iso("2026-01-01T00:00:00+01:00"),
            parse_iso("2025-12-31T23:00:00Z")
        );
        assert_eq!(
            parse_iso("2026-02-03"),
            parse_iso("2026-02-03T00:00:00.000Z")
        );
        assert_eq!(parse_iso("nope"), None);
    }
}

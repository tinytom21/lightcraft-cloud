//! The time the background engine goes by. The real one is the system clock; tests move a fake one by hand, so nothing
//! sleeps.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that only moves when told to.
pub struct FakeClock(Mutex<SystemTime>);

impl FakeClock {
    /// Starts at 2026-11-01 00:00:00 UTC unless given a time.
    pub fn new() -> Self {
        FakeClock(Mutex::new(UNIX_EPOCH + Duration::from_secs(1_793_491_200)))
    }

    pub fn advance(&self, d: Duration) {
        let mut t = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *t += d;
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// `YYYY-mm-ddTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)` (RFC 3339, as restic writes it) as a time.
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut nanos = 0u32;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        nanos = format!("{:0<9}", &digits[..digits.len().min(9)]).parse().ok()?;
        rest = &frac[digits.len()..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (oh, om) = rest[1..].split_once(':')?;
            sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60)
        }
    };
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2.rem_euclid(400);
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset;
    u64::try_from(secs).ok().map(|s| UNIX_EPOCH + Duration::new(s, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(t: SystemTime) -> u64 {
        t.duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn rfc3339_times_parse_with_offsets_and_fractions() {
        assert_eq!(secs(parse_rfc3339("2026-11-01T00:00:00Z").unwrap()), 1_793_491_200);
        assert_eq!(secs(parse_rfc3339("2026-11-01T01:00:00+01:00").unwrap()), 1_793_491_200);
        assert_eq!(secs(parse_rfc3339("2026-10-31T19:00:00-05:00").unwrap()), 1_793_491_200);
        let t = parse_rfc3339("2026-11-01T00:00:00.123456789012+00:00").unwrap();
        // (Windows keeps times to 100 ns, so the last two digits are the system's, not the parser's)
        let (s, n) = (secs(t), t.duration_since(UNIX_EPOCH).unwrap().subsec_nanos());
        assert_eq!((s, n / 100), (1_793_491_200, 1_234_567));
        if !cfg!(windows) {
            assert_eq!(n, 123_456_789);
        }
        assert_eq!(secs(parse_rfc3339("2024-02-29T12:00:00Z").unwrap()), 1_709_208_000);
        for bad in ["", "yesterday", "2026-13-01T00:00:00Z", "2026-11-01T00:00:00", "2026-11-01T00:00:00.Z", "2026-11-01T25:00:00Z"] {
            assert!(parse_rfc3339(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_fake_clock_only_moves_when_advanced() {
        let c = FakeClock::new();
        let a = c.now();
        assert_eq!(c.now(), a);
        c.advance(Duration::from_secs(90));
        assert_eq!(c.now().duration_since(a).unwrap(), Duration::from_secs(90));
    }
}

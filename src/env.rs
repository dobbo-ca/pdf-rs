//! Time and randomness for the writer, and Go's time formatting (docs/design.md section 10).
//! The crate reads the clock and the random source only through a [`WriteEnv`], so the parity
//! oracle can pin both.

use crate::error::Error;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

/// A point in time and the UTC offset it is shown in: what the writer needs of Go's
/// `time.Time`. There is no monotonic reading and no sub-second part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoTime {
    /// Seconds since 1970-01-01T00:00:00Z.
    pub unix: i64,
    /// The offset east of UTC, in minutes. 0 is UTC.
    pub utc_offset_min: i32,
}

/// The local calendar fields of a [`GoTime`].
struct Civil {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
}

impl GoTime {
    fn civil(&self) -> Civil {
        let local = self
            .unix
            .saturating_add(i64::from(self.utc_offset_min) * 60);
        let days = local.div_euclid(86_400);
        let sod = local.rem_euclid(86_400);
        // Howard Hinnant's civil_from_days.
        let z = days.saturating_add(719_468);
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era.saturating_mul(400) + i64::from(month <= 2);
        Civil {
            year,
            month,
            day,
            hour: sod / 3600,
            minute: sod % 3600 / 60,
            second: sod % 60,
        }
    }

    /// `(sign, hh, mm)` of the offset, as Go prints it.
    fn offset_parts(&self) -> (char, i32, i32) {
        let m = self.utc_offset_min;
        let sign = if m < 0 { '-' } else { '+' };
        let a = m.saturating_abs();
        (sign, a / 60, a % 60)
    }

    /// `time.Time.String()` for a time with no monotonic reading:
    /// `2006-01-02 15:04:05 -0700 MST`. The zone name is `UTC` at offset 0 and, as Go prints a
    /// zone without a name, the offset again otherwise. There is no fractional part.
    pub fn go_string(&self) -> String {
        let c = self.civil();
        let (sign, hh, mm) = self.offset_parts();
        let year = if c.year < 0 {
            format!("-{:04}", c.year.unsigned_abs())
        } else {
            format!("{:04}", c.year)
        };
        let zone = if self.utc_offset_min == 0 {
            "UTC".to_string()
        } else {
            format!("{sign}{hh:02}{mm:02}")
        };
        format!(
            "{year}-{:02}-{:02} {:02}:{:02}:{:02} {sign}{hh:02}{mm:02} {zone}",
            c.month, c.day, c.hour, c.minute, c.second
        )
    }

    /// `types.DateString` (types/date.go:27): `D:YYYYMMDDHHmmSS+hh'mm'`.
    pub fn date_string(&self) -> String {
        let c = self.civil();
        let (sign, hh, mm) = self.offset_parts();
        format!(
            "D:{}{:02}{:02}{:02}{:02}{:02}{sign}{hh:02}'{mm:02}'",
            c.year, c.month, c.day, c.hour, c.minute, c.second
        )
    }
}

/// The clock and the random source of one write. `Default` is the real clock (UTC) and
/// `/dev/urandom`; [`WriteEnv::parity`] is pinned for the parity oracle.
#[derive(Debug, Clone, Copy)]
pub struct WriteEnv {
    /// Go's `time.Now()` (`TimeNow` in the patched oracle). Used for the info dictionary dates and
    /// the file identifier.
    pub now: GoTime,
    /// Fills the slice with random bytes (Go's `crypto/rand.Reader`, `RandReader` in the patched
    /// oracle). Used for the AES initialisation vectors of the encrypted write fallback.
    pub random: fn(&mut [u8]) -> Result<(), Error>,
}

/// Fills `buf` from `/dev/urandom`. Std only: no `getrandom` dependency (docs/design.md section
/// 1, decision 11). Unix only.
///
/// # Errors
/// [`Error::Corrupt`] when `/dev/urandom` cannot be opened or read.
pub fn os_random(buf: &mut [u8]) -> Result<(), Error> {
    let mut f = std::fs::File::open("/dev/urandom")
        .map_err(|e| Error::Corrupt(format!("random source: {}", e.kind())))?;
    f.read_exact(buf)
        .map_err(|e| Error::Corrupt(format!("random source: {}", e.kind())))
}

/// The parity random source: byte `i` of each request is `i mod 256`.
///
/// # Errors
/// None; the signature is the one of [`WriteEnv::random`].
pub fn parity_random(buf: &mut [u8]) -> Result<(), Error> {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i % 256) as u8;
    }
    Ok(())
}

impl WriteEnv {
    /// 2000-01-01T00:00:00Z and [`parity_random`]: what the Go oracle's hooks return.
    pub fn parity() -> Self {
        Self {
            now: GoTime {
                unix: 946_684_800,
                utc_offset_min: 0,
            },
            random: parity_random,
        }
    }
}

impl Default for WriteEnv {
    fn default() -> Self {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        Self {
            now: GoTime {
                unix,
                utc_offset_min: 0,
            },
            random: os_random,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(unix: i64, off: i32) -> GoTime {
        GoTime {
            unix,
            utc_offset_min: off,
        }
    }

    #[test]
    fn go_string_utc() {
        assert_eq!(
            t(946_684_800, 0).go_string(),
            "2000-01-01 00:00:00 +0000 UTC"
        );
        assert_eq!(t(0, 0).go_string(), "1970-01-01 00:00:00 +0000 UTC");
        assert_eq!(
            t(1_700_000_000, 0).go_string(),
            "2023-11-14 22:13:20 +0000 UTC"
        );
        assert_eq!(t(-86_400, 0).go_string(), "1969-12-31 00:00:00 +0000 UTC");
        assert_eq!(
            t(951_782_400, 0).go_string(),
            "2000-02-29 00:00:00 +0000 UTC"
        );
        assert_eq!(
            t(1_709_164_800 + 86_399, 0).go_string(),
            "2024-02-29 23:59:59 +0000 UTC"
        );
        assert_eq!(
            t(1_709_164_800 + 86_400, 0).go_string(),
            "2024-03-01 00:00:00 +0000 UTC"
        );
    }

    #[test]
    fn go_string_fixed_offsets_repeat_the_offset() {
        assert_eq!(
            t(1_700_000_000, 330).go_string(),
            "2023-11-15 03:43:20 +0530 +0530"
        );
        assert_eq!(
            t(1_700_000_000, -480).go_string(),
            "2023-11-14 14:13:20 -0800 -0800"
        );
    }

    #[test]
    fn date_string_matches_go() {
        assert_eq!(t(946_684_800, 0).date_string(), "D:20000101000000+00'00'");
        assert_eq!(
            t(1_700_000_000, 330).date_string(),
            "D:20231115034320+05'30'"
        );
        assert_eq!(
            t(1_700_000_000, -480).date_string(),
            "D:20231114141320-08'00'"
        );
    }

    #[test]
    fn parity_env_is_pinned() {
        let e = WriteEnv::parity();
        assert_eq!(e.now.go_string(), "2000-01-01 00:00:00 +0000 UTC");
        let mut b = [0u8; 300];
        (e.random)(&mut b).unwrap();
        assert_eq!((b[0], b[1], b[255], b[256], b[299]), (0, 1, 255, 0, 43));
    }

    #[test]
    fn default_env_has_a_clock_and_randomness() {
        let e = WriteEnv::default();
        assert!(e.now.unix > 1_700_000_000);
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        (e.random)(&mut a).unwrap();
        (e.random)(&mut b).unwrap();
        assert_ne!(a, b);
    }
}

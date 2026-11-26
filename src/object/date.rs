//! PDF dates (Go: types/date.go). `DateString` and `DateTime` work on [`GoTime`], which has the
//! two things of `time.Time` they use: the instant and the offset it is shown in.
//!
//! `DateTime` keeps Go's quirks: `strconv.Atoi` accepts a sign (so `-5` is a valid hour), the
//! time zone hours are taken modulo 24, a negative offset keeps its minutes positive, and an
//! input that is not a PDF date is tried against four `time.Parse` layouts. Sub-second parts are
//! accepted and dropped; the time zone offset is saturated to what `GoTime` can hold.

use super::text::{decode_utf16_string, is_string_utf16be};
use crate::env::GoTime;

/// `DateString`: `D:YYYYMMDDHHmmSS+hh'mm'`.
pub fn date_string(t: &GoTime) -> String {
    t.date_string()
}

/// Days from 1970-01-01 to the civil date (proleptic Gregorian, any year).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// `daysIn`: the days of month `m` (1 to 12).
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `time.Date(y, m, d, h, min, s, 0, FixedZone("", off_min*60))`. Out-of-range fields roll over
/// as in Go.
fn zoned_time(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, off_min: i64) -> GoTime {
    let unix = days_from_civil(y, mo, d)
        .saturating_mul(86_400)
        .saturating_add(h.saturating_mul(3600))
        .saturating_add(mi.saturating_mul(60))
        .saturating_add(s)
        .saturating_sub(off_min.saturating_mul(60));
    let utc_offset_min =
        i32::try_from(off_min).unwrap_or(if off_min < 0 { i32::MIN } else { i32::MAX });
    GoTime {
        unix,
        utc_offset_min,
    }
}

fn utc_time(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> GoTime {
    zoned_time(y, mo, d, h, mi, s, 0)
}

/// `strconv.Atoi`.
fn atoi(s: &[u8]) -> Option<i64> {
    std::str::from_utf8(s).ok()?.parse::<i64>().ok()
}

/// The first char of `s` and its length in bytes, if `s` starts with valid UTF-8.
fn first_char(s: &[u8]) -> Option<(char, usize)> {
    let chunk = &s[..s.len().min(4)];
    let valid = match std::str::from_utf8(chunk) {
        Ok(v) => v,
        Err(e) => std::str::from_utf8(&chunk[..e.valid_up_to()]).ok()?,
    };
    valid.chars().next().map(|c| (c, c.len_utf8()))
}

/// The last char of `s` and its length in bytes, if `s` ends with valid UTF-8.
fn last_char(s: &[u8]) -> Option<(char, usize)> {
    let n = s.len();
    for back in 1..=n.min(4) {
        let tail = &s[n - back..];
        if let Ok(v) = std::str::from_utf8(tail)
            && v.chars().count() == 1
        {
            return v.chars().next().map(|c| (c, back));
        }
    }
    None
}

/// `strings.TrimSpace`: Unicode white space at both ends; invalid UTF-8 is not space.
fn trim_space(s: &[u8]) -> &[u8] {
    let mut s = s;
    while let Some((c, n)) = first_char(s)
        && c.is_whitespace()
    {
        s = &s[n..];
    }
    while let Some((c, n)) = last_char(s)
        && c.is_whitespace()
    {
        s = &s[..s.len() - n];
    }
    s
}

/// `prevalidateDate`: the text to parse, or `None` if `s` cannot be a date.
fn prevalidate_date(s: &[u8], relaxed: bool) -> Option<Vec<u8>> {
    // utf16 conversion if applicable.
    let decoded: Vec<u8>;
    let mut t: &[u8] = if is_string_utf16be(s) {
        decoded = decode_utf16_string(s).ok()?.into_bytes();
        &decoded
    } else {
        s
    };

    if let Some(r) = t.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        t = r;
    }

    // Remove trailing 0x00
    while let Some((&0, rest)) = t.split_last() {
        t = rest;
    }

    if relaxed {
        // Accept a missing "D:" prefix. "YYYY" is mandatory.
        if let Some(r) = t.strip_prefix(b"D:") {
            t = r;
        }
        let v: Vec<u8> = trim_space(t)
            .iter()
            .copied()
            .filter(|&c| c != b'.' && c != b'\\')
            .collect();
        return (v.len() >= 4).then_some(v);
    }

    // "D:YYYY" is mandatory
    if t.len() < 6 || !t.starts_with(b"D:") {
        return None;
    }
    Some(t[2..].to_vec())
}

fn parse_timezone_hours(s: &[u8], o: u8) -> Option<i64> {
    // Opinionated hack.
    let tzh = atoi(s)? % 24;
    if o == b'Z' && tzh != 0 {
        return None;
    }
    Some(tzh)
}

fn parse_timezone_minutes(s: &[u8], o: u8) -> Option<i64> {
    let tzm = atoi(s)?;
    if tzm > 59 {
        return None;
    }
    if o == b'Z' && tzm != 0 {
        return None;
    }
    Some(tzm)
}

fn timezone_separator(c: u8) -> bool {
    matches!(c, b'+' | b'-' | b'Z')
}

fn empty_time_zone(t: &[u8], relaxed: bool) -> bool {
    t.is_empty() || (relaxed && (t == b"'" || t == b"'0"))
}

/// `parseTimezone`: `(hours, minutes)` of the offset. `None` is Go's `ok == false`.
fn parse_timezone(s: &[u8], off: usize, relaxed: bool) -> Option<(i64, i64)> {
    let o = *s.get(off)?;

    if !timezone_separator(o) || (s.len() == off + 1 && o != b'Z') {
        // Ignore an invalid separator or an incomplete signed offset if relaxed.
        return relaxed.then_some((0, 0));
    }

    // A local time equal to UT: "YYYYMMDDHHmmSSZ", or if relaxed "...Z'" and "...Z'0". The same
    // test serves the "HH'mm" part.
    let mut rest = s.get(off + 1..)?;
    if empty_time_zone(rest, relaxed) {
        return Some((0, 0));
    }
    if let Some(r) = rest.strip_prefix(b"-") {
        rest = r;
    }
    let rest: Vec<u8> = rest
        .iter()
        .map(|&c| if c == b' ' { b'0' } else { c })
        .collect();
    let ss: Vec<&[u8]> = rest.split(|&c| c == b'\'').collect();

    let mut tzh = parse_timezone_hours(ss.first()?, o)?;
    if o == b'-' {
        tzh = -tzh;
    }

    match ss.get(1) {
        // Ignore missing timezone minutes.
        None => Some((tzh, 0)),
        Some(m) if m.is_empty() && ss.len() == 2 => Some((tzh, 0)),
        Some(m) => Some((tzh, parse_timezone_minutes(m, o)?)),
    }
}

/// `parseYear`: `(year, finished)`.
fn parse_year(s: &[u8]) -> Option<(i64, bool)> {
    let y = atoi(s.get(0..4)?)?;
    match s.len() {
        4 => Some((y, true)),
        5 => None,
        _ => Some((y, false)),
    }
}

/// `parseMonth`.
fn parse_month(s: &[u8]) -> Option<(i64, bool)> {
    let m = atoi(s.get(4..6)?)?;
    if !(1..=12).contains(&m) {
        return None;
    }
    match s.len() {
        6 => Some((m, true)),
        7 => None,
        _ => Some((m, false)),
    }
}

/// `parseDay`: also checks the day against the month.
fn parse_day(s: &[u8], y: i64, m: i64) -> Option<(i64, bool)> {
    let d = atoi(s.get(6..8)?)?;
    if !(1..=31).contains(&d) || d > days_in_month(y, m) {
        return None;
    }
    match s.len() {
        8 => Some((d, true)),
        9 => None,
        _ => Some((d, false)),
    }
}

/// `parseHour`. There is no lower bound: Go's `Atoi` accepts a sign.
fn parse_hour(s: &[u8]) -> Option<(i64, bool)> {
    let h = atoi(s.get(8..10)?)?;
    if h > 23 {
        return None;
    }
    match s.len() {
        10 => Some((h, true)),
        11 => None,
        _ => Some((h, false)),
    }
}

/// `parseMinute`.
fn parse_minute(s: &[u8]) -> Option<(i64, bool)> {
    let min = atoi(s.get(10..12)?)?;
    if min > 59 {
        return None;
    }
    match s.len() {
        12 => Some((min, true)),
        13 => None,
        _ => Some((min, false)),
    }
}

/// `parseSecond`: `(seconds, finished, offset of the time zone)`. A time zone separator in the
/// second digit position makes it a one-digit second.
fn parse_second(s: &[u8]) -> Option<(i64, bool, usize)> {
    let two = s.get(12..14)?;
    let (second, off) = if two.get(1).is_some_and(|&c| timezone_separator(c)) {
        (&two[..1], 13)
    } else {
        (two, 14)
    };
    let sec = atoi(second)?;
    if sec > 59 {
        return None;
    }
    Some((sec, s.len() == 14, off))
}

// The pieces of the four layouts of `digestPopularOutOfSpecDates`.
#[derive(Clone, Copy)]
enum Tok {
    Lit(&'static [u8]),
    /// `Mon`
    WeekDay,
    /// `Monday`
    LongWeekDay,
    /// `Jan`
    Month,
    /// `January`
    LongMonth,
    /// `1`
    NumMonth,
    /// `2`
    Day,
    /// `02`
    ZeroDay,
    /// `15`
    Hour,
    /// `3`
    Hour12,
    /// `04`
    ZeroMinute,
    /// `05`
    ZeroSecond,
    /// `2006`
    LongYear,
    /// `PM`
    Pm,
}

/// `Mon Jan 2 15:04:05 2006`
const LAYOUT_ANSIC: &[Tok] = &[
    Tok::WeekDay,
    Tok::Lit(b" "),
    Tok::Month,
    Tok::Lit(b" "),
    Tok::Day,
    Tok::Lit(b" "),
    Tok::Hour,
    Tok::Lit(b":"),
    Tok::ZeroMinute,
    Tok::Lit(b":"),
    Tok::ZeroSecond,
    Tok::Lit(b" "),
    Tok::LongYear,
];

/// `Monday, January 02, 2006 3:04:05 PM`
const LAYOUT_LONG: &[Tok] = &[
    Tok::LongWeekDay,
    Tok::Lit(b", "),
    Tok::LongMonth,
    Tok::Lit(b" "),
    Tok::ZeroDay,
    Tok::Lit(b", "),
    Tok::LongYear,
    Tok::Lit(b" "),
    Tok::Hour12,
    Tok::Lit(b":"),
    Tok::ZeroMinute,
    Tok::Lit(b":"),
    Tok::ZeroSecond,
    Tok::Lit(b" "),
    Tok::Pm,
];

/// `1/2/2006 15:04:05`
const LAYOUT_SLASH: &[Tok] = &[
    Tok::NumMonth,
    Tok::Lit(b"/"),
    Tok::Day,
    Tok::Lit(b"/"),
    Tok::LongYear,
    Tok::Lit(b" "),
    Tok::Hour,
    Tok::Lit(b":"),
    Tok::ZeroMinute,
    Tok::Lit(b":"),
    Tok::ZeroSecond,
];

/// `Mon, Jan 2, 2006`
const LAYOUT_SHORT: &[Tok] = &[
    Tok::WeekDay,
    Tok::Lit(b", "),
    Tok::Month,
    Tok::Lit(b" "),
    Tok::Day,
    Tok::Lit(b", "),
    Tok::LongYear,
];

const SHORT_DAYS: [&[u8]; 7] = [b"Sun", b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat"];
const LONG_DAYS: [&[u8]; 7] = [
    b"Sunday",
    b"Monday",
    b"Tuesday",
    b"Wednesday",
    b"Thursday",
    b"Friday",
    b"Saturday",
];
const SHORT_MONTHS: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];
const LONG_MONTHS: [&[u8]; 12] = [
    b"January",
    b"February",
    b"March",
    b"April",
    b"May",
    b"June",
    b"July",
    b"August",
    b"September",
    b"October",
    b"November",
    b"December",
];

/// `time.match`: equal, ignoring the case of letters.
fn match_fold(s1: &[u8], s2: &[u8]) -> bool {
    s1.iter().zip(s2).all(|(&c1, &c2)| {
        if c1 == c2 {
            return true;
        }
        let (a, b) = (c1 | 0x20, c2 | 0x20);
        a == b && a.is_ascii_lowercase()
    })
}

/// `time.lookup`: the index of the first name that `val` starts with, and what follows it.
fn lookup<'a>(tab: &[&[u8]], val: &'a [u8]) -> Option<(usize, &'a [u8])> {
    for (i, v) in tab.iter().enumerate() {
        if let Some(head) = val.get(..v.len())
            && match_fold(head, v)
        {
            return Some((i, &val[v.len()..]));
        }
    }
    None
}

fn cutspace(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&c| c == b' ').count();
    &s[n..]
}

/// `time.skip`: `prefix` must match; a space in it matches a run of spaces, or the end of input.
fn skip<'a>(mut value: &'a [u8], mut prefix: &[u8]) -> Option<&'a [u8]> {
    while let Some(&p0) = prefix.first() {
        if p0 == b' ' {
            if value.first().is_some_and(|&c| c != b' ') {
                return None;
            }
            prefix = cutspace(prefix);
            value = cutspace(value);
            continue;
        }
        if value.first() != Some(&p0) {
            return None;
        }
        prefix = &prefix[1..];
        value = &value[1..];
    }
    Some(value)
}

/// `time.getnum`: one or two digits (exactly two if `fixed`).
fn getnum(s: &[u8], fixed: bool) -> Option<(i64, &[u8])> {
    let a = *s.first().filter(|c| c.is_ascii_digit())?;
    match s.get(1).filter(|c| c.is_ascii_digit()) {
        Some(&b) => Some((i64::from(a - b'0') * 10 + i64::from(b - b'0'), &s[2..])),
        None if fixed => None,
        None => Some((i64::from(a - b'0'), &s[1..])),
    }
}

/// `time.Parse(layout, value)` for the layouts above, which have no zone, so the time is UTC.
fn time_parse(layout: &[Tok], value: &[u8]) -> Option<GoTime> {
    let mut v = value;
    let (mut year, mut month, mut day) = (0i64, -1i64, -1i64);
    let (mut hour, mut min) = (0i64, 0i64);
    let mut sec = 0i64;
    let (mut am, mut pm) = (false, false);

    for tok in layout {
        match *tok {
            Tok::Lit(p) => v = skip(v, p)?,
            Tok::WeekDay => v = lookup(&SHORT_DAYS, v)?.1,
            Tok::LongWeekDay => v = lookup(&LONG_DAYS, v)?.1,
            Tok::Month => {
                let (i, rest) = lookup(&SHORT_MONTHS, v)?;
                month = i as i64 + 1;
                v = rest;
            }
            Tok::LongMonth => {
                let (i, rest) = lookup(&LONG_MONTHS, v)?;
                month = i as i64 + 1;
                v = rest;
            }
            Tok::NumMonth => {
                let (m, rest) = getnum(v, false)?;
                if !(1..=12).contains(&m) {
                    return None;
                }
                month = m;
                v = rest;
            }
            Tok::Day | Tok::ZeroDay => {
                // Any one- or two-digit day; the whole date is validated below.
                let (d, rest) = getnum(v, matches!(tok, Tok::ZeroDay))?;
                day = d;
                v = rest;
            }
            Tok::Hour => {
                let (h, rest) = getnum(v, false)?;
                if h >= 24 {
                    return None;
                }
                hour = h;
                v = rest;
            }
            Tok::Hour12 => {
                let (h, rest) = getnum(v, false)?;
                if h > 12 {
                    return None;
                }
                hour = h;
                v = rest;
            }
            Tok::ZeroMinute => {
                let (m, rest) = getnum(v, true)?;
                if m >= 60 {
                    return None;
                }
                min = m;
                v = rest;
            }
            Tok::ZeroSecond => {
                let (s, rest) = getnum(v, true)?;
                if s >= 60 {
                    return None;
                }
                sec = s;
                v = rest;
                // A fractional second that the layout does not have is accepted and dropped.
                if let [b'.' | b',', d, tail @ ..] = v
                    && d.is_ascii_digit()
                {
                    v = &tail[tail.iter().take_while(|c| c.is_ascii_digit()).count()..];
                }
            }
            Tok::LongYear => {
                if v.len() < 4 || !v[0].is_ascii_digit() {
                    return None;
                }
                year = atoi(&v[..4])?;
                v = &v[4..];
            }
            Tok::Pm => {
                match v.get(..2)? {
                    b"PM" => pm = true,
                    b"AM" => am = true,
                    _ => return None,
                }
                v = &v[2..];
            }
        }
    }

    // Extra text.
    if !v.is_empty() {
        return None;
    }
    if pm && hour < 12 {
        hour += 12;
    } else if am && hour == 12 {
        hour = 0;
    }

    // Validate the day of the month.
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some(utc_time(year, month, day, hour, min, sec))
}

/// `digestPopularOutOfSpecDates`.
fn digest_popular_out_of_spec_dates(s: &[u8]) -> Option<GoTime> {
    [LAYOUT_ANSIC, LAYOUT_LONG, LAYOUT_SLASH, LAYOUT_SHORT]
        .iter()
        .find_map(|l| time_parse(l, s))
}

/// `DateTime`: decodes `s` (7.9.4 Dates, `D:YYYYMMDDHHmmSSOHH'mm`). `relaxed` accepts a missing
/// `D:` prefix, stray dots, backslashes and blanks, and the short forms of the zone. `None` is
/// Go's `ok == false`.
pub fn date_time(s: &[u8], relaxed: bool) -> Option<GoTime> {
    let s = prevalidate_date(s, relaxed)?;
    let s = s.as_slice();

    let Some((y, finished)) = parse_year(s) else {
        // Try the workaround.
        return digest_popular_out_of_spec_dates(s);
    };
    if finished {
        return Some(utc_time(y, 1, 1, 0, 0, 0));
    }

    let (m, finished) = parse_month(s)?;
    if finished {
        return Some(utc_time(y, m, 1, 0, 0, 0));
    }

    let (day, finished) = parse_day(s, y, m)?;
    if finished {
        return Some(utc_time(y, m, day, 0, 0, 0));
    }

    let (h, finished) = parse_hour(s)?;
    if finished {
        return Some(utc_time(y, m, day, h, 0, 0));
    }

    let (min, finished) = parse_minute(s)?;
    if finished {
        return Some(utc_time(y, m, day, h, min, 0));
    }

    let (sec, finished, off) = parse_second(s)?;
    if finished {
        return Some(utc_time(y, m, day, h, min, sec));
    }

    // Process the timezone.
    let (tzh, tzm) = parse_timezone(s, off, relaxed)?;
    Some(zoned_time(
        y,
        m,
        day,
        h,
        min,
        sec,
        tzh.saturating_mul(60).saturating_add(tzm),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-07-13 09:53:58 UTC.
    const T2026: i64 = 1_783_936_438;

    fn ok(s: &str, relaxed: bool) {
        assert!(date_time(s.as_bytes(), relaxed).is_some(), "{s:?} invalid");
    }

    fn strict_ok(s: &[u8]) {
        assert!(date_time(s, false).is_some(), "{s:?} invalid");
    }

    #[test]
    fn parse_date_time() {
        // (D:YYYYMMDDHHmmSSOHH'mm); O = -, +, Z
        strict_ok(b"D:2017");
        // UTF-16 string "D:2017".
        strict_ok(b"\xfe\xff\x00\x44\x00\x3A\x00\x32\x00\x30\x00\x31\x00\x37");
        for s in [
            "D:201703",
            "D:20170430",
            "D:2017043015",
            "D:201704301559",
            "D:20170430155901Z",
            "D:20170430155901",
            "D:20170430155901+06'59",
            "D:20170430155901Z00",
            "D:20170430155901Z00'00",
            "D:20210602180254-06",
            "D:20170430155901+06'",
            "D:20210515103719-02'00",
            "D:2025022513222+01'00'",
        ] {
            ok(s, false);
        }
        assert!(date_time(b"D:20170430155901+66'A9", false).is_none());

        for s in [
            "D:20201222164228Z'",
            "D:20230912144809Z'0",
            "20250718155751+",
            "20141117162446Z00'00'",
            "D: 20210827124448+00'00'",
            "D: 20191003062617-07'00'",
            "D:20150521.124925823",
            "D:20210517043452}",
            "D:20210608122455Z00\\'00",
            "D:20020301230221- 5' 0'",
            "D:20061102145045-05'",
            "D:20150312082530-5'00'",
            "D:20191009100417-05'00''",
            "D:20200429084309+ 0' 0'",
            "D:20211028112621--04'00",
            "D:20210419150333-04'00'Z",
            "\u{feff}D:20160404061414+65'53'",
        ] {
            ok(s, true);
        }
    }

    #[test]
    fn date_time_timezone() {
        // (name, suffix, strict, relaxed, offset in seconds)
        let cases: [(&str, &str, bool, bool, i64); 13] = [
            ("terminal Z", "Z", true, true, 0),
            ("Z hours", "Z00", true, true, 0),
            ("Z hours and minutes", "Z00'00", true, true, 0),
            ("Z full offset", "Z00'00'", true, true, 0),
            ("no timezone", "", true, true, 0),
            ("positive offset", "+02'00'", true, true, 2 * 3600),
            ("negative offset", "-02'00'", true, true, -2 * 3600),
            ("terminal plus", "+", false, true, 0),
            ("terminal minus", "-", false, true, 0),
            ("Z apostrophe", "Z'", false, true, 0),
            ("Z apostrophe and zero", "Z'0", false, true, 0),
            ("Z nonzero hours", "Z01", false, false, 0),
            ("Z nonzero minutes", "Z00'01'", false, false, 0),
        ];
        for (name, suffix, strict, relaxed, offset) in cases {
            for (mode, is_relaxed, want_ok) in
                [("strict", false, strict), ("relaxed", true, relaxed)]
            {
                let s = format!("D:20260713095358{suffix}");
                let got = date_time(s.as_bytes(), is_relaxed);
                assert_eq!(got.is_some(), want_ok, "{name} {mode}");
                if let Some(t) = got {
                    // The instant is 2026-07-13 09:53:58 at that offset.
                    assert_eq!(t.unix, T2026 - offset, "{name} {mode}");
                    assert_eq!(i64::from(t.utc_offset_min) * 60, offset, "{name} {mode}");
                }
            }
        }
    }

    #[test]
    fn write_date_time_round_trips() {
        // DateString of a time in several zones parses back (strictly) to the same instant.
        for (offset_min, want) in [
            (0, "D:20260713095358+00'00'"),
            (120, "D:20260713115358+02'00'"),
            (-600, "D:20260712235358-10'00'"),
            (600, "D:20260713195358+10'00'"),
        ] {
            let t = GoTime {
                unix: T2026,
                utc_offset_min: offset_min,
            };
            let s = date_string(&t);
            assert_eq!(s, want);
            let back = date_time(s.as_bytes(), false);
            assert_eq!(back, Some(t), "{s}");
        }
    }

    #[test]
    fn partial_dates_and_validation() {
        let t = |s: &str| date_time(s.as_bytes(), false).map(|t| t.unix);
        assert_eq!(t("D:2017"), Some(1_483_228_800));
        assert_eq!(t("D:20170430155901"), Some(1_493_567_941));
        // 2017 is not a leap year.
        assert_eq!(t("D:20170229"), None);
        assert!(t("D:20160229").is_some());
        assert_eq!(t("D:20171301"), None);
        assert_eq!(t("D:2017043024"), None);
        // A year of five digits or a day of one digit is not a date.
        assert_eq!(t("D:20175"), None);
        assert_eq!(t("D:2017043"), None);
        // Strict needs the prefix and six bytes.
        assert_eq!(date_time(b"20170430", false), None);
        assert_eq!(date_time(b"D:20", false), None);
        // A sign is accepted by Atoi: hour -1 is before midnight.
        assert_eq!(
            date_time(b"D:20170430-1", false).map(|t| t.unix),
            Some(1_493_510_400 - 3600)
        );
        // A second digit that is a zone separator is a one-digit second.
        assert_eq!(
            date_time(b"D:2017043015591Z", false).map(|t| t.unix),
            Some(1_493_567_941)
        );
    }

    #[test]
    fn popular_out_of_spec_layouts() {
        let r = |s: &str| date_time(s.as_bytes(), true).map(|t| (t.unix, t.utc_offset_min));
        assert_eq!(r("Wed Sep 5 12:30:00 2018"), Some((1_536_150_600, 0)));
        // Names match in any case, and a day may have two digits.
        assert_eq!(r("wed sep 05 12:30:00 2018"), Some((1_536_150_600, 0)));
        assert_eq!(
            r("Monday, January 02, 2006 3:04:05 PM"),
            Some((1_136_214_245, 0))
        );
        assert_eq!(
            r("monday, january 02, 2006 3:04:05 AM"),
            Some((1_136_171_045, 0))
        );
        assert_eq!(
            r("Monday, January 02, 2006 12:04:05 AM"),
            Some((1_136_160_245, 0))
        );
        assert_eq!(r("1/2/2006 15:04:05"), Some((1_136_214_245, 0)));
        assert_eq!(r("Mon, Jan 2, 2006"), Some((1_136_160_000, 0)));
        // Out of range fields, a day that the month does not have, and trailing text.
        assert_eq!(r("Mon Jan 32 15:04:05 2006"), None);
        assert_eq!(r("Mon Jan 2 24:04:05 2006"), None);
        assert_eq!(r("2/30/2006 15:04:05"), None);
        assert_eq!(r("1/2/2006 15:04:05 x"), None);
        assert_eq!(r("13/2/2006 15:04:05"), None);
        // A fraction after the seconds is taken and dropped (`.` is stripped before this layer
        // in relaxed mode, so test the layout directly).
        assert_eq!(
            time_parse(LAYOUT_ANSIC, b"Mon Jan 2 15:04:05.123 2006").map(|t| t.unix),
            Some(1_136_214_245)
        );
        assert_eq!(
            time_parse(LAYOUT_SLASH, b"1/2/2006 15:04:05,5").map(|t| t.unix),
            Some(1_136_214_245)
        );
        // A layout space matches a run of spaces.
        assert_eq!(
            time_parse(LAYOUT_ANSIC, b"Mon   Jan  2   15:04:05   2006").map(|t| t.unix),
            Some(1_136_214_245)
        );
    }

    #[test]
    fn trim_space_is_unicode_aware() {
        assert_eq!(trim_space(b"  a b \t\n"), b"a b");
        assert_eq!(trim_space("\u{a0}\u{2003}x\u{3000}".as_bytes()), b"x");
        // Invalid bytes are not space and stop the trim.
        assert_eq!(trim_space(b" \xff "), b"\xff");
        assert_eq!(trim_space(b""), b"");
    }

    #[test]
    fn civil_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(0, 1, 1), -719_528);
        assert_eq!(days_from_civil(-1, 1, 1), -719_893);
        assert!(is_leap(2000) && !is_leap(1900) && is_leap(-4) && !is_leap(2023));
    }
}

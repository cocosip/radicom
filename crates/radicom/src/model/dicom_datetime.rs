//! DA / DT / TM parsing and formatting (ELEM-01 date-time family;
//! docs/01-architecture.md §3).
//!
//! Accepted input shapes mirror fo-dicom's format arrays:
//! - DA: `YYYY`, `YYYYMM`, `YYYYMMDD`, `YYYY.MM.DD`, `YYYY/MM/DD`
//! - DT: `YYYY[MM[DD[HH[MM[SS[.F{1,6}]]]]]]` plus optional UTC offset
//!   (`±HHMM`, `±HH:MM`, `±HH`), dotted/slashed date variants
//! - TM: `HH[MM[SS[.F{1,6}]]]` with `.`, `:` or none as fraction separator
//!
//! Ranges (`min-max`, at most two segments) parse into [`Range`].

use crate::error::{DataError, Error};
use crate::model::range::Range;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};

/// A DT point: wall-clock time with an optional UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtPoint {
    pub naive: NaiveDateTime,
    pub offset: Option<FixedOffset>,
}

impl DtPoint {
    pub fn to_fixed(self) -> Option<DateTime<FixedOffset>> {
        // `naive` is the local wall-clock reading, so attach the offset as
        // local time, not by treating the wall clock as UTC (review #11).
        self.offset
            .and_then(|o| o.from_local_datetime(&self.naive).single())
    }
}

fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn date_from_ymd(year: i32, month: u32, day: u32) -> Result<NaiveDate, Error> {
    NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| {
        Error::Data(DataError::Other {
            message: format!("invalid date {year:04}-{month:02}-{day:02}"),
        })
    })
}

fn time_from_hms(hour: u32, min: u32, sec: u32, nano: u32) -> Result<NaiveTime, Error> {
    NaiveTime::from_hms_nano_opt(hour, min, sec, nano).ok_or_else(|| {
        Error::Data(DataError::Other {
            message: format!("invalid time {hour:02}:{min:02}:{sec:02}.{nano:09}"),
        })
    })
}

fn parse_date_core(s: &str) -> Result<NaiveDate, Error> {
    let invalid = || {
        Error::Data(DataError::Other {
            message: format!("invalid DA value \"{s}\""),
        })
    };
    if let Some((y, rest)) = s.split_once(['.', '/']) {
        // yyyy.MM.dd / yyyy/MM/dd
        let (m, d) = rest.split_once(['.', '/']).ok_or_else(invalid)?;
        let year: i32 = digits(y)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?;
        let month = digits(m).ok_or_else(invalid)?;
        let day = digits(d).ok_or_else(invalid)?;
        return date_from_ymd(year, month, day);
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    match s.len() {
        4 => date_from_ymd(digits(s).ok_or_else(invalid)? as i32, 1, 1),
        6 => {
            let year: i32 = digits(&s[..4])
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?;
            date_from_ymd(year, digits(&s[4..6]).ok_or_else(invalid)?, 1)
        }
        8 => {
            let year: i32 = digits(&s[..4])
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?;
            date_from_ymd(
                year,
                digits(&s[4..6]).ok_or_else(invalid)?,
                digits(&s[6..8]).ok_or_else(invalid)?,
            )
        }
        _ => Err(invalid()),
    }
}

/// Parses a DA value or range.
pub fn parse_da(s: &str) -> Result<Range<NaiveDate>, Error> {
    match s.split_once('-') {
        None => {
            let d = parse_date_core(s)?;
            Ok(Range::new(d, d))
        }
        Some((a, b)) => {
            let min = parse_date_core(a)?;
            let max = if b.is_empty() {
                min
            } else {
                parse_date_core(b)?
            };
            Ok(Range::new(min, max))
        }
    }
}

/// Parses a TM value or range.
pub fn parse_tm(s: &str) -> Result<Range<NaiveTime>, Error> {
    fn one(s: &str) -> Result<NaiveTime, Error> {
        let invalid = || {
            Error::Data(DataError::Other {
                message: format!("invalid TM value \"{s}\""),
            })
        };
        if s.is_empty()
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'.' || b == b':')
        {
            return Err(invalid());
        }
        let (main, frac) = if s.contains(':') {
            // colon forms: HH:MM[:SS[:F]] — HH and MM are always 2 digits,
            // so after dropping separators the fraction starts at digit 6
            let digits_only: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits_only.len() < 2 || digits_only.len() > 12 {
                return Err(invalid());
            }
            let at = digits_only.len().min(6);
            (digits_only[..at].to_string(), digits_only[at..].to_string())
        } else if s.contains('.') {
            let parts: Vec<&str> = s.split('.').collect();
            match parts.len() {
                2 if parts[0].len() <= 2 => (parts[0].to_string(), parts[1].to_string()), // HH.mm
                2 => (parts[0].to_string(), parts[1].to_string()), // HHMMSS.f
                3 => (
                    format!("{}{}{}", parts[0], parts[1], parts[2]),
                    String::new(),
                ), // HH.mm.ss
                4 => (
                    format!("{}{}{}", parts[0], parts[1], parts[2]),
                    parts[3].to_string(), // HH.mm.ss.f
                ),
                _ => return Err(invalid()),
            }
        } else {
            (s.to_string(), String::new())
        };
        if !frac.bytes().all(|b| b.is_ascii_digit()) || frac.len() > 6 {
            return Err(invalid());
        }
        if main.len() > 6 || main.len() % 2 != 0 {
            return Err(invalid());
        }
        let hour = if main.is_empty() {
            0
        } else {
            digits(&main[..2]).ok_or_else(invalid)?
        };
        let minute = if main.len() >= 4 {
            digits(&main[2..4]).ok_or_else(invalid)?
        } else {
            0
        };
        let second = if main.len() >= 6 {
            digits(&main[4..6]).ok_or_else(invalid)?
        } else {
            0
        };
        let nano = if frac.is_empty() {
            0
        } else {
            let padded = format!("{frac:0<6}");
            digits(&padded).ok_or_else(invalid)? * 1_000
        };
        time_from_hms(hour, minute, second, nano)
    }
    match s.split_once('-') {
        None => {
            let t = one(s)?;
            Ok(Range::new(t, t))
        }
        Some((a, b)) => {
            let min = one(a)?;
            let max = if b.is_empty() { min } else { one(b)? };
            Ok(Range::new(min, max))
        }
    }
}

/// Parses a DT value or range.
pub fn parse_dt(s: &str) -> Result<Range<DtPoint>, Error> {
    fn one(s: &str) -> Result<DtPoint, Error> {
        let invalid = || {
            Error::Data(DataError::Other {
                message: format!("invalid DT value \"{s}\""),
            })
        };
        // extract a trailing UTC offset: ±HHMM / ±HH:MM / ±HH
        let (body, offset) = match s.find(['+', '-']) {
            Some(i) if i > 0 => {
                let os = &s[i..];
                let sign = if os.starts_with('+') { 1 } else { -1 };
                let nums: String = os[1..].chars().filter(|c| *c != ':').collect();
                // reject non-digits before slicing: a multibyte char like 'é'
                // would otherwise make `nums[..2]` panic on a char boundary
                // (review #10).
                if !nums.chars().all(|c| c.is_ascii_digit()) {
                    return Err(invalid());
                }
                let (oh, om) = match nums.len() {
                    2 => (nums.parse::<u32>().map_err(|_| invalid())?, 0),
                    4 => {
                        let h: u32 = nums[..2].parse().map_err(|_| invalid())?;
                        let m: u32 = nums[2..].parse().map_err(|_| invalid())?;
                        (h, m)
                    }
                    _ => return Err(invalid()),
                };
                // PS3.3 §6.2: positive offsets are limited to 14h, negative to 12h.
                let max_hour = if sign > 0 { 14 } else { 12 };
                if oh > max_hour || om > 59 {
                    return Err(invalid());
                }
                let off = FixedOffset::east_opt(sign * ((oh * 3600 + om * 60) as i32))
                    .ok_or_else(invalid)?;
                (&s[..i], Some(off))
            }
            _ => (s, None),
        };
        // dotted/slashed date forms never carry a time part
        let date_only = body.contains('/') || (body.len() > 4 && body.as_bytes()[4] == b'.');
        let (date, hour_s, frac_s) = if date_only {
            (parse_date_core(body)?, "", "")
        } else {
            // split date digits (4/6/8) from trailing time digits+fraction
            let digit_len = body
                .bytes()
                .take_while(|b| b.is_ascii_digit())
                .count()
                .min(8);
            let (date_s, rest_s) = body.split_at(digit_len);
            let date = parse_date_core(date_s)?;
            let (h, f) = match rest_s.find('.') {
                Some(i) => (&rest_s[..i], &rest_s[i + 1..]),
                None => (rest_s, ""),
            };
            (date, h, f)
        };
        if !frac_s.bytes().all(|b| b.is_ascii_digit()) || frac_s.len() > 6 {
            return Err(invalid());
        }
        let (hour, minute, second) = match hour_s.len() {
            0 => (0, 0, 0),
            2 => (digits(hour_s).ok_or_else(invalid)?, 0, 0),
            4 => (
                digits(&hour_s[..2]).ok_or_else(invalid)?,
                digits(&hour_s[2..4]).ok_or_else(invalid)?,
                0,
            ),
            6 => (
                digits(&hour_s[..2]).ok_or_else(invalid)?,
                digits(&hour_s[2..4]).ok_or_else(invalid)?,
                digits(&hour_s[4..6]).ok_or_else(invalid)?,
            ),
            _ => return Err(invalid()),
        };
        let nano = if frac_s.is_empty() {
            0
        } else {
            let padded = format!("{frac_s:0<6}");
            digits(&padded).ok_or_else(invalid)? * 1_000
        };
        let time = time_from_hms(hour, minute, second, nano)?;
        Ok(DtPoint {
            naive: NaiveDateTime::new(date, time),
            offset,
        })
    }
    // Try a single value first: a trailing -HH[MM][:MM] suffix binds as a
    // UTC offset, so only a failed single parse falls back to splitting
    // the value into a min-max range.
    if let Ok(p) = one(s) {
        return Ok(Range::new(p, p));
    }
    match s.split_once('-') {
        Some((a, b)) => {
            let min = one(a)?;
            let max = if b.is_empty() { min } else { one(b)? };
            Ok(Range::new(min, max))
        }
        None => one(s).map(|p| Range::new(p, p)),
    }
}

/// Validates DT offsets anywhere in the string: single-value parsing binds
/// a trailing suffix as an offset, but a range split can swallow an
/// out-of-range offset (e.g. `-1500`) as the range end; this pre-check
/// rejects such inputs outright.
pub fn has_invalid_dt_offset(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'+' || bytes[i] == b'-' {
            let nums: String = s[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let hour: u32 = match nums.len() {
                2 => nums.parse().unwrap_or(u32::MAX),
                4 => nums[..2].parse().unwrap_or(u32::MAX),
                _ => continue,
            };
            // PS3.3 §6.2: positive offsets ≤14h, negative ≤12h.
            let max_hour = if bytes[i] == b'+' { 14 } else { 12 };
            if hour > max_hour {
                return true;
            }
        }
    }
    false
}

// -- canonical formatting ---------------------------------------------------

/// Formats DA canonically: `YYYYMMDD`.
pub fn format_da(d: NaiveDate) -> String {
    d.format("%Y%m%d").to_string()
}

/// Formats TM canonically: `HHMMSS[.F…]` — fraction only when non-zero,
/// trailing zeros trimmed.
pub fn format_tm(t: NaiveTime) -> String {
    let mut s = format!("{:02}{:02}{:02}", t.hour(), t.minute(), t.second());
    // TM fractions are microseconds (PS3.5 §6.2); a sub-microsecond value has no
    // printable fraction and must not leave a bare trailing '.' (review CORE-19).
    let micros = t.nanosecond() / 1_000;
    if micros > 0 {
        let frac = format!("{micros:06}");
        s.push('.');
        s.push_str(frac.trim_end_matches('0'));
    }
    s
}

/// Formats DT canonically: `YYYYMMDDHHMMSS[.F…][±HHMM]`.
pub fn format_dt(p: DtPoint) -> String {
    let d = p.naive.date();
    let mut s = format!("{}{}", d.format("%Y%m%d"), format_tm(p.naive.time()));
    if let Some(o) = p.offset {
        let total = o.local_minus_utc();
        let sign = if total < 0 { '-' } else { '+' };
        let abs = total.abs();
        s.push(sign);
        s.push_str(&format!("{:02}{:02}", abs / 3600, (abs % 3600) / 60));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn da_all_input_shapes() {
        let y = parse_da("2024").unwrap();
        assert_eq!(y.min.to_string(), "2024-01-01");
        let ym = parse_da("202402").unwrap().min;
        assert_eq!(ym.to_string(), "2024-02-01");
        let ymd = parse_da("20240305").unwrap().min;
        assert_eq!(ymd.to_string(), "2024-03-05");
        assert_eq!(parse_da("2024.03.05").unwrap().min, ymd);
        assert_eq!(parse_da("2024/03/05").unwrap().min, ymd);
        assert_eq!(format_da(ymd), "20240305");
    }

    #[test]
    fn da_range_and_errors() {
        let r = parse_da("20240101-20241231").unwrap();
        assert_eq!(r.min.to_string(), "2024-01-01");
        assert_eq!(r.max.to_string(), "2024-12-31");
        assert!(r.contains(&parse_da("20240615").unwrap().min));

        assert!(parse_da("202413").is_err(), "month 13");
        assert!(parse_da("20240230").is_err(), "day 30 in February");
        assert!(parse_da("202").is_err());
        assert!(parse_da("abcd").is_err());
    }

    #[test]
    fn tm_shapes_and_round_trip() {
        assert_eq!(format_tm(parse_tm("103010").unwrap().min), "103010");
        assert_eq!(parse_tm("10").unwrap().min.to_string(), "10:00:00");
        assert_eq!(parse_tm("1030").unwrap().min.to_string(), "10:30:00");
        assert_eq!(
            parse_tm("10.30.15.5").unwrap().min.to_string(),
            "10:30:15.500"
        );
        assert_eq!(
            parse_tm("103015.123456").unwrap().min.nanosecond(),
            123_456_000
        );
        assert_eq!(
            parse_tm("10:30:15:123").unwrap().min.to_string(),
            "10:30:15.123"
        );
        assert_eq!(
            parse_tm("103015.1200").unwrap().min.nanosecond(),
            120_000_000
        );
        let r = parse_tm("080000-120000").unwrap();
        assert!(r.contains(&parse_tm("0900").unwrap().min));
        assert_eq!(format_tm(parse_tm("103015.5").unwrap().min), "103015.5");

        // a sub-microsecond fraction has no DICOM representation: no bare '.'
        let sub_micro = NaiveTime::from_hms_nano_opt(10, 30, 15, 999).unwrap();
        assert_eq!(format_tm(sub_micro), "103015");
        let one_micro = NaiveTime::from_hms_nano_opt(10, 30, 15, 1_000).unwrap();
        assert_eq!(format_tm(one_micro), "103015.000001");

        assert!(parse_tm("24").is_err(), "hour 24");
        assert!(parse_tm("1060").is_err(), "minute 60");
        assert!(parse_tm("1").is_err(), "odd digit count");
    }

    #[test]
    fn dt_shapes_offsets_and_round_trip() {
        let p = parse_dt("20240305103015").unwrap().min;
        assert_eq!(p.naive.to_string(), "2024-03-05 10:30:15");
        assert_eq!(p.offset, None);

        let with_off = parse_dt("20240305103015+0530").unwrap().min;
        assert_eq!(
            with_off.offset.map(|o| o.local_minus_utc()),
            Some(5 * 3600 + 30 * 60)
        );
        assert_eq!(format_dt(with_off), "20240305103015+0530");

        let colon_off = parse_dt("20240305103015-08:00").unwrap().min;
        assert_eq!(
            colon_off.offset.map(|o| o.local_minus_utc()),
            Some(-8 * 3600)
        );
        assert_eq!(format_dt(colon_off), "20240305103015-0800");

        let frac = parse_dt("20240305103015.123456+0100").unwrap().min;
        assert_eq!(frac.naive.nanosecond(), 123_456_000);
        assert_eq!(format_dt(frac), "20240305103015.123456+0100");

        assert_eq!(
            format_dt(parse_dt("202403051030").unwrap().min),
            "20240305103000",
            "canonical output is always 14 digits"
        );
        assert_eq!(format_dt(parse_dt("2024").unwrap().min), "20240101000000");

        let r = parse_dt("20240101-20241231").unwrap();
        assert_eq!(r.max.naive.to_string(), "2024-12-31 00:00:00");

        assert!(parse_dt("20240305T103015").is_err(), "non-digit separator");
        assert!(parse_dt("20240305103015+2500").is_err(), "offset hour > 14");
        assert!(
            parse_dt("20240305103015.1234567890").is_err(),
            "fraction too long"
        );

        // PS3.3 §6.2: a negative offset is limited to 12 hours (positive to 14).
        // A bare parse_dt() falls back to range-splitting on a bad suffix, so the
        // guard has_invalid_dt_offset() is what rejects the out-of-range offset.
        assert!(
            has_invalid_dt_offset("20240305103015-1300"),
            "negative offset hour > 12"
        );
        assert!(
            !has_invalid_dt_offset("20240305103015-1200"),
            "negative offset of 12h is allowed"
        );
        assert!(
            !has_invalid_dt_offset("20240305103015+1400"),
            "positive offset of 14h is allowed"
        );
        assert!(
            has_invalid_dt_offset("20240305103015+1500"),
            "positive offset hour > 14"
        );
    }

    #[test]
    fn non_ascii_offset_is_rejected_without_panicking() {
        // review #10: a multibyte char in the offset must not slice mid-char.
        assert!(parse_dt("20240101+0é0").is_err());
    }

    #[test]
    fn dt_to_fixed_keeps_the_wall_clock_time() {
        // review #11: 12:00+0800 is 12:00 local, not 20:00+08:00.
        let at = parse_dt("20261029120000+0800")
            .unwrap()
            .min
            .to_fixed()
            .unwrap();
        assert_eq!(at.to_rfc3339(), "2026-10-29T12:00:00+08:00");
    }
}

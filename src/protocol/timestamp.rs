//! Timestamps in responses (`botocore.utils.parse_timestamp`,
//! `awscli/customizations/timestampformat.py`) and requests
//! (`botocore/serialize.py` `_timestamp_*`).

use anyhow::{Result, anyhow, bail};
use chrono::Datelike;
use num_traits::ToPrimitive;

use crate::protocol::TimestampFormat;
use crate::protocol::common::parse_py_float;
use crate::value::Value;

/// A timestamp as it appears on the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum WireTimestamp {
    /// A JSON / CBOR number (epoch seconds).
    Number(Value),
    /// Text (XML, headers, JSON strings).
    Text(String),
}

/// Convert a wire timestamp to the CLI output value: with `Iso8601`,
/// `parse_timestamp(value).isoformat()`; with `Wire`, the value unchanged
/// (numbers stay numbers, text stays text).
///
/// # Panics
///
/// When the value cannot be parsed as a timestamp (botocore raises);
/// parsers use [`try_output_value`].
pub fn output_value(wire: WireTimestamp, format: TimestampFormat) -> Value {
    try_output_value(wire, format).expect("unparseable timestamp")
}

/// [`output_value`], returning the error botocore raises for an
/// unparseable timestamp.
pub fn try_output_value(wire: WireTimestamp, format: TimestampFormat) -> Result<Value> {
    match format {
        TimestampFormat::Wire => Ok(match wire {
            WireTimestamp::Number(v) => v,
            WireTimestamp::Text(s) => Value::Str(s),
        }),
        TimestampFormat::Iso8601 => Ok(Value::Str(
            parse_timestamp(&wire, &local_offset)?.isoformat(),
        )),
    }
}

/// Serialize an input timestamp value (a string or number from the command
/// line or JSON) in the given wire format (`iso8601`, `rfc822`,
/// `unixTimestamp`), the way botocore's serializers do after
/// `parse_to_aware_datetime`.
pub fn serialize(value: &Value, wire_format: &str) -> anyhow::Result<Value> {
    let (secs, micro) = parse_to_aware_utc(value)?;
    serialize_utc(secs, micro, wire_format)
}

/// `calendar.timegm(parse_to_aware_datetime(value).utctimetuple())`, as
/// `BaseRestSerializer._convert_header_value` computes it.
pub fn utc_epoch_seconds(value: &Value) -> Result<i64> {
    Ok(parse_to_aware_utc(value)?.0)
}

/// `botocore.utils.parse_to_aware_datetime` as UTC epoch seconds and
/// microseconds.
fn parse_to_aware_utc(value: &Value) -> Result<(i64, u32)> {
    let wire = match value {
        Value::Int(_) | Value::Float(_) => WireTimestamp::Number(value.clone()),
        Value::Str(s) => WireTimestamp::Text(s.clone()),
        other => bail!("Invalid timestamp \"{other:?}\""),
    };
    Ok(parse_timestamp(&wire, &local_offset)?.to_utc_epoch())
}

/// `Serializer._timestamp_iso8601` / `_timestamp_unixtimestamp` /
/// `_timestamp_rfc822` on an aware UTC datetime.
fn serialize_utc(secs: i64, micro: u32, wire_format: &str) -> Result<Value> {
    match wire_format.to_ascii_lowercase().as_str() {
        "iso8601" => {
            let dt = DateTime::from_epoch(secs, micro, Some(0));
            let base = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
            );
            Ok(Value::Str(if micro > 0 {
                format!("{base}.{micro:06}Z")
            } else {
                format!("{base}Z")
            }))
        }
        "unixtimestamp" => {
            if micro > 0 {
                let total = i128::from(secs) * 1_000_000 + i128::from(micro);
                Ok(Value::Float(total as f64 / 1e6))
            } else {
                Ok(Value::int(secs))
            }
        }
        "rfc822" => Ok(Value::Str(formatdate_gmt(secs))),
        other => bail!("'Serializer' object has no attribute '_timestamp_{other}'"),
    }
}

/// `email.utils.formatdate(timeval, usegmt=True)`.
fn formatdate_gmt(secs: i64) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let dt = DateTime::from_epoch(secs, 0, Some(0));
    let days = secs.div_euclid(86_400);
    // 1970-01-01 was a Thursday.
    let weekday = (days + 3).rem_euclid(7) as usize;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[weekday],
        dt.day,
        MONTHS[dt.month as usize - 1],
        dt.year,
        dt.hour,
        dt.minute,
        dt.second
    )
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A Python `datetime.datetime` (naive when `offset` is `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub microsecond: u32,
    /// `utcoffset()` in seconds.
    pub offset: Option<i32>,
}

impl DateTime {
    /// The wall-clock time `offset` seconds east of UTC for an epoch
    /// instant.
    fn from_epoch(secs: i64, micro: u32, offset: Option<i32>) -> DateTime {
        let local = secs + i64::from(offset.unwrap_or(0));
        let days = local.div_euclid(86_400);
        let rem = local.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        DateTime {
            year,
            month,
            day,
            hour: (rem / 3600) as u32,
            minute: (rem % 3600 / 60) as u32,
            second: (rem % 60) as u32,
            microsecond: micro,
            offset,
        }
    }

    /// Epoch seconds and microseconds, treating a naive value as UTC
    /// (`parse_to_aware_datetime`).
    fn to_utc_epoch(self) -> (i64, u32) {
        let days = days_from_civil(self.year, self.month, self.day);
        let local = days * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second);
        (
            local - i64::from(self.offset.unwrap_or(0)),
            self.microsecond,
        )
    }

    /// `datetime.isoformat()`.
    pub fn isoformat(&self) -> String {
        let mut s = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        if self.microsecond != 0 {
            s.push_str(&format!(".{:06}", self.microsecond));
        }
        if let Some(off) = self.offset {
            let sign = if off < 0 { '-' } else { '+' };
            let a = off.unsigned_abs();
            s.push_str(&format!("{sign}{:02}:{:02}", a / 3600, a % 3600 / 60));
            if a % 60 != 0 {
                s.push_str(&format!(":{:02}", a % 60));
            }
        }
        s
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// The offset of the local time zone (`dateutil.tz.tzlocal`) at an epoch
/// instant, in seconds east of UTC. Unit tests run as if `TZ=UTC`, the
/// zone the goldens are generated in.
#[cfg(test)]
fn local_offset(_secs: i64) -> i32 {
    0
}

/// The offset of the local time zone (`dateutil.tz.tzlocal`) at an epoch
/// instant, in seconds east of UTC.
#[cfg(not(test))]
fn local_offset(secs: i64) -> i32 {
    use chrono::{Offset, TimeZone};
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(utc) => chrono::Local
            .offset_from_utc_datetime(&utc.naive_utc())
            .fix()
            .local_minus_utc(),
        None => 0,
    }
}

/// `botocore.utils.parse_timestamp`: numbers and numeric strings are
/// epoch seconds shown in the local time zone
/// (`datetime.fromtimestamp(value, tzlocal())`); other strings go through
/// `dateutil.parser.parse(value, tzinfos={'GMT': tzutc()})`.
fn parse_timestamp(wire: &WireTimestamp, offset_at: &dyn Fn(i64) -> i32) -> Result<DateTime> {
    let number = match wire {
        WireTimestamp::Number(Value::Int(i)) => {
            Some(Epoch::Int(i.to_i64().ok_or_else(|| {
                anyhow!("Unable to calculate correct timezone offset for \"{i}\"")
            })?))
        }
        WireTimestamp::Number(Value::Float(f)) => Some(Epoch::Float(*f)),
        WireTimestamp::Number(other) => bail!("Invalid timestamp \"{other:?}\""),
        WireTimestamp::Text(s) => parse_py_float(s).map(Epoch::Float),
    };
    match (number, wire) {
        (Some(Epoch::Float(f)), WireTimestamp::Text(s)) if f.is_nan() => dateutil_parse(s),
        (Some(epoch), _) => {
            let (secs, micro) = epoch.split().ok_or_else(|| {
                anyhow!(
                    "Unable to calculate correct timezone offset for \"{}\"",
                    wire_text(wire)
                )
            })?;
            let dt = DateTime::from_epoch(secs, micro, Some(offset_at(secs)));
            if !(1..=9999).contains(&dt.year) {
                bail!(
                    "Unable to calculate correct timezone offset for \"{}\"",
                    wire_text(wire)
                );
            }
            Ok(dt)
        }
        (None, WireTimestamp::Text(s)) => dateutil_parse(s),
        (None, WireTimestamp::Number(_)) => unreachable!(),
    }
}

fn wire_text(wire: &WireTimestamp) -> String {
    match wire {
        WireTimestamp::Text(s) => s.clone(),
        WireTimestamp::Number(Value::Int(i)) => i.to_string(),
        WireTimestamp::Number(Value::Float(f)) => crate::protocol::common::py_float_repr(*f),
        WireTimestamp::Number(v) => format!("{v:?}"),
    }
}

enum Epoch {
    Int(i64),
    Float(f64),
}

impl Epoch {
    /// `_PyTime_ObjectToTimeval(..., _PyTime_ROUND_HALF_EVEN)`.
    fn split(&self) -> Option<(i64, u32)> {
        match *self {
            Epoch::Int(i) => Some((i, 0)),
            Epoch::Float(f) => {
                if !f.is_finite() {
                    return None;
                }
                let mut int_part = f.trunc();
                let mut frac = ((f - int_part) * 1e6).round_ties_even();
                if frac >= 1e6 {
                    frac -= 1e6;
                    int_part += 1.0;
                } else if frac < 0.0 {
                    frac += 1e6;
                    int_part -= 1.0;
                }
                if int_part.abs() > 1e15 {
                    return None;
                }
                Some((int_part as i64, frac as u32))
            }
        }
    }
}

/// The subset of `dateutil.parser.parse` that AWS timestamps use: ISO 8601
/// dates and date-times (fractional seconds truncated to microseconds,
/// `Z` / `UTC` / `GMT` / `±HH[:MM]` offsets) and RFC 822 / 1123 dates.
/// A value without a zone stays naive.
fn dateutil_parse(s: &str) -> Result<DateTime> {
    let fail = || anyhow!("Invalid timestamp \"{s}\": Unknown string format: {s}");
    let t = s.trim();
    let b = t.as_bytes();
    let parsed = if b.first().is_some_and(u8::is_ascii_digit) && b.get(4) == Some(&b'-') {
        parse_iso(t)
    } else if b.len() > 8 && b[..8].iter().all(u8::is_ascii_digit) && matches!(b[8], b'T' | b't') {
        parse_iso_basic(t)
    } else {
        parse_rfc822(t)
    };
    let dt = parsed.ok_or_else(fail)?;
    if dt.month == 0 || dt.month > 12 {
        bail!("Invalid timestamp \"{s}\": month must be in 1..12: {s}");
    }
    let last = days_in_month(dt.year, dt.month);
    if dt.day == 0 || dt.day > last {
        bail!(
            "Invalid timestamp \"{s}\": day {} must be in range 1..{last} for month {} in year {}: {s}",
            dt.day,
            dt.month,
            dt.year
        );
    }
    if dt.hour > 23 || dt.minute > 59 || dt.second > 59 {
        return Err(fail());
    }
    Ok(dt)
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn digits(&mut self, min: usize, max: usize) -> Option<(u32, usize)> {
        let start = self.i;
        while self.i < self.s.len() && self.i - start < max && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let n = self.i - start;
        if n < min {
            return None;
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        Some((text.parse().ok()?, n))
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn skip_spaces(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn rest(&self) -> &[u8] {
        &self.s[self.i..]
    }

    fn done(&self) -> bool {
        self.i >= self.s.len()
    }
}

/// `dateutil.parser._parser._parsems`: the fraction is truncated to six
/// digits.
fn fraction(c: &mut Cursor<'_>) -> Option<u32> {
    let start = c.i;
    while c.s.get(c.i).is_some_and(u8::is_ascii_digit) {
        c.i += 1;
    }
    if c.i == start {
        return None;
    }
    let digits = std::str::from_utf8(&c.s[start..c.i]).ok()?;
    let six: String = format!("{digits:0<6}").chars().take(6).collect();
    six.parse().ok()
}

/// A zone suffix: `Z`, `UTC`, `GMT`, `±HH`, `±HHMM`, `±HH:MM`.
fn zone(c: &mut Cursor<'_>) -> Option<Option<i32>> {
    c.skip_spaces();
    if c.done() {
        return Some(None);
    }
    let rest = c.rest();
    for name in [&b"UTC"[..], b"GMT", b"Z", b"z"] {
        if rest.eq_ignore_ascii_case(name) && (name.len() > 1 || rest == name) {
            c.i = c.s.len();
            return Some(Some(0));
        }
    }
    let sign = match rest.first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    c.i += 1;
    let (hh, n) = c.digits(2, 4)?;
    let (h, m) = if n == 4 {
        (hh / 100, hh % 100)
    } else if n == 2 {
        if c.eat(b':') {
            let (m, _) = c.digits(2, 2)?;
            (hh, m)
        } else {
            (hh, 0)
        }
    } else {
        return None;
    };
    c.skip_spaces();
    if !c.done() {
        return None;
    }
    Some(Some(sign * (h as i32 * 3600 + m as i32 * 60)))
}

fn parse_iso(t: &str) -> Option<DateTime> {
    let mut c = Cursor {
        s: t.as_bytes(),
        i: 0,
    };
    let (year, _) = c.digits(4, 4)?;
    if !c.eat(b'-') {
        return None;
    }
    let (month, _) = c.digits(2, 2)?;
    if !c.eat(b'-') {
        return None;
    }
    let (day, _) = c.digits(2, 2)?;
    let mut dt = DateTime {
        year: i64::from(year),
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
        microsecond: 0,
        offset: None,
    };
    if c.done() {
        return Some(dt);
    }
    if !(c.eat(b'T') || c.eat(b't') || c.eat(b' ')) {
        return None;
    }
    c.skip_spaces();
    dt.hour = c.digits(2, 2)?.0;
    if !c.eat(b':') {
        return None;
    }
    dt.minute = c.digits(2, 2)?.0;
    if c.eat(b':') {
        dt.second = c.digits(2, 2)?.0;
        if c.eat(b'.') || c.eat(b',') {
            dt.microsecond = fraction(&mut c)?;
        }
    }
    dt.offset = zone(&mut c)?;
    Some(dt)
}

/// The ISO 8601 basic format (`20231114T221320Z`).
fn parse_iso_basic(t: &str) -> Option<DateTime> {
    let mut c = Cursor {
        s: t.as_bytes(),
        i: 0,
    };
    let (year, _) = c.digits(4, 4)?;
    let (month, _) = c.digits(2, 2)?;
    let (day, _) = c.digits(2, 2)?;
    let mut dt = DateTime {
        year: i64::from(year),
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
        microsecond: 0,
        offset: None,
    };
    if !(c.eat(b'T') || c.eat(b't')) {
        return None;
    }
    dt.hour = c.digits(2, 2)?.0;
    dt.minute = c.digits(2, 2)?.0;
    if let Some((second, _)) = c.digits(2, 2) {
        dt.second = second;
        if c.eat(b'.') || c.eat(b',') {
            dt.microsecond = fraction(&mut c)?;
        }
    }
    dt.offset = zone(&mut c)?;
    Some(dt)
}

fn parse_rfc822(t: &str) -> Option<DateTime> {
    const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let mut words = t.split_ascii_whitespace().peekable();
    let first = words.peek()?.trim_end_matches(',').to_ascii_lowercase();
    if WEEKDAYS.iter().any(|w| first.starts_with(w))
        && !first.starts_with(|c: char| c.is_ascii_digit())
    {
        words.next();
    }
    let rest: Vec<&str> = words.collect();
    let (day, month, year, time, tz) = match rest.as_slice() {
        [d, m, y, time, tz @ ..] => (*d, *m, *y, *time, tz.join(" ")),
        _ => return None,
    };
    let day: u32 = day.parse().ok()?;
    let ml = month.to_ascii_lowercase();
    let month = MONTHS
        .iter()
        .position(|m| ml.starts_with(&m.to_ascii_lowercase()))? as u32
        + 1;
    if !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut year: i64 = year.parse().ok()?;
    if year.to_string().len() <= 2 && year < 100 {
        year = convert_two_digit_year(year);
    }
    let mut c = Cursor {
        s: time.as_bytes(),
        i: 0,
    };
    let hour = c.digits(1, 2)?.0;
    if !c.eat(b':') {
        return None;
    }
    let minute = c.digits(2, 2)?.0;
    let mut second = 0;
    let mut microsecond = 0;
    if c.eat(b':') {
        second = c.digits(2, 2)?.0;
        if c.eat(b'.') {
            microsecond = fraction(&mut c)?;
        }
    }
    if !c.done() {
        return None;
    }
    let mut zc = Cursor {
        s: tz.as_bytes(),
        i: 0,
    };
    let offset = zone(&mut zc)?;
    Some(DateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        microsecond,
        offset,
    })
}

/// `dateutil.parser.parserinfo.convertyear` for two-digit years.
fn convert_two_digit_year(year: i64) -> i64 {
    let now = i64::from(chrono::Utc::now().year());
    let century = now / 100 * 100;
    let mut y = year + century;
    if y >= now + 50 {
        y -= 100;
    } else if y < now - 50 {
        y += 100;
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(_: i64) -> i32 {
        0
    }

    fn iso_utc(wire: WireTimestamp) -> String {
        parse_timestamp(&wire, &utc).unwrap().isoformat()
    }

    /// Goldens: `parse_timestamp(value).isoformat()` with `TZ=UTC`.
    #[test]
    fn output_matches_botocore() {
        let cases: &[(WireTimestamp, &str)] = &[
            (
                WireTimestamp::Number(Value::int(1700000000)),
                "2023-11-14T22:13:20+00:00",
            ),
            (
                WireTimestamp::Number(Value::Float(1700000000.5)),
                "2023-11-14T22:13:20.500000+00:00",
            ),
            (
                WireTimestamp::Number(Value::Float(1700000000.1234567)),
                "2023-11-14T22:13:20.123457+00:00",
            ),
            (
                WireTimestamp::Number(Value::int(0)),
                "1970-01-01T00:00:00+00:00",
            ),
            (
                WireTimestamp::Number(Value::Float(-1.5)),
                "1969-12-31T23:59:58.500000+00:00",
            ),
            (
                WireTimestamp::Text("1700000000".into()),
                "2023-11-14T22:13:20+00:00",
            ),
            (
                WireTimestamp::Text("1.7e9".into()),
                "2023-11-14T22:13:20+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00:00Z".into()),
                "2023-01-01T00:00:00+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00:00.123Z".into()),
                "2023-01-01T00:00:00.123000+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00:00.1234567Z".into()),
                "2023-01-01T00:00:00.123456+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00:00.000Z".into()),
                "2023-01-01T00:00:00+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T09:30:00+09:00".into()),
                "2023-01-01T09:30:00+09:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T09:30:00-0730".into()),
                "2023-01-01T09:30:00-07:30",
            ),
            (
                WireTimestamp::Text("2023-01-01T09:30:00+00:00".into()),
                "2023-01-01T09:30:00+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00:00".into()),
                "2023-01-01T00:00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01 12:34:56".into()),
                "2023-01-01T12:34:56",
            ),
            (
                WireTimestamp::Text("2023-01-01".into()),
                "2023-01-01T00:00:00",
            ),
            (
                WireTimestamp::Text("Tue, 29 Apr 2014 18:30:38 GMT".into()),
                "2014-04-29T18:30:38+00:00",
            ),
            (
                WireTimestamp::Text("Wed, 1 Jan 2025 01:02:03 +0900".into()),
                "2025-01-01T01:02:03+09:00",
            ),
            (
                WireTimestamp::Text("29 Apr 2014 18:30:38 UTC".into()),
                "2014-04-29T18:30:38+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00:00.000+0000".into()),
                "2023-01-01T00:00:00+00:00",
            ),
            (
                WireTimestamp::Text(" 1_700_000_000 ".into()),
                "2023-11-14T22:13:20+00:00",
            ),
            (
                WireTimestamp::Text("1700000000.0000005".into()),
                "2023-11-14T22:13:20+00:00",
            ),
            (
                WireTimestamp::Text("1700000000.0000015".into()),
                "2023-11-14T22:13:20.000001+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00:00,5Z".into()),
                "2023-01-01T00:00:00.500000+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00Z".into()),
                "2023-01-01T00:00:00+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01t00:00:00z".into()),
                "2023-01-01T00:00:00+00:00",
            ),
            (
                WireTimestamp::Text("Tue, 29 Apr 14 18:30:38 GMT".into()),
                "2014-04-29T18:30:38+00:00",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00:00 +0530".into()),
                "2023-01-01T00:00:00+05:30",
            ),
            (
                WireTimestamp::Text("2023-01-01T00:00:00-03".into()),
                "2023-01-01T00:00:00-03:00",
            ),
        ];
        for (wire, want) in cases {
            assert_eq!(iso_utc(wire.clone()), *want, "{wire:?}");
        }
    }

    #[test]
    fn wire_format_is_identity() {
        let v = try_output_value(
            WireTimestamp::Number(Value::Float(1.5)),
            TimestampFormat::Wire,
        )
        .unwrap();
        assert_eq!(v, Value::Float(1.5));
        let v = try_output_value(WireTimestamp::Text("x".into()), TimestampFormat::Wire).unwrap();
        assert_eq!(v, Value::str("x"));
    }

    #[test]
    fn invalid_timestamps_fail() {
        let cases = [
            (
                "garbage",
                "Invalid timestamp \"garbage\": Unknown string format: garbage",
            ),
            (
                "2023-02-30T00:00:00Z",
                "Invalid timestamp \"2023-02-30T00:00:00Z\": day 30 must be in range 1..28 for month 2 in year 2023: 2023-02-30T00:00:00Z",
            ),
            (
                "nan",
                "Invalid timestamp \"nan\": Unknown string format: nan",
            ),
            (
                "inf",
                "Unable to calculate correct timezone offset for \"inf\"",
            ),
        ];
        for (s, msg) in cases {
            let err = parse_timestamp(&WireTimestamp::Text(s.into()), &utc).unwrap_err();
            assert_eq!(err.to_string(), msg);
        }
    }

    /// Goldens: `Serializer._convert_timestamp_to_str(value, fmt)`.
    #[test]
    fn serialize_matches_botocore() {
        let cases: &[(Value, &str, Value)] = &[
            (
                Value::str("2023-01-01T00:00:00Z"),
                "iso8601",
                Value::str("2023-01-01T00:00:00Z"),
            ),
            (
                Value::str("2023-01-01T00:00:00.5Z"),
                "iso8601",
                Value::str("2023-01-01T00:00:00.500000Z"),
            ),
            (
                Value::str("2023-01-01T09:00:00+09:00"),
                "iso8601",
                Value::str("2023-01-01T00:00:00Z"),
            ),
            (
                Value::str("2023-01-01"),
                "unixTimestamp",
                Value::int(1672531200),
            ),
            (
                Value::str("2023-01-01T00:00:00.003691Z"),
                "unixtimestamp",
                Value::Float(1672531200.003691),
            ),
            (
                Value::int(1700000000),
                "rfc822",
                Value::str("Tue, 14 Nov 2023 22:13:20 GMT"),
            ),
            (
                Value::Float(1700000000.25),
                "unixtimestamp",
                Value::Float(1700000000.25),
            ),
            (
                Value::str("1700000000"),
                "iso8601",
                Value::str("2023-11-14T22:13:20Z"),
            ),
            (
                Value::str("Tue, 29 Apr 2014 18:30:38 GMT"),
                "rfc822",
                Value::str("Tue, 29 Apr 2014 18:30:38 GMT"),
            ),
            (
                Value::str("2023-01-01T00:00:00.9Z"),
                "rfc822",
                Value::str("Sun, 01 Jan 2023 00:00:00 GMT"),
            ),
        ];
        for (input, fmt, want) in cases {
            assert_eq!(&serialize(input, fmt).unwrap(), want, "{input:?} {fmt}");
        }
    }
}

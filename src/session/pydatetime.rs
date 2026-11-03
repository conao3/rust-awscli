//! The datetime handling of the credential code: `dateutil.parser.parse`
//! for the timestamp formats the caches and providers hold, and
//! `datetime.isoformat()` / `strftime` of the result.

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Utc};

/// A parsed datetime: the wall-clock value and its UTC offset (`None` for
/// a naive datetime).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PyDateTime {
    pub naive: NaiveDateTime,
    pub offset: Option<FixedOffset>,
}

impl PyDateTime {
    pub fn from_utc(dt: DateTime<Utc>) -> PyDateTime {
        PyDateTime {
            naive: dt.naive_utc(),
            offset: Some(FixedOffset::east_opt(0).expect("offset")),
        }
    }

    /// `dateutil.parser.parse` for ISO 8601 style timestamps
    /// (`YYYY-MM-DD[T ]HH:MM[:SS[.ffffff]]` with `Z`, `UTC`, `GMT`, or a
    /// numeric offset).
    pub fn parse(s: &str) -> Option<PyDateTime> {
        let s = s.trim();
        let (date_part, rest) = match s.find(['T', 't', ' ']) {
            Some(i) => (&s[..i], s[i + 1..].trim_start()),
            None => (s, ""),
        };
        let date = NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
            .or_else(|_| NaiveDate::parse_from_str(date_part, "%Y%m%d"))
            .ok()?;
        if rest.is_empty() {
            return Some(PyDateTime {
                naive: date.and_hms_opt(0, 0, 0)?,
                offset: None,
            });
        }
        let tz_start = rest
            .char_indices()
            .find(|(_, c)| matches!(c, 'Z' | 'z' | '+' | '-') || c.is_ascii_alphabetic())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let (time_part, tz_part) = (rest[..tz_start].trim(), rest[tz_start..].trim());
        let time = parse_time(time_part)?;
        let offset = match tz_part {
            "" => None,
            "Z" | "z" | "UTC" | "GMT" | "utc" | "gmt" => Some(FixedOffset::east_opt(0)?),
            other => Some(parse_offset(other)?),
        };
        Some(PyDateTime {
            naive: date.and_time(time),
            offset,
        })
    }

    /// The instant (naive values are taken as local time, as comparisons
    /// with `tzlocal()` datetimes do).
    pub fn to_utc(&self) -> DateTime<Utc> {
        match self.offset {
            Some(o) => (self.naive - Duration::seconds(i64::from(o.local_minus_utc()))).and_utc(),
            None => self
                .naive
                .and_local_timezone(chrono::Local)
                .earliest()
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|| self.naive.and_utc()),
        }
    }

    /// Seconds from now until this time (`total_seconds(t - now)`).
    pub fn seconds_remaining(&self) -> f64 {
        (self.to_utc() - Utc::now()).num_milliseconds() as f64 / 1000.0
    }

    /// `datetime.isoformat()`.
    pub fn isoformat(&self) -> String {
        let mut out = self.naive.format("%Y-%m-%dT%H:%M:%S").to_string();
        let micros = self.naive.and_utc().timestamp_subsec_micros();
        if micros != 0 {
            out.push_str(&format!(".{micros:06}"));
        }
        if let Some(o) = self.offset {
            out.push_str(&format_offset(o.local_minus_utc()));
        }
        out
    }
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    let (hms, frac) = match s.split_once(['.', ',']) {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let parts: Vec<&str> = if hms.contains(':') {
        hms.split(':').collect()
    } else {
        hms.as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect()
    };
    let num = |i: usize| -> Option<u32> { parts.get(i).map_or(Some(0), |p| p.parse::<u32>().ok()) };
    let micros = match frac {
        None => 0,
        Some(f) => {
            let digits: String = f.chars().take(6).collect();
            let n: u32 = digits.parse().ok()?;
            n * 10u32.pow(6 - digits.len() as u32)
        }
    };
    NaiveTime::from_hms_micro_opt(num(0)?, num(1)?, num(2)?, micros)
}

fn parse_offset(s: &str) -> Option<FixedOffset> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    let (h, m) = match digits.len() {
        2 => (digits.parse::<i32>().ok()?, 0),
        4 => (
            digits[..2].parse::<i32>().ok()?,
            digits[2..].parse::<i32>().ok()?,
        ),
        _ => return None,
    };
    FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

fn format_offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let secs = secs.abs();
    let mut out = format!("{sign}{:02}:{:02}", secs / 3600, (secs % 3600) / 60);
    if secs % 60 != 0 {
        out.push_str(&format!(":{:02}", secs % 60));
    }
    out
}

/// `datetime.now(tzutc()) + timedelta(seconds=n)` formatted with
/// `'%Y-%m-%dT%H:%M:%SZ'`.
pub fn utc_after(seconds: i64) -> String {
    (Utc::now() + Duration::seconds(seconds))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// `datetime.fromtimestamp(ts, tzutc())` formatted with
/// `'%Y-%m-%dT%H:%M:%SZ'`.
pub fn utc_from_timestamp(ts: f64) -> String {
    let secs = ts.floor() as i64;
    DateTime::<Utc>::from_timestamp(secs, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format() {
        let iso = |s: &str| PyDateTime::parse(s).unwrap().isoformat();
        assert_eq!(iso("2030-01-01T00:00:00Z"), "2030-01-01T00:00:00+00:00");
        assert_eq!(iso("2030-01-01T00:00:00UTC"), "2030-01-01T00:00:00+00:00");
        assert_eq!(
            iso("2030-01-01T01:02:03.5+09:00"),
            "2030-01-01T01:02:03.500000+09:00"
        );
        assert_eq!(iso("2030-01-01T00:00:00"), "2030-01-01T00:00:00");
        assert_eq!(iso("2030-01-01 00:00:00-0130"), "2030-01-01T00:00:00-01:30");
        assert!(
            PyDateTime::parse("2030-01-01T00:00:00Z")
                .unwrap()
                .seconds_remaining()
                > 0.0
        );
    }
}

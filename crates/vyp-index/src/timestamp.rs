//! RFC 3339 timestamps and durations, as Unix seconds.
//!
//! Index upload times (PEP 700) are RFC 3339 strings such as
//! `2024-01-15T09:30:12.345678Z`; cutoffs may also be given as an ISO 8601
//! duration (`P7D`, `PT12H`) or in words (`7 days`, `2 weeks`).

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS[.frac]]` with `Z` or `±HH:MM`.
/// A bare date means midnight UTC; a missing offset means UTC.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = match s.find(['T', 't', ' ']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, m, d) * 86_400;
    let Some(time) = time else { return Some(secs) };

    let (clock, offset) = if let Some(t) = time.strip_suffix(['Z', 'z']) {
        (t, 0)
    } else if let Some(i) = time.rfind(['+', '-']) {
        let (clock, off) = time.split_at(i);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off[1..].split_once(':').unwrap_or((&off[1..], "0"));
        (clock, sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60))
    } else {
        (time, 0)
    };
    let mut fields = clock.split(':');
    let h: i64 = fields.next()?.parse().ok()?;
    let mi: i64 = fields.next()?.parse().ok()?;
    let sec: i64 = match fields.next() {
        Some(sf) => sf.split('.').next()?.parse().ok()?,
        None => 0,
    };
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    secs += h * 3600 + mi * 60 + sec - offset;
    Some(secs)
}

pub fn format_rfc3339(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Parse `P7D`, `PT12H`, `P1W`, `7 days`, `2 weeks`, `24 hours`, `30 minutes`.
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some(iso) = s.strip_prefix(['P', 'p']) {
        let mut total = 0i64;
        let mut num = String::new();
        let mut in_time = false;
        for c in iso.chars() {
            match c {
                'T' | 't' => in_time = true,
                '0'..='9' => num.push(c),
                _ => {
                    let n: i64 = num.parse().ok()?;
                    num.clear();
                    total += n * match (c.to_ascii_uppercase(), in_time) {
                        ('W', false) => 7 * 86_400,
                        ('D', false) => 86_400,
                        ('H', true) => 3600,
                        ('M', true) => 60,
                        ('S', true) => 1,
                        _ => return None,
                    };
                }
            }
        }
        return num.is_empty().then_some(total);
    }
    let mut words = s.split_whitespace();
    let n: i64 = words.next()?.parse().ok()?;
    let unit = words.next()?.trim_end_matches('s').to_ascii_lowercase();
    if words.next().is_some() {
        return None;
    }
    let per = match unit.as_str() {
        "week" => 7 * 86_400,
        "day" => 86_400,
        "hour" => 3600,
        "minute" => 60,
        "second" => 1,
        _ => return None,
    };
    Some(n * per)
}

/// A cutoff as Unix seconds: an absolute timestamp, or "now minus" a duration.
pub fn parse_cutoff(s: &str) -> Option<i64> {
    parse_rfc3339(s).or_else(|| parse_duration(s).map(|d| now() - d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upload_times() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2024-01-15T09:30:12.345678Z"), Some(1_705_311_012));
        assert_eq!(parse_rfc3339("2024-01-15T10:30:12+01:00"), Some(1_705_311_012));
        assert_eq!(parse_rfc3339("2024-01-15"), Some(1_705_276_800));
        assert_eq!(parse_rfc3339("2024-13-01"), None);
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn formats_round_trip() {
        for secs in [0, 1_705_311_012, 951_782_400 /* 2000-02-29 */] {
            assert_eq!(parse_rfc3339(&format_rfc3339(secs)), Some(secs));
        }
        assert_eq!(format_rfc3339(1_705_311_012), "2024-01-15T09:30:12Z");
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("P7D"), Some(7 * 86_400));
        assert_eq!(parse_duration("PT12H"), Some(12 * 3600));
        assert_eq!(parse_duration("P1DT2H"), Some(86_400 + 7200));
        assert_eq!(parse_duration("2 weeks"), Some(14 * 86_400));
        assert_eq!(parse_duration("1 day"), Some(86_400));
        assert_eq!(parse_duration("P7"), None);
        assert_eq!(parse_duration("7 fortnights"), None);
    }
}

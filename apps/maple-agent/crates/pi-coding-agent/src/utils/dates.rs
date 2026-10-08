//! Date conversion at the session-format boundary. ISO timestamps use UTC;
//! legacy unzoned forms use the host's civil-time rules, like Node's Date parser.
//! This covers the recorded V8 grammar, not every implementation-defined form.
use pi_ai::types::{JsString, JsValue};

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += i64::from(m <= 2);
    (y, m, d)
}
fn days_from_civil(mut y: i64, m: i64, d: i64) -> i64 {
    y -= i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}
pub fn iso_timestamp(ms: i64) -> JsString {
    let (y, m, d) = civil_from_days(ms.div_euclid(86_400_000));
    let time = ms.rem_euclid(86_400_000);
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else {
        format!("{y:+07}")
    };
    format!(
        "{year}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3_600_000,
        time / 60_000 % 60,
        time / 1000 % 60,
        time % 1000
    )
    .into()
}
fn time_clip(value: f64) -> f64 {
    if value.is_finite() && value.abs() <= 8.64e15 {
        value.trunc()
    } else {
        f64::NAN
    }
}
fn decimal(value: &str) -> Option<i64> {
    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}
#[derive(Default)]
struct DateParts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    fraction: i64,
    offset: Option<i64>,
}
impl DateParts {
    fn timestamp(&self) -> Option<f64> {
        if !(-300_000..=300_000).contains(&self.year)
            || !(1..=12).contains(&self.month)
            || !(1..=31).contains(&self.day)
            || !(0..=24).contains(&self.hour)
            || !(0..=59).contains(&self.minute)
            || !(0..=59).contains(&self.second)
            || (self.hour == 24 && (self.minute != 0 || self.second != 0 || self.fraction != 0))
        {
            return None;
        }
        let utc = days_from_civil(self.year, self.month, self.day) as f64 * 86_400_000.0
            + (self.hour * 3_600_000 + self.minute * 60_000 + self.second * 1000 + self.fraction)
                as f64;
        let result = if let Some(offset) = self.offset {
            utc - offset as f64 * 60_000.0
        } else {
            local_timestamp(self)?
        };
        Some(time_clip(result))
    }
}
#[cfg(unix)]
fn local_timestamp(parts: &DateParts) -> Option<f64> {
    // Date parsing uses local civil-time rules but does not sample the clock.
    let mut civil: libc::tm = unsafe { std::mem::zeroed() };
    civil.tm_year = i32::try_from(parts.year - 1900).ok()?;
    civil.tm_mon = i32::try_from(parts.month - 1).ok()?;
    civil.tm_mday = i32::try_from(parts.day).ok()?;
    civil.tm_hour = i32::try_from(parts.hour).ok()?;
    civil.tm_min = i32::try_from(parts.minute).ok()?;
    civil.tm_sec = i32::try_from(parts.second).ok()?;
    civil.tm_isdst = -1;
    let seconds = unsafe { libc::mktime(&mut civil) };
    Some(seconds as f64 * 1000.0 + parts.fraction as f64)
}
#[cfg(not(unix))]
fn local_timestamp(parts: &DateParts) -> Option<f64> {
    // Unix hosts are the currently validated source/replay targets. UTC remains
    // deterministic elsewhere until a Windows civil-time adapter is recorded.
    Some(
        days_from_civil(parts.year, parts.month, parts.day) as f64 * 86_400_000.0
            + (parts.hour * 3_600_000
                + parts.minute * 60_000
                + parts.second * 1000
                + parts.fraction) as f64,
    )
}
fn zone_offset(zone: &str, strict: bool) -> Option<i64> {
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = &zone[1..];
    let (hours, minutes) = if let Some((h, m)) = digits.split_once(':') {
        if strict && (h.len() != 2 || m.len() != 2) {
            return None;
        }
        (decimal(h)?, decimal(m)?)
    } else if digits.len() == 4 {
        (decimal(&digits[..2])?, decimal(&digits[2..])?)
    } else if !strict && (1..=2).contains(&digits.len()) {
        (decimal(digits)?, 0)
    } else {
        return None;
    };
    (hours <= 23 && minutes <= 59).then_some(sign * (hours * 60 + minutes))
}
fn parse_time(value: &str, parts: &mut DateParts, strict: bool) -> Option<()> {
    let mut pieces = value.split(':');
    let hour = pieces.next()?;
    let minute = pieces.next()?;
    if strict && (hour.len() != 2 || minute.len() != 2) {
        return None;
    }
    parts.hour = decimal(hour)?;
    parts.minute = decimal(minute)?;
    if let Some(seconds) = pieces.next() {
        let (seconds, fraction) = seconds
            .split_once('.')
            .map_or((seconds, None), |(s, f)| (s, Some(f)));
        if strict && seconds.len() != 2 {
            return None;
        }
        parts.second = decimal(seconds)?;
        if let Some(fraction) = fraction {
            if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            parts.fraction = decimal(&format!("{}00", &fraction[..fraction.len().min(3)])[..3])?;
        }
    }
    if pieces.next().is_some() {
        return None;
    }
    Some(())
}
fn parse_iso(text: &str) -> Option<f64> {
    let year_len = if text.starts_with(['+', '-']) { 7 } else { 4 };
    let year_text = text.get(..year_len)?;
    let (sign, digits) = if year_len == 7 {
        (if text.starts_with('-') { -1 } else { 1 }, &year_text[1..])
    } else {
        (1, year_text)
    };
    if year_text == "-000000" {
        return None;
    }
    let mut parts = DateParts {
        year: sign * decimal(digits)?,
        month: 1,
        day: 1,
        offset: Some(0),
        ..Default::default()
    };
    let mut rest = text.get(year_len..)?;
    if rest.is_empty() {
        return parts.timestamp();
    }
    if !rest.starts_with('-') {
        return None;
    }
    parts.month = decimal(rest.get(1..3)?)?;
    rest = rest.get(3..)?;
    if rest.is_empty() {
        return parts.timestamp();
    }
    if !rest.starts_with('-') {
        return None;
    }
    parts.day = decimal(rest.get(1..3)?)?;
    rest = rest.get(3..)?;
    if rest.is_empty() {
        return parts.timestamp();
    }
    if !rest.starts_with(['T', 't']) {
        return None;
    }
    rest = &rest[1..];
    parts.offset = None;
    if rest.ends_with(['Z', 'z']) {
        parts.offset = Some(0);
        rest = &rest[..rest.len() - 1];
    } else if let Some(index) = rest.find(['+', '-']) {
        parts.offset = Some(zone_offset(&rest[index..], true)?);
        rest = &rest[..index];
    }
    parse_time(rest, &mut parts, true)?;
    parts.timestamp()
}
fn parse_legacy(text: &str) -> Option<f64> {
    let mut cleaned = String::new();
    let mut comment_depth = 0;
    for c in text.chars() {
        match c {
            '(' => comment_depth += 1,
            ')' if comment_depth > 0 => comment_depth -= 1,
            _ if comment_depth == 0 => cleaned.push(c.to_ascii_lowercase()),
            _ => {}
        }
    }
    // In V8's legacy fallback a leading negative-zero expanded year is
    // consumed as the sign/zero prefix, leaving a month/day form.
    if let Some(rest) = cleaned.strip_prefix("-000000-") {
        cleaned = rest.to_owned();
    }
    let mut parts = DateParts {
        month: 1,
        day: 1,
        ..Default::default()
    };
    let mut numbers = vec![];
    let mut named_month = None;
    let mut meridiem = None;
    let mut have_time = false;
    for token in cleaned
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
    {
        if token == "am" || token == "pm" {
            if meridiem.replace(token).is_some() {
                return None;
            }
            continue;
        }
        let months = [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ];
        if let Some(index) = months.iter().position(|month| token.starts_with(month)) {
            if named_month.replace(index as i64 + 1).is_some() {
                return None;
            }
            continue;
        }
        if ["sun", "mon", "tue", "wed", "thu", "fri", "sat"]
            .iter()
            .any(|day| token.starts_with(day))
        {
            continue;
        }
        let named_zone = match token {
            "z" | "ut" | "utc" | "gmt" => Some(0),
            "est" => Some(-300),
            "edt" => Some(-240),
            "cst" => Some(-360),
            "cdt" => Some(-300),
            "mst" => Some(-420),
            "mdt" => Some(-360),
            "pst" => Some(-480),
            "pdt" => Some(-420),
            _ => None,
        };
        if let Some(offset) = named_zone {
            parts.offset = Some(offset);
            continue;
        }
        if let Some(zone) = token
            .strip_prefix("gmt")
            .or_else(|| token.strip_prefix("utc"))
        {
            parts.offset = Some(zone_offset(zone, false)?);
            continue;
        }
        if token.starts_with(['+', '-']) && have_time {
            parts.offset = Some(zone_offset(token, false)?);
            continue;
        }
        if token.contains(':') {
            if have_time {
                return None;
            }
            have_time = true;
            let mut time = token;
            if let Some(value) = time.strip_suffix("am").or_else(|| time.strip_suffix("pm")) {
                meridiem = Some(&time[time.len() - 2..]);
                time = value;
            }
            if let Some(index) = time.find(['+', '-']) {
                parts.offset = Some(zone_offset(&time[index..], false)?);
                time = &time[..index];
            }
            if let Some(value) = time.strip_suffix('z') {
                parts.offset = Some(0);
                time = value;
            }
            parse_time(time, &mut parts, false)?;
            continue;
        }
        for number in token.trim_start_matches(['+', '-']).split(['-', '/', '.']) {
            numbers.push(decimal(number)?);
        }
    }
    if let Some(month) = named_month {
        parts.month = month;
        match numbers.as_slice() {
            [day] => {
                parts.year = 2001;
                parts.day = *day;
            }
            [first, second] if *first > 31 => {
                parts.year = *first;
                parts.day = *second;
            }
            [day, year] => {
                parts.day = *day;
                parts.year = *year;
            }
            _ => return None,
        }
    } else {
        match numbers.as_slice() {
            [0] => parts.year = 2000,
            [month @ 1..=12] => {
                parts.year = 2001;
                parts.month = *month;
            }
            [13..=31] => return None,
            [year] => parts.year = *year,
            [year, month] if *year > 31 => {
                parts.year = *year;
                parts.month = *month;
            }
            [month, day] => {
                parts.year = 2001;
                parts.month = *month;
                parts.day = *day;
            }
            [year, month, day] if !(1..=31).contains(year) => {
                parts.year = *year;
                parts.month = *month;
                parts.day = *day;
            }
            [month, day, year] => {
                parts.year = *year;
                parts.month = *month;
                parts.day = *day;
            }
            _ => return None,
        }
    }
    if (0..=49).contains(&parts.year) {
        parts.year += 2000;
    } else if (50..=99).contains(&parts.year) {
        parts.year += 1900;
    }
    if let Some(meridiem) = meridiem {
        if !(1..=12).contains(&parts.hour) {
            return None;
        }
        parts.hour = parts.hour % 12 + if meridiem == "pm" { 12 } else { 0 };
    }
    parts.timestamp()
}
pub fn parse_timestamp(value: &JsString) -> f64 {
    let Some(text) = value.as_str() else {
        return f64::NAN;
    };
    if let Some(result) = parse_iso(text) {
        return result;
    }
    let trimmed = text.trim_matches(crate::utils::paths::js_whitespace);
    // Once the ISO time separator has been recognized, V8 does not retry legacy
    // parsing (including when whitespace surrounds an otherwise valid ISO time).
    let date_length = if trimmed.starts_with(['+', '-']) {
        13
    } else {
        10
    };
    if trimmed
        .as_bytes()
        .get(date_length)
        .is_some_and(|b| matches!(b, b'T' | b't'))
    {
        return f64::NAN;
    }
    parse_legacy(trimmed).unwrap_or(f64::NAN)
}
/// `new Date(value).getTime()` used by raw session-header `created` values.
pub fn timestamp_from_value(value: Option<&JsValue>) -> f64 {
    match value {
        None => f64::NAN,
        Some(JsValue::Null) => 0.0,
        Some(JsValue::Bool(value)) => f64::from(u8::from(*value)),
        Some(JsValue::Number(value)) => time_clip(*value),
        Some(JsValue::String(value)) => parse_timestamp(value),
        Some(JsValue::Array(values)) => parse_timestamp(&pi_ai::utils::raw_message::join_values(
            values.iter().map(Some),
            ",",
        )),
        Some(JsValue::Object(_)) => f64::NAN,
    }
}

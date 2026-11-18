//! Pure product logic for the `get_current_time` tool.
//! Host timezone discovery, clock access and resolver ownership belong to callers.

use pilotdeck_protocol::{JsObject, JsValue};
use pilotdeck_transport::system_clock::{ZonedDateTimeParts, format_epoch_iso};

use crate::js_util::{object, schema, string};

pub const TIME_TOOL_NAME: &str = "get_current_time";

const WEEKDAYS: &[&str] = &[
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

pub fn definition() -> JsObject {
    object([
        ("name", string(TIME_TOOL_NAME)),
        ("title", string("Get Current Time")),
        (
            "description",
            string(
                "Return the current local time for a timezone. Use this before creating cron_create tasks with absolute dates/times such as tonight, tomorrow morning, or next Monday. For simple relative delays like 'in 10 minutes', prefer cron_create with schedule.type='delay' instead of calling this tool.",
            ),
        ),
        ("kind", string("custom")),
        (
            "inputSchema",
            JsValue::Object(schema(
                r#"{"type":"object","additionalProperties":false,"properties":{"timezone":{"type":"string","description":"IANA timezone, e.g. Asia/Shanghai. Defaults to the host local timezone."}}}"#,
            )),
        ),
    ])
}

/// Select the requested zone, reading the trusted host fallback only when needed.
pub fn timezone(input: &JsValue, host_timezone: impl FnOnce() -> String) -> String {
    input
        .get("timezone")
        .and_then(JsValue::as_string)
        .map(|value| String::from_utf16_lossy(value.units()))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(host_timezone)
}

/// Product projections, to be mapped into the caller's tool output type.
pub struct TimeOutput {
    pub content: Vec<JsValue>,
    pub data: JsValue,
}

/// Format an instant using the caller's trusted zone resolver.
pub fn execute(
    timezone: String,
    now: f64,
    resolve: impl FnOnce(&str, i64) -> Result<ZonedDateTimeParts, ()>,
) -> Result<TimeOutput, String> {
    if !now.is_finite() || now.abs() > 8.64e15 {
        return Err("Invalid time value".into());
    }
    let epoch_ms = now.trunc() as i64;
    let parts =
        resolve(&timezone, epoch_ms).map_err(|_| format!("Invalid timezone: {timezone}"))?;
    let iso = format_epoch_iso(epoch_ms);
    let local = format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}{}",
        format_year(parts.year),
        parts.month,
        parts.day,
        parts.hour,
        parts.minute,
        parts.second,
        format_offset(parts.offset_seconds)
    );
    let date = format!(
        "{}-{:02}-{:02}",
        format_year(parts.year),
        parts.month,
        parts.day
    );
    let weekday = WEEKDAYS[parts.weekday as usize];
    let output = object([
        ("timezone", string(timezone)),
        ("iso", string(iso)),
        ("local", string(local)),
        ("date", string(date)),
        ("weekday", string(weekday)),
        ("unixMs", JsValue::Number(epoch_ms as f64)),
    ]);
    Ok(TimeOutput {
        content: vec![JsValue::Object(object([
            ("type", string("json")),
            ("value", JsValue::Object(output.clone())),
        ]))],
        data: JsValue::Object(output),
    })
}

fn format_year(year: i64) -> String {
    if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:04}", -year)
    } else {
        format!("{year:04}")
    }
}

fn format_offset(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let seconds = seconds.unsigned_abs();
    format!("{sign}{:02}:{:02}", seconds / 3_600, seconds % 3_600 / 60)
}

/// Days since 1970-01-01 (Howard Hinnant), proleptic Gregorian;
/// **1-based** month. Shared with the Retry-After date parser.
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let y = year - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    era * 146_097 + y * 365 + y / 4 - y / 100 + doy - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn parts() -> ZonedDateTimeParts {
        ZonedDateTimeParts {
            year: -1,
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
            weekday: 6,
            offset_seconds: -19_800,
        }
    }

    #[test]
    fn timezone_selection_only_reads_fallback_when_needed() {
        let calls = Cell::new(0);
        let input = JsValue::Object(object([("timezone", string("  Asia/Tokyo  "))]));
        assert_eq!(
            timezone(&input, || {
                calls.set(calls.get() + 1);
                "UTC".into()
            }),
            "Asia/Tokyo"
        );
        assert_eq!(calls.get(), 0);
        let input = JsValue::Object(object([("timezone", string("  "))]));
        assert_eq!(
            timezone(&input, || {
                calls.set(calls.get() + 1);
                "UTC".into()
            }),
            "UTC"
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn output_keeps_truncation_extended_year_and_dual_projection() {
        let output = execute("zone".into(), -1.9, |zone, epoch| {
            assert_eq!((zone, epoch), ("zone", -1));
            Ok(parts())
        })
        .unwrap();
        let data = output.data.as_object().unwrap();
        assert_eq!(data.get("iso"), Some(&string("1969-12-31T23:59:59.999Z")));
        assert_eq!(
            data.get("local"),
            Some(&string("-0001-01-02T03:04:05-05:30"))
        );
        assert_eq!(data.get("unixMs"), Some(&JsValue::Number(-1.0)));
        assert_eq!(output.content[0].get("value"), Some(&output.data));
    }

    #[test]
    fn invalid_instant_does_not_call_resolver_and_invalid_zone_is_reported() {
        for now in [f64::NAN, f64::INFINITY, 8.64e15 + 1.0] {
            assert_eq!(
                execute("zone".into(), now, |_, _| panic!(
                    "invalid instant reached resolver"
                ))
                .err()
                .as_deref(),
                Some("Invalid time value")
            );
        }
        assert_eq!(
            execute("bad".into(), 0.0, |_, _| Err(())).err().as_deref(),
            Some("Invalid timezone: bad")
        );
    }
}

//! Time-zone names resolve like the fixed TS baseline, from bundled data only.
//!
//! `fixtures/tz-names/oracle.json` is recorded by `record.mjs` from the
//! baseline's own `get_current_time` tool, `isValidCronTimezone` and
//! `computeNextCronRunAt` under Node v22.22.0 (ICU 78.2, tzdb 2025c). Node
//! answers these from its bundled ICU data, never from the host's zoneinfo
//! tree; the Rust port answers from the bundled tzdb in
//! `pilotdeck_transport::system_clock::named_zone`. Nothing here reads
//! `/usr/share/zoneinfo`, so the result does not depend on whether the host
//! has `tzdata-legacy` (Ubuntu 24.04 ships `US/Pacific`, `GB`, `PRC`, … only
//! in that package) or any tzdata at all.
use std::sync::Arc;

use pilotdeck_gateway::{
    host::modules::tool_context::{HostToolCheckpoint, ToolRuntimeContext},
    runtime::{
        cron::{CronZoneResolverCache, compute_next_cron_run_at, is_valid_cron_timezone},
        tools::{ExecutableTool, ToolRunContext, builtin::time::GetCurrentTimeTool},
    },
};
use pilotdeck_protocol::{JsObject, JsValue};
use pilotdeck_transport::system_clock::{ZonedPartsResolver, civil_from_days, named_zone};
use serde_json::Value;

fn oracle() -> Value {
    serde_json::from_str(include_str!("fixtures/tz-names/oracle.json")).expect("tz-names oracle")
}

fn context(now: f64) -> ToolRunContext {
    ToolRunContext::new(Arc::new(ToolRuntimeContext {
        fields: JsObject::new(),
        files: HostToolCheckpoint::default().tool_context_state(),
        abort_signal: None,
        model: None,
        elicitation: None,
        user_dialog: None,
        audit_recorder: None,
        file_history: None,
        file_update_notifier: None,
        now: Some(Arc::new(move || JsValue::Number(now))),
        progress: None,
        subagent: None,
        plan_todo: None,
        goal: None,
        plan_directory: None,
    }))
}

fn to_json(value: &JsValue) -> Value {
    let text = pilotdeck_protocol::stringify_json(value).unwrap().unwrap();
    serde_json::from_str(&text).expect("tool output is JSON")
}

/// Every hand-picked name — canonical zones, `backward` links, ICU-only IDs,
/// case variants, padded input, UTC-offset identifiers and names ICU rejects
/// — gives the baseline's tool output (echoed name, local time, offset,
/// weekday) or error, cron validity and next cron run.
#[test]
fn named_zones_match_the_baseline_tool_and_cron() {
    let oracle = oracle();
    let now = oracle["now"].as_array().unwrap();
    let cron = oracle["cron"].as_str().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let tool = GetCurrentTimeTool::with_zone_cache(Arc::new(CronZoneResolverCache::new()));
    let cache = CronZoneResolverCache::new();
    let mut failures = Vec::new();
    for case in oracle["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        for (index, expected) in case["tool"].as_array().unwrap().iter().enumerate() {
            let input = pilotdeck_protocol::parse_json(
                &serde_json::json!({ "timezone": name }).to_string(),
            )
            .unwrap();
            let at = now[index].as_f64().unwrap();
            let actual = match runtime.block_on(tool.execute(input, context(at))) {
                Ok(output) => serde_json::json!({ "data": to_json(output.data.as_ref().unwrap()) }),
                Err(error) => serde_json::json!({ "error": {
                    "code": error.code,
                    "message": error.message.to_utf8().unwrap(),
                }}),
            };
            if &actual != expected {
                failures.push(format!("{name:?} tool@{at}: {actual} != {expected}"));
            }
        }
        let valid = is_valid_cron_timezone(name);
        if Some(valid) != case["cronValid"].as_bool() {
            failures.push(format!("{name:?} cron validity: {valid}"));
        }
        let next = compute_next_cron_run_at(&cron.into(), now[1].as_f64().unwrap(), name, &cache)
            .expect("in-range scan");
        if next != case["cronNext"].as_f64() {
            failures.push(format!(
                "{name:?} next cron run: {next:?} != {}",
                case["cronNext"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The accepted name set is exactly ICU's: every name the baseline accepts
/// (in its recorded spelling, lower case and upper case) is valid, every
/// name it rejects is not. The candidates are every bundled tzdb name, the
/// ICU-only IDs, and names ICU rejects, so nothing Rust can accept is left
/// out of the comparison.
#[test]
fn accepted_zone_names_are_exactly_icus() {
    let oracle = oracle();
    let accepted = oracle["accepted"].as_object().unwrap();
    assert_eq!(accepted.len(), 637, "accepted set must not silently shrink");
    let mut failures = Vec::new();
    for name in accepted.keys() {
        for spelling in [name.clone(), name.to_lowercase(), name.to_uppercase()] {
            if !is_valid_cron_timezone(&spelling) || !named_zone::is_named_zone(&spelling) {
                failures.push(format!("{spelling:?} must be accepted"));
            }
        }
    }
    for name in oracle["rejected"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        if is_valid_cron_timezone(name) || ZonedPartsResolver::new(name).is_ok() {
            failures.push(format!("{name:?} must be rejected"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every accepted name has the UTC offset of its ICU zone at each recorded
/// sample (1900–2099, four seasons a year). One known data-version gap is
/// skipped: the bundled tzdb is 2025b, and 2025c (ICU 78.2) corrected
/// America/Tijuana's 1953–1975 history; ICU 77.1 (tzdb 2025b) matches the
/// bundled data there too.
#[test]
fn zone_offsets_match_icu_samples() {
    let oracle = oracle();
    let samples: Vec<i64> = oracle["samples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|sample| sample.as_i64().unwrap())
        .collect();
    let tijuana_2025c_window = 1953..=1975;
    let mut failures = Vec::new();
    for (name, zone) in oracle["accepted"].as_object().unwrap() {
        let zone = zone.as_str().unwrap();
        let expected = oracle["offsets"][zone].as_array().unwrap();
        let resolver = ZonedPartsResolver::new(name).expect("accepted name resolves");
        for (sample, offset) in samples.iter().zip(expected) {
            let parts = resolver.parts_at(*sample).unwrap();
            let (utc_year, ..) = civil_from_days(sample.div_euclid(86_400_000));
            if zone == "America/Tijuana" && tijuana_2025c_window.contains(&utc_year) {
                continue;
            }
            if Some(i64::from(parts.offset_seconds)) != offset.as_i64() {
                failures.push(format!(
                    "{name} ({zone}) @ {sample}: {} != {offset}",
                    parts.offset_seconds
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The bundled data is the tzdb release the comments and the offset gap
/// above describe; a crate bump must revisit both.
#[test]
fn bundled_tzdb_release_is_pinned() {
    assert_eq!(named_zone::TZDB_VERSION, Some("2025b"));
}

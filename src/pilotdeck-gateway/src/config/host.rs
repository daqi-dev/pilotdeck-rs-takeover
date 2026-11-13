//! Intrinsic prototype state belongs to the JS host, shared with its other RPCs.
//! A config that can inspect or freeze those intrinsics must remain TS-owned.
use super::value::*;
use pilotdeck_protocol::JsValue;
fn selection(v: &JsValue) -> bool {
    let Some(s) = v.as_string() else { return false };
    let s = trim(s);
    let Some(i) = s.units().iter().position(|u| *u == 47) else {
        return false;
    };
    builtin_property(&pilotdeck_protocol::JsString::from_utf16(
        s.units()[..i].to_vec(),
    )) || builtin_property(&pilotdeck_protocol::JsString::from_utf16(
        s.units()[i + 1..].to_vec(),
    ))
}
pub(super) fn required(raw: &JsValue) -> bool {
    // Own provider/model keys and the ordinary agent selection are handled
    // by model::parse and sections::agent, including inherited catalog errors.
    // Keep the host boundary for other selection sites until separately proven.
    let agent = field(raw, "agent");
    if selection(field(field(agent, "subagents"), "default"))
        || selection(field(field(raw, "memory"), "model"))
    {
        return true;
    }
    let router = field(raw, "router");
    if selection(field(field(router, "scenarios"), "default"))
        || selection(field(field(router, "tokenSaver"), "judge"))
    {
        return true;
    }
    for (_, v) in entries(field(router, "fallback")) {
        if let JsValue::Array(a) = v
            && a.iter().any(selection)
        {
            return true;
        }
    }
    if entries(field(field(router, "tokenSaver"), "tiers"))
        .iter()
        .any(|(_, t)| selection(field(t, "model")))
    {
        return true;
    }
    let stats = field(router, "stats");
    if selection(field(stats, "baselineModel"))
        || entries(field(stats, "modelPricing"))
            .iter()
            .any(|(key, _)| selection(&JsValue::String((*key).clone())))
    {
        return true;
    }
    let baseline = field(stats, "baselineModel");
    if field(baseline, "provider")
        .as_string()
        .is_some_and(builtin_property)
        || field(baseline, "model")
            .as_string()
            .is_some_and(builtin_property)
    {
        return true;
    }
    false
}

//! SDK `sandbox: { type: "tool_policy", ... }` capability eligibility.
//! This is session data policy, independent of the existing OS sandbox mode.
//! Fixed TS ProjectSessionRuntimeBundle.ts:807-825. Evaluate after every
//! registry composition/update so later MCP/custom/subagent tools cannot
//! regain a denied capability. This predicate never grants another policy.
use pilotdeck_protocol::{JsString, JsValue};

/// `sandbox` is the restriction object, not the whole SDK config. Syntax is
/// validated by sdk_config before it reaches the session. Host-bridge tests
/// are exact kind/name checks from TS, including the literal `task_` prefix;
/// do not infer capability from descriptions, aliases or similar tool names.
pub(super) fn tool_allowed(
    sandbox: Option<&JsValue>,
    name: &JsString,
    kind: Option<&JsString>,
) -> bool {
    let Some(sandbox) = sandbox else {
        return true;
    };
    let field = |key: &str, expected: &str| {
        sandbox
            .get(key)
            .and_then(JsValue::as_string)
            .is_some_and(|value| value.equals_str(expected))
    };
    let kind_is = |expected: &str| kind.is_some_and(|kind| kind.equals_str(expected));
    let host_bridge = kind_is("shell")
        || kind_is("mcp")
        || kind_is("custom")
        || name.equals_str("execute_code")
        || name.equals_str("agent")
        || name.units().starts_with(&[
            b't' as u16,
            b'a' as u16,
            b's' as u16,
            b'k' as u16,
            b'_' as u16,
        ]);
    let deny_process = field("process", "deny") && host_bridge;
    let deny_filesystem = if field("filesystem", "deny") {
        kind_is("filesystem") || host_bridge
    } else {
        field("filesystem", "read_only")
            && (name.equals_str("write_file")
                || name.equals_str("edit_file")
                || name.equals_str("edit_notebook")
                || host_bridge)
    };
    let deny_network = field("network", "deny") && (kind_is("network") || host_bridge);
    !(deny_process || deny_filesystem || deny_network)
}

#[cfg(test)]
#[path = "sdk_sandbox_tests.rs"]
mod tests;

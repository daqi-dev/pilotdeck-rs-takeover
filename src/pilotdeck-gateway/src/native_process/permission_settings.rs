//! `~/.pilotdeck/permissions.json` (baseline `permission/settings.ts`), read
//! at the start of every turn (`InProcessGateway.ts:1441-1457`).
//!
//! The file holds only user-sourced rules. Each turn they replace the
//! `source: "user"` entries of the session's permission rules
//! (`applyAgentPermissionOverrides` → `replaceUserRules`,
//! agent/turn/permissionOverrides.ts:20-31); policy, session grants and
//! remembered approvals (`source: "policy" | "session"`) stay in front of
//! them. Nothing in the gateway writes the file back: a remembered approval
//! lives only in the session's rule set. An unreadable or malformed file is
//! the empty default, and an edit takes effect on the next turn.
use crate::config::permission::{PermissionRule, PermissionRuleSet};
use pilotdeck_protocol::{JsString, JsValue};
use std::path::Path;

/// `getPermissionSettingsPath()`: `<pilot home>/permissions.json`.
pub(super) fn settings_path(pilot_home: &Path) -> std::path::PathBuf {
    pilot_home.join("permissions.json")
}

/// `permissionSettingsToRuleSet(readPermissionSettings())`.
pub(super) fn read_user_rules(pilot_home: &Path) -> PermissionRuleSet {
    let Ok(raw) = std::fs::read_to_string(settings_path(pilot_home)) else {
        return PermissionRuleSet::default();
    };
    parse_user_rules(&raw)
}

fn parse_user_rules(raw: &str) -> PermissionRuleSet {
    let Ok(parsed) = pilotdeck_protocol::parse_json(raw) else {
        return PermissionRuleSet::default();
    };
    let Some(record) = parsed.as_object() else {
        return PermissionRuleSet::default();
    };
    let list = |key: &str, behavior: &str| -> Vec<PermissionRule> {
        let Some(JsValue::Array(items)) = record.get(key) else {
            return Vec::new();
        };
        // `normalizeStringArray`: strings only, normalized, empty entries
        // and repeats of an already-normalized entry dropped.
        let mut seen: Vec<(JsString, Option<JsString>)> = Vec::new();
        let mut rules = Vec::new();
        for item in items.iter().filter_map(JsValue::as_string) {
            if item.units().iter().all(|unit| is_js_space(*unit)) {
                continue;
            }
            let mut rule = crate::runtime::interaction::coordinator::entry_to_rule(item);
            let key = (rule.tool_name.clone(), rule.pattern.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            rule.source = "user".into();
            rule.behavior = behavior.into();
            rules.push(rule);
        }
        rules
    };
    PermissionRuleSet {
        allow: list("allowedTools", "allow"),
        deny: list("disallowedTools", "deny"),
        ask: list("askTools", "ask"),
    }
}

/// `String.prototype.trim`'s whitespace set (WhiteSpace + LineTerminator).
fn is_js_space(unit: u16) -> bool {
    matches!(
        unit,
        0x09 | 0x0a | 0x0b | 0x0c | 0x0d | 0x20 | 0xa0 | 0x1680 | 0x2000
            ..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff
    )
}

/// `replaceUserRules` on each list: keep the non-user rules in place and
/// append the incoming user rules.
pub(super) fn replace_user_rules(target: &mut PermissionRuleSet, user: &PermissionRuleSet) {
    for (rules, incoming) in [
        (&mut target.allow, &user.allow),
        (&mut target.deny, &user.deny),
        (&mut target.ask, &user.ask),
    ] {
        rules.retain(|rule| !rule.source.equals_str("user"));
        rules.extend(
            incoming
                .iter()
                .filter(|rule| rule.source.equals_str("user"))
                .cloned(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(rules: &[PermissionRule]) -> Vec<(String, Option<String>)> {
        rules
            .iter()
            .map(|rule| {
                (
                    String::from_utf16_lossy(rule.tool_name.units()),
                    rule.pattern
                        .as_ref()
                        .map(|pattern| String::from_utf16_lossy(pattern.units())),
                )
            })
            .collect()
    }

    #[test]
    fn normalizes_claude_style_entries_and_drops_noise() {
        let rules = parse_user_rules(
            r#"{"allowedTools":["Bash(touch allowed*)","Read","  ",42,"Read","Bash()"],"disallowedTools":["Bash(rm *)","Write"],"askTools":"x"}"#,
        );
        assert_eq!(
            shape(&rules.allow),
            vec![
                ("bash".into(), Some("touch allowed*".into())),
                ("read_file".into(), None),
                ("bash".into(), None),
            ]
        );
        assert_eq!(
            shape(&rules.deny),
            vec![
                ("bash".into(), Some("rm *".into())),
                ("write_file".into(), None),
            ]
        );
        assert!(rules.ask.is_empty());
        assert!(
            rules
                .allow
                .iter()
                .all(|rule| rule.source.equals_str("user") && rule.behavior.equals_str("allow"))
        );
        assert!(
            rules
                .deny
                .iter()
                .all(|rule| rule.behavior.equals_str("deny"))
        );
    }

    #[test]
    fn a_malformed_file_is_the_empty_default() {
        for raw in ["", "nope", "[]", "null", "{\"allowedTools\":{}}"] {
            let rules = parse_user_rules(raw);
            assert!(rules.allow.is_empty() && rules.deny.is_empty() && rules.ask.is_empty());
        }
    }

    #[test]
    fn user_rules_are_replaced_behind_the_others() {
        let mut target = PermissionRuleSet::default();
        target
            .allow
            .push(PermissionRule::new("session", "allow", "bash"));
        target
            .allow
            .push(PermissionRule::new("user", "allow", "old"));
        target
            .deny
            .push(PermissionRule::new("policy", "deny", "bash"));
        let user = parse_user_rules(r#"{"allowedTools":["Glob"],"disallowedTools":["Write"]}"#);
        replace_user_rules(&mut target, &user);
        assert_eq!(
            shape(&target.allow),
            vec![("bash".into(), None), ("glob".into(), None)]
        );
        assert_eq!(
            shape(&target.deny),
            vec![("bash".into(), None), ("write_file".into(), None)]
        );
    }
}

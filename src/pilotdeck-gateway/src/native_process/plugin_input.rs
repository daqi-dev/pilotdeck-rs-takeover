//! Fixed-baseline InputProcessor's data-plugin slash expansion. TurnInputProcessor
//! consumes only messages; its diagnostics are intentionally not new wire events.
use crate::runtime::{
    plugins::{PluginCommandContribution, PluginSessionContributionSnapshot},
    turn::{live::InputTransform, runner::AgentInput},
};
use pilotdeck_protocol::JsString;
use std::sync::Arc;

pub(super) fn transformer(snapshot: &PluginSessionContributionSnapshot) -> InputTransform {
    let commands = snapshot.commands.clone();
    Arc::new(move |input| process(input, &commands))
}
fn append(out: &mut Vec<u16>, text: &str) {
    out.extend(text.encode_utf16());
}
fn has(haystack: &[u16], needle: &[u16]) -> bool {
    haystack.windows(needle.len()).any(|part| part == needle)
}
fn bounded(value: &[u16], limit: usize) -> Vec<u16> {
    if value.len() <= limit {
        return value.to_vec();
    }
    let marker: Vec<u16> = "\n[truncated by PilotDeck command admission]"
        .encode_utf16()
        .collect();
    let mut result = value[..limit.saturating_sub(marker.len())].to_vec();
    result.extend(marker);
    result
}
fn section(name: &str, value: &[u16], forbidden: &[u16]) -> Vec<u16> {
    let mut suffix = 0;
    let delimiter = loop {
        let delimiter = format!(
            "<<<PILOTDECK_{}_{suffix}>>>",
            name.replace('-', "_").to_ascii_uppercase()
        );
        if !has(forbidden, &delimiter.encode_utf16().collect::<Vec<_>>()) {
            break delimiter;
        }
        suffix += 1;
    };
    let mut result = Vec::new();
    append(&mut result, &format!("<{name}>\n{delimiter}\n"));
    result.extend(value);
    append(&mut result, &format!("\n{delimiter}\n</{name}>"));
    result
}
fn process(input: AgentInput, commands: &[PluginCommandContribution]) -> AgentInput {
    let AgentInput::Text(text) = &input else {
        return input;
    };
    let units = text.units();
    if units.first() != Some(&47) {
        return input;
    }
    let valid = |unit: &u16| matches!(*unit, 48..=57|65..=90|97..=122|95|58|45);
    let end = 1 + units[1..].iter().take_while(|unit| valid(unit)).count();
    if end == 1
        || (end < units.len() && !crate::channel::sms::trim_units(&units[end..end + 1]).is_empty())
    {
        return input;
    }
    let name = String::from_utf16_lossy(&units[1..end]);
    let Some(command) = commands.iter().find(|command| command.name == name) else {
        return input;
    };
    let mut argument_start = end;
    while argument_start < units.len()
        && crate::channel::sms::trim_units(&units[argument_start..argument_start + 1]).is_empty()
    {
        argument_start += 1;
    }
    let argument = &units[argument_start..];
    let content: Vec<u16> = command.content.encode_utf16().collect();
    let body = crate::channel::sms::trim_units(&content);
    let mut output = Vec::new();
    if body.is_empty() {
        append(&mut output, &format!("Run plugin command \"/{name}\""));
        if argument.is_empty() {
            append(&mut output, ".");
        } else {
            append(&mut output, " with argument: ");
            output.extend(argument);
        }
    } else {
        let body = bounded(body, 32_000);
        let argument = bounded(argument, 8_000);
        let mut forbidden = body.clone();
        forbidden.push(10);
        forbidden.extend(&argument);
        append(&mut output, &format!("<plugin-command name=\"/{name}\">\n"));
        output.extend(section("command-body", &body, &forbidden));
        output.push(10);
        if !argument.is_empty() {
            output.push(10);
            output.extend(section("command-argument", &argument, &forbidden));
        }
        append(&mut output, "\n</plugin-command>");
    }
    AgentInput::Text(JsString::from_utf16(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn commands(body: &str) -> Vec<PluginCommandContribution> {
        vec![PluginCommandContribution {
            name: "local:greet".into(),
            description: None,
            argument_hint: None,
            content: body.into(),
            namespace: "local".into(),
        }]
    }
    fn text(input: AgentInput) -> JsString {
        if let AgentInput::Text(value) = input {
            value
        } else {
            panic!("text expected")
        }
    }
    #[test]
    fn exact_bounded_sections_and_repeated_expansion() {
        let commands = commands(" body ");
        let first = text(process(
            AgentInput::Text("/local:greet arg".into()),
            &commands,
        ));
        assert_eq!(
            first,
            JsString::from(
                "<plugin-command name=\"/local:greet\">\n<command-body>\n<<<PILOTDECK_COMMAND_BODY_0>>>\nbody\n<<<PILOTDECK_COMMAND_BODY_0>>>\n</command-body>\n\n<command-argument>\n<<<PILOTDECK_COMMAND_ARGUMENT_0>>>\narg\n<<<PILOTDECK_COMMAND_ARGUMENT_0>>>\n</command-argument>\n</plugin-command>"
            )
        );
        assert_eq!(
            text(process(AgentInput::Text(first.clone()), &commands)),
            first
        );
    }
    #[test]
    fn unknown_slash_blocks_and_leading_space_are_untouched() {
        for raw in ["/unknown arg", " /local:greet arg", "/local:greet! arg"] {
            assert_eq!(
                text(process(AgentInput::Text(raw.into()), &commands("body"))),
                JsString::from(raw)
            );
        }
        let blocks = vec![pilotdeck_protocol::JsValue::String(
            "/local:greet arg".into(),
        )];
        assert!(
            matches!(process(AgentInput::Blocks(blocks.clone()),&commands("body")),AgentInput::Blocks(actual) if actual==blocks)
        );
    }
    #[test]
    fn delimiters_cannot_be_injected_and_utf16_is_bounded() {
        let body = "<<<PILOTDECK_COMMAND_BODY_0>>>".to_owned() + &"x".repeat(33_000);
        let mut arg: Vec<u16> = "/local:greet ".encode_utf16().collect();
        arg.extend(std::iter::repeat_n(0xd800, 9_000));
        let result = text(process(
            AgentInput::Text(JsString::from_utf16(arg)),
            &commands(&body),
        ));
        let rendered = String::from_utf16_lossy(result.units());
        assert!(rendered.contains("<<<PILOTDECK_COMMAND_BODY_1>>>"));
        assert_eq!(
            rendered
                .matches("[truncated by PilotDeck command admission]")
                .count(),
            2
        );
        assert!(result.units().contains(&0xd800));
        assert!(result.units().len() < 41_000);
    }
    #[test]
    fn command_sets_are_session_scoped() {
        let input = AgentInput::Text("/local:greet".into());
        assert_eq!(
            text(process(input.clone(), &[])),
            JsString::from("/local:greet")
        );
        assert!(
            text(process(input, &commands("session one")))
                .to_utf8()
                .unwrap()
                .contains("session one")
        );
    }
    #[test]
    fn fixed_node22_input_processor_oracle_and_utf16_tail_match() {
        use pilotdeck_protocol::{JsValue, parse_json};
        use sha2::{Digest, Sha256};
        let fixture = parse_json(include_str!(
            "../../tests/fixtures/sdk-session-plugins/input-oracle.json"
        ))
        .unwrap();
        let JsValue::Array(cases) = fixture.get("cases").unwrap() else {
            panic!("oracle cases")
        };
        for case in cases {
            let raw = case.get("input").unwrap();
            if raw.get("type") == Some(&JsValue::String("text".into())) {
                let input = raw.get("text").unwrap().as_string().unwrap().clone();
                let actual = text(process(
                    AgentInput::Text(input),
                    &commands(
                        &case
                            .get("body")
                            .unwrap()
                            .as_string()
                            .unwrap()
                            .to_utf8()
                            .unwrap(),
                    ),
                ));
                let JsValue::Array(messages) = case.get("result").unwrap().get("messages").unwrap()
                else {
                    panic!("oracle messages")
                };
                let JsValue::Array(content) = messages[0].get("content").unwrap() else {
                    panic!("oracle content")
                };
                assert_eq!(
                    &actual,
                    content[0].get("text").unwrap().as_string().unwrap()
                );
            }
        }
        let mut input: Vec<u16> = "/local:greet ".encode_utf16().collect();
        input.extend(std::iter::repeat_n(0xd800, 9000));
        let actual = text(process(
            AgentInput::Text(JsString::from_utf16(input)),
            &commands(&"x".repeat(33000)),
        ));
        let bytes: Vec<u8> = actual
            .units()
            .iter()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let hash = format!("{:x}", Sha256::digest(bytes));
        assert_eq!(
            Some(hash.as_str()),
            fixture
                .get("bounded")
                .unwrap()
                .get("sha256Utf16Le")
                .unwrap()
                .as_string()
                .unwrap()
                .to_utf8()
                .ok()
                .as_deref()
        );
        assert_eq!(actual.units().len(), 40259);
    }
}

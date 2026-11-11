use super::*;
use pilotdeck_protocol::parse_json;

fn descriptor(json: &str) -> JsValue {
    parse_json(json).expect("descriptor json")
}

const CONTINUABLE: &str = r#"{"version":2,"mode":"continuable","provider":"pilotdeck-native","definitionId":"explore","parentSessionId":"s","label":"Look","agentProvider":"p","agentModel":"m"}"#;

#[test]
fn continuable_descriptor_parses_and_one_shot_is_not_continuable() {
    let (continuable, fields) = parse_descriptor(&descriptor(CONTINUABLE))
        .expect("valid")
        .expect("known version");
    assert!(continuable);
    assert_eq!(descriptor_string(&fields, "parentSessionId"), "s".into());
    let (continuable, _) = parse_descriptor(&descriptor(
        r#"{"version":1,"mode":"one-shot","provider":"pilotdeck-native","definitionId":"explore"}"#,
    ))
    .expect("valid")
    .expect("known version");
    assert!(!continuable);
    // An unknown version is not an error: there is just no descriptor.
    assert_eq!(
        parse_descriptor(&descriptor(r#"{"version":3}"#)).expect("valid"),
        None
    );
}

#[test]
fn descriptor_errors_follow_parse_subagent_descriptor() {
    let error = |json: &str| parse_descriptor(&descriptor(json)).expect_err("invalid");
    assert_eq!(
        error("[]"),
        "Persisted subagent descriptor must be an object."
    );
    assert_eq!(
        error(r#"{"version":"2"}"#),
        "Persisted subagent descriptor version must be a number."
    );
    assert_eq!(
        error(&CONTINUABLE.replace(r#""agentModel":"m""#, r#""agentModel":"m","extra":1"#)),
        "Persisted subagent descriptor has unknown field \"extra\"."
    );
    assert_eq!(
        error(&CONTINUABLE.replace("continuable", "one-shot")),
        "Persisted subagent descriptor mode must be \"continuable\"."
    );
    assert_eq!(
        error(&CONTINUABLE.replace(r#""label":"Look""#, r#""label":"  ""#)),
        "Subagent descriptor label must be a non-empty string."
    );
}

#[test]
fn child_transcripts_live_under_the_parent_chat_directory() {
    assert_eq!(
        transcript_path(
            std::path::Path::new("/h/chats"),
            &"parent".into(),
            &"child".into()
        ),
        PathBuf::from("/h/chats/parent/subagents/child.jsonl")
    );
    // Ids are sanitized like every session file name, so an id cannot
    // leave the parent's directory.
    let escaped = transcript_path(
        std::path::Path::new("/h/chats"),
        &"parent".into(),
        &"../../x".into(),
    );
    assert!(
        escaped.starts_with("/h/chats/parent/subagents/"),
        "{escaped:?}"
    );
    assert_eq!(escaped.components().count(), 6, "{escaped:?}");
}

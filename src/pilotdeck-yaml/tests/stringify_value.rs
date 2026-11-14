//! `stringify(parse(src) with adapters ??= {}, { lineWidth: 0, singleQuote: false })`
//! as `pilotdeck gateway setup` writes pilotdeck.yaml (gatewaySetup.ts saveYamlConfig).
//! Expected texts were produced by the baseline's yaml@2.9.0 under Node.
use pilotdeck_protocol::JsString;
use pilotdeck_yaml::stringify::{EditableDocument, StringifyOptions};
use pilotdeck_yaml::value::{Container, Value};

fn render(src: &str) -> String {
    let doc =
        pilotdeck_yaml::parse_document(&JsString::from(src), pilotdeck_yaml::Options::default());
    assert!(doc.errors.is_empty(), "{src}");
    let mut graph = doc.to_json().expect("json");
    let Value::Ref(root) = graph.root else {
        panic!("object root")
    };
    let Container::Object(entries) = &mut graph.nodes[root] else {
        panic!("object root")
    };
    if !entries
        .iter()
        .any(|(k, _)| k == &JsString::from("adapters"))
    {
        graph.nodes.push(Container::Object(Vec::new()));
        let id = graph.nodes.len() - 1;
        let Container::Object(entries) = &mut graph.nodes[root] else {
            unreachable!()
        };
        entries.push((JsString::from("adapters"), Value::Ref(id)));
    }
    let options = StringifyOptions {
        line_width: 0,
        single_quote: Some(false),
    };
    EditableDocument::stringify_value(&graph, options).expect("stringify")
}

#[test]
fn case_0() {
    assert_eq!(render("a: 1\n"), "a: 1\nadapters: {}\n");
}

#[test]
fn case_1() {
    assert_eq!(
        render(
            "schemaVersion: 1\n# comment\nagent:\n  model: bbx/deterministic\nlist: [1, 2, \"x\"]\nempty: {}\nearr: []\n"
        ),
        "schemaVersion: 1\nagent:\n  model: bbx/deterministic\nlist:\n  - 1\n  - 2\n  - x\nempty: {}\nearr: []\nadapters: {}\n"
    );
}

#[test]
fn case_2() {
    assert_eq!(
        render(
            "s1: \"true\"\ns2: \"123\"\ns3: \"\"\ns4: \"a: b\"\ns5: \"it's\"\ns6: 'say \"hi\"'\ns7: \"line1\\nline2\\n\"\ns8: \" lead\"\ns9: \"x #y\"\nn: null\nf: 1.50\nneg: -0\ninf: .inf\nnan: .nan\nbig: 12345678901234567890\n"
        ),
        "s1: \"true\"\ns2: \"123\"\ns3: \"\"\ns4: \"a: b\"\ns5: it's\ns6: say \"hi\"\ns7: |\n  line1\n  line2\ns8: \" lead\"\ns9: \"x #y\"\nn: null\nf: 1.5\nneg: -0\ninf: .inf\nnan: .nan\nbig: 12345678901234567000\nadapters: {}\n"
    );
}

#[test]
fn case_3() {
    assert_eq!(
        render(
            "long: word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word\nlongq: \"w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x \"\n"
        ),
        "long: word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word\nlongq: \"w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x w\\\"x \"\nadapters: {}\n"
    );
}

#[test]
fn case_4() {
    assert_eq!(
        render("base: &b\n  k: v\nuse1: *b\nuse2: *b\nother: &o [1]\nagain: *o\n"),
        "base: &a1\n  k: v\nuse1: *a1\nuse2: *a1\nother: &a2\n  - 1\nagain: *a2\nadapters: {}\n"
    );
}

#[test]
fn case_5() {
    assert_eq!(
        render("\"2\": two\nb: bee\n\"1\": one\na: ay\n"),
        "\"1\": one\n\"2\": two\nb: bee\na: ay\nadapters: {}\n"
    );
}

#[test]
fn case_6() {
    assert_eq!(
        render(
            "arr:\n  - k: v\n    k2: v2\n  - - nested\n    - list\n  - plain\nml: |\n  first\n  second\nctrl: \"a\\x01b\"\nuni: \"中文 ✅\"\nkey with space: v\n\"key: colon\": v\n? [complex]\n: v\n"
        ),
        "arr:\n  - k: v\n    k2: v2\n  - - nested\n    - list\n  - plain\nml: |\n  first\n  second\nctrl: \"a\\x01b\"\nuni: 中文 ✅\nkey with space: v\n\"key: colon\": v\n\"[ complex ]\": v\nadapters: {}\n"
    );
}

#[test]
fn case_7() {
    assert_eq!(
        render("adapters:\n  wecom:\n    extra:\n      groups:\n        g1: {a: 1}\n"),
        "adapters:\n  wecom:\n    extra:\n      groups:\n        g1:\n          a: 1\n"
    );
}

#[test]
fn case_8() {
    assert_eq!(
        render(
            "x: \"multi line with a lot of words that keeps going\\nsecond line also long enough to exceed the minimum length\"\n"
        ),
        "x: |-\n  multi line with a lot of words that keeps going\n  second line also long enough to exceed the minimum length\nadapters: {}\n"
    );
}

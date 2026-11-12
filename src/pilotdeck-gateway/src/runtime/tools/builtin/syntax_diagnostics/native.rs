use super::{SyntaxDiagnostic, diagnostic, trim_diagnostic_message};

pub(super) fn javascript(path: &str, content: &str) -> Vec<SyntaxDiagnostic> {
    let allocator = oxc_allocator::Allocator::default();
    let source_type = oxc_span::SourceType::from_path(path)
        .unwrap_or_else(|_| oxc_span::SourceType::unambiguous())
        .with_unambiguous(true);
    oxc_parser::Parser::new(&allocator, content, source_type)
        .with_options(oxc_parser::ParseOptions {
            parse_regular_expression: true,
            ..Default::default()
        })
        .parse()
        .errors
        .into_iter()
        .take(super::MAX_DIAGNOSTICS)
        .map(|error| {
            let offset = error
                .labels
                .as_ref()
                .and_then(|labels| labels.first())
                .map_or(0, |label| label.offset());
            let (line, column) = position(content, offset);
            diagnostic(line, column, trim_diagnostic_message(&error.message))
        })
        .collect()
}

/// Parser spans use UTF-8 byte offsets; the public tool reports UTF-16 columns.
fn position(content: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(content.len());
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = &content[..end];
    (
        prefix.bytes().filter(|&c| c == b'\n').count() + 1,
        prefix
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .encode_utf16()
            .count()
            + 1,
    )
}

pub(super) fn html(content: &str) -> Vec<SyntaxDiagnostic> {
    super::super::web::html_dom::fragment_diagnostics(content)
        .into_iter()
        .map(|(line, message)| {
            diagnostic(
                line as usize,
                1,
                trim_diagnostic_message(&format!("HTML parse error: {message}")),
            )
        })
        .collect()
}

/// Strict CSV records: escaped quotes and multiline fields are accepted;
/// malformed quoting and inconsistent record widths are reported. The parser
/// never materializes cell values and therefore uses constant auxiliary space.
pub(super) fn csv(content: &str, tab: bool) -> Vec<SyntaxDiagnostic> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Start,
        Bare,
        Quoted,
        Closed,
    }
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let delimiter = if tab { b'\t' } else { b',' };
    let bytes = content.as_bytes();
    let mut state = State::Start;
    let mut fields = 1;
    let mut width = None;
    let mut touched = false;
    let mut record_separator: Option<&[u8]> = None;
    let mut index = 0;
    let fail = |index, message| {
        let (line, column) = position(content, index);
        vec![diagnostic(line, column, message)]
    };
    while index < bytes.len() {
        let byte = bytes[index];
        if state == State::Quoted {
            if byte == b'"' {
                if bytes.get(index + 1) == Some(&b'"') {
                    index += 2;
                    continue;
                }
                state = State::Closed;
            }
            index += 1;
            continue;
        }
        if byte == delimiter {
            fields += 1;
            touched = true;
            state = State::Start;
        } else if record_separator.map_or_else(
            || matches!(byte, b'\r' | b'\n'),
            |separator| bytes[index..].starts_with(separator),
        ) {
            let separator = *record_separator.get_or_insert_with(|| {
                if bytes[index..].starts_with(b"\r\n") {
                    b"\r\n"
                } else if byte == b'\r' {
                    b"\r"
                } else {
                    b"\n"
                }
            });
            if touched {
                if let Some(expected) = width {
                    if fields != expected {
                        return fail(
                            index,
                            format!(
                                "Invalid record length: expected {expected} fields, found {fields}."
                            ),
                        );
                    }
                } else {
                    width = Some(fields);
                }
            }
            index += separator.len() - 1;
            fields = 1;
            touched = false;
            state = State::Start;
        } else if byte == b'"' {
            if state != State::Start {
                return fail(index, "Invalid quote in an unquoted field.".into());
            }
            touched = true;
            state = State::Quoted;
        } else {
            if state == State::Closed {
                return fail(index, "Invalid character after a closing quote.".into());
            }
            touched = true;
            state = State::Bare;
        }
        index += 1;
    }
    if state == State::Quoted {
        return fail(index, "Unclosed quoted field.".into());
    }
    if touched
        && let Some(expected) = width
        && fields != expected
    {
        return fail(
            index,
            format!("Invalid record length: expected {expected} fields, found {fields}."),
        );
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oxc_checks_js_ts_jsx_and_utf16_locations() {
        for path in [
            "a.js", "a.cjs", "a.mjs", "a.ts", "a.cts", "a.mts", "a.tsx", "a.jsx",
        ] {
            assert!(!javascript(path, "const broken = ;").is_empty(), "{path}");
        }
        assert!(javascript("a.tsx", "const x: number = 1; const el = <p>{x}</p>;").is_empty());
        assert!(javascript("a.js", "const x = /ab+/g;").is_empty());
        assert_eq!(position("中😀\nx😀;", "中😀\nx😀".len()), (2, 4));
        let errors = javascript("a.ts", "// first\nconst x: = 1;");
        assert_eq!(errors[0].line, 2.0);
    }

    #[test]
    fn html_reports_malformed_fragments_but_accepts_valid_markup() {
        assert!(html("<section><p>hello</p></section>").is_empty());
        assert!(!html("<p a='one' a='two'>hello</p>").is_empty());
        assert!(!html("<p title=\"unterminated").is_empty());
    }

    #[test]
    fn csv_validates_quotes_multiline_records_bom_and_width() {
        assert!(
            csv(
                "\u{feff}a,b\r\n\r\n\"one\nline\",\"escaped \"\"quote\"\"\"\r\n",
                false
            )
            .is_empty()
        );
        assert!(csv("a\tb\n1\t2", true).is_empty());
        assert!(!csv("a,b\r\n\n\"one\",two", false).is_empty());
        for value in ["a,b\n1", "a,b\n1,2,3", "a,\"broken", "a,b\"c", "a,\"b\"x"] {
            assert!(!csv(value, false).is_empty(), "{value}");
        }
        assert_eq!(csv("a,b\n1", false)[0].line, 2.0);
    }
}

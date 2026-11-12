//! Port of TS `src/always-on/contracts/PlanContract.ts` (`parsePlanMarkdown`
//! with the default options): the strict plan markdown contract the
//! discovery plan tool enforces. Lengths are JS string lengths (UTF-16).

/// `PLAN_REQUIRED_SECTIONS` (PlanContract.ts:31-38).
const PLAN_REQUIRED_SECTIONS: [&str; 6] = [
    "Summary",
    "Rationale",
    "Context Signals",
    "Proposed Change",
    "Execution Steps",
    "Verification",
];

const PLAN_METADATA_KEYS: [&str; 5] =
    ["id", "sourceRunId", "createdAt", "projectRoot", "dedupeKey"];

/// `PlanParseResult` — the parts the plan tool reads.
pub(crate) struct ParsedPlan {
    pub title: String,
    /// `(key, value)` in `PLAN_METADATA_KEYS` order.
    pub metadata: Vec<(&'static str, String)>,
    pub raw_content: String,
}

impl ParsedPlan {
    pub(crate) fn meta(&self, key: &str) -> &str {
        self.metadata
            .iter()
            .find(|(name, _)| *name == key)
            .map_or("", |(_, value)| value.as_str())
    }
}

fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

/// The `plan_invalid` message (the code is the tool's prefix).
type PlanError = String;

/// `parsePlanMarkdown(content)` (PlanContract.ts:59-210).
pub(crate) fn parse_plan_markdown(content: &str) -> Result<ParsedPlan, PlanError> {
    let normalized = content.replace("\r\n", "\n").replace('\u{a0}', " ");
    if normalized.is_empty() {
        return Err("plan content is empty.".into());
    }
    if js_len(&normalized) > 100_000 {
        return Err("plan content exceeds maxResultSizeChars (100000).".into());
    }
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut cursor = 0;
    let blank = |line: &str| js_trim(line).is_empty();
    while cursor < lines.len() && blank(lines[cursor]) {
        cursor += 1;
    }
    let title_line = lines.get(cursor).copied().unwrap_or("");
    let Some(title) = title_line.strip_prefix("# ") else {
        return Err("plan must start with a level-1 markdown heading.".into());
    };
    let title = js_trim(title).to_string();
    if title.is_empty() {
        return Err("plan title must not be empty.".into());
    }
    cursor += 1;
    while cursor < lines.len() && blank(lines[cursor]) {
        cursor += 1;
    }
    let mut metadata_lines: Vec<&str> = Vec::new();
    while cursor < lines.len() && lines[cursor].starts_with('>') {
        let line = &lines[cursor][1..];
        metadata_lines.push(
            line.strip_prefix(|ch: char| ch.is_whitespace())
                .unwrap_or(line),
        );
        cursor += 1;
    }
    if metadata_lines.is_empty() {
        return Err("plan must include a metadata blockquote immediately after the title.".into());
    }
    if js_trim(metadata_lines[0]) != "Always-On Discovery Plan" {
        return Err(
            "plan metadata blockquote first line must be \"Always-On Discovery Plan\".".into(),
        );
    }
    let mut metadata: Vec<(&'static str, String)> = PLAN_METADATA_KEYS
        .iter()
        .map(|key| (*key, String::new()))
        .collect();
    for line in &metadata_lines[1..] {
        let trimmed = js_trim(line);
        if trimmed.is_empty() {
            continue;
        }
        let Some(colon) = trimmed.find(':') else {
            return Err(format!(
                "plan metadata line \"{line}\" is not in \"key: value\" form."
            ));
        };
        let key = js_trim(&trimmed[..colon]);
        let value = js_trim(&trimmed[colon + 1..]);
        match metadata.iter_mut().find(|(name, _)| *name == key) {
            Some(slot) => slot.1 = value.to_string(),
            None => return Err(format!("plan metadata contains unknown key \"{key}\".")),
        }
    }
    for (key, value) in &metadata {
        if value.is_empty() {
            return Err(format!("plan metadata is missing required key \"{key}\"."));
        }
    }
    while cursor < lines.len() && blank(lines[cursor]) {
        cursor += 1;
    }

    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in &lines[cursor..] {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some(done) = current.take() {
                sections.push(done);
            }
            let name = js_trim(rest).to_string();
            if name.is_empty() {
                return Err("plan section heading must not be empty.".into());
            }
            if sections.iter().any(|(seen, _)| *seen == name) {
                return Err(format!("plan contains duplicate section \"{name}\"."));
            }
            current = Some((name, Vec::new()));
            continue;
        }
        if line.starts_with("# ") {
            return Err("plan must contain exactly one level-1 heading at the top.".into());
        }
        if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    if let Some(done) = current.take() {
        sections.push(done);
    }
    if sections.len() != PLAN_REQUIRED_SECTIONS.len() {
        return Err(format!(
            "plan must contain exactly the required sections: {}.",
            PLAN_REQUIRED_SECTIONS.join(", ")
        ));
    }
    for (index, expected) in PLAN_REQUIRED_SECTIONS.iter().enumerate() {
        if sections[index].0 != *expected {
            return Err(format!(
                "plan section #{} must be \"{expected}\", got \"{}\".",
                index + 1,
                sections[index].0
            ));
        }
    }
    let body = |index: usize| &sections[index].1;
    let non_empty = |index: usize, name: &str| -> Result<String, PlanError> {
        let text = js_trim(&body(index).join("\n")).to_string();
        if text.is_empty() {
            return Err(format!("plan section \"{name}\" must not be empty."));
        }
        Ok(text)
    };
    let bullet = |line: &&str| {
        let rest = line.trim_start_matches(|ch: char| ch.is_whitespace());
        rest.starts_with('-') && rest[1..].starts_with(|ch: char| ch.is_whitespace())
    };
    // Summary.
    let summary = non_empty(0, "Summary")?;
    if js_len(&summary) > 200 {
        return Err("plan Summary exceeds 200 characters.".into());
    }
    if is_multi_paragraph(&summary) {
        return Err("plan Summary must be a single paragraph.".into());
    }
    non_empty(1, "Rationale")?;
    if !body(2).iter().any(bullet) {
        return Err("plan Context Signals must contain at least one unordered list item.".into());
    }
    let proposed = non_empty(3, "Proposed Change")?;
    // DEFAULT_FUZZY_TODOS: /^\s*TODO\b/i and /^\s*待补充\b/ (no `m` flag:
    // the start of the whole text).
    let head = proposed.trim_start_matches(|ch: char| ch.is_whitespace());
    if head
        .get(..4)
        .is_some_and(|word| word.eq_ignore_ascii_case("todo"))
        && !word_char_at(head, 4)
    {
        return Err("plan Proposed Change must not be fuzzy (\"^\\s*TODO\\b\").".into());
    }
    if let Some(rest) = head.strip_prefix("待补充")
        && rest.chars().next().is_some_and(is_word_char)
    {
        return Err("plan Proposed Change must not be fuzzy (\"^\\s*待补充\\b\").".into());
    }
    // Execution Steps.
    let steps: Vec<&str> = body(4)
        .iter()
        .map(|line| js_trim(line))
        .filter(|line| !line.is_empty())
        .collect();
    if steps.is_empty() {
        return Err("plan Execution Steps must contain at least one ordered list item.".into());
    }
    let mut ordered = 0;
    for line in steps {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0
            && line[digits..].starts_with('.')
            && line[digits + 1..].starts_with(|ch: char| ch.is_whitespace())
        {
            ordered += 1;
            continue;
        }
        if line.starts_with(['-', '*', '+']) && line[1..].starts_with(|ch: char| ch.is_whitespace())
        {
            return Err(
                "plan Execution Steps must use an ordered list (no unordered items).".into(),
            );
        }
    }
    if ordered == 0 {
        return Err("plan Execution Steps must contain at least one ordered list item.".into());
    }
    if !body(5).iter().any(bullet) {
        return Err("plan Verification must contain at least one unordered list item.".into());
    }
    let raw_content = format!(
        "{}\n",
        normalized.trim_end_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
    );
    Ok(ParsedPlan {
        title,
        metadata,
        raw_content,
    })
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn word_char_at(text: &str, index: usize) -> bool {
    text[index..].chars().next().is_some_and(is_word_char)
}

/// `text.split(/\n\s*\n/).length > 1`.
fn is_multi_paragraph(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if *ch != '\n' {
            continue;
        }
        let mut next = index + 1;
        while next < chars.len() && chars[next].is_whitespace() && chars[next] != '\n' {
            next += 1;
        }
        if next < chars.len() && chars[next] == '\n' {
            return true;
        }
        // `\s*` also swallows newlines: any later newline reached through
        // whitespace only counts.
        let mut probe = index + 1;
        while probe < chars.len() && chars[probe].is_whitespace() {
            if chars[probe] == '\n' {
                return true;
            }
            probe += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negative_assert_eq;

    const PLAN: &str = "# T\n\n> Always-On Discovery Plan\n> id: plan_1\n> sourceRunId: r\n> createdAt: c\n> projectRoot: /p\n> dedupeKey: d\n\n## Summary\nS.\n\n## Rationale\nR.\n\n## Context Signals\n- a\n\n## Proposed Change\nDo.\n\n## Execution Steps\n1. one\n\n## Verification\n- ok\n\n";

    #[test]
    fn accepts_the_contract_and_trims_the_tail() {
        let parsed = parse_plan_markdown(PLAN).expect("valid");
        negative_assert_eq!(parsed.title.clone(), "T".to_string(), "title");
        negative_assert_eq!(parsed.meta("id").to_string(), "plan_1".to_string(), "id");
        assert!(parsed.raw_content.ends_with("- ok\n"));
    }

    #[test]
    fn rejects_contract_violations() {
        for (plan, message) in [
            ("", "plan content is empty."),
            ("x", "plan must start with a level-1 markdown heading."),
            (
                &PLAN.replace("1. one", "- one") as &str,
                "plan Execution Steps must use an ordered list (no unordered items).",
            ),
            (
                &PLAN.replace("Do.", "todo later") as &str,
                "plan Proposed Change must not be fuzzy (\"^\\s*TODO\\b\").",
            ),
            (
                &PLAN.replace("S.", "a\n\nb") as &str,
                "plan Summary must be a single paragraph.",
            ),
        ] {
            negative_assert_eq!(
                parse_plan_markdown(plan).err(),
                Some(message.to_string()),
                "contract violation"
            );
        }
    }
}

//! Port of TS `src/always-on/contracts/ReportContract.ts`: the forgiving
//! work-report contract (missing sections are filled and noted) and the
//! fallback report written when no report tool call landed.

/// `REPORT_METADATA_FIRST_LINE` (ReportContract.ts:29).
const REPORT_METADATA_FIRST_LINE: &str = "Always-On Discovery Run Report";

/// `REPORT_REQUIRED_SECTIONS` (ReportContract.ts:30-38).
const REPORT_REQUIRED_SECTIONS: [&str; 7] = [
    "Plan Reference",
    "Steps Performed",
    "Files Changed",
    "Command Output",
    "Verification Results",
    "Follow-ups",
    "Notes",
];

/// `ReportMetadata` (ReportContract.ts:11-19).
pub(crate) struct ReportMetadata<'a> {
    pub run_id: &'a str,
    pub plan_id: &'a str,
    pub started_at: &'a str,
    pub finished_at: &'a str,
    pub outcome: &'a str,
    /// `"git-worktree"` or `"snapshot-copy"`.
    pub workspace_strategy: &'a str,
    pub workspace_handle: &'a str,
}

/// `ReportParseResult` — the parts the callers read.
pub(crate) struct ParsedReport {
    pub fallbacks: Vec<String>,
    pub raw_content: String,
}

fn metadata_lines(metadata: &ReportMetadata<'_>) -> Vec<String> {
    vec![
        format!("> {REPORT_METADATA_FIRST_LINE}"),
        format!("> runId: {}", metadata.run_id),
        format!("> planId: {}", metadata.plan_id),
        format!("> startedAt: {}", metadata.started_at),
        format!("> finishedAt: {}", metadata.finished_at),
        format!("> outcome: {}", metadata.outcome),
        format!("> workspaceStrategy: {}", metadata.workspace_strategy),
        format!("> workspaceHandle: {}", metadata.workspace_handle),
    ]
}

/// `buildFallbackReport` (ReportContract.ts:47-86), without the partial
/// payload (no call site passes one).
pub(crate) fn build_fallback_report(
    metadata: &ReportMetadata<'_>,
    title: &str,
    reason: &str,
) -> String {
    let mut lines = vec![format!("# {title} - Work Report"), String::new()];
    lines.extend(metadata_lines(metadata));
    for line in [
        "",
        "## Plan Reference",
        "(unavailable: report tool was not invoked)",
        "",
        "## Steps Performed",
        "(empty)",
        "",
        "## Files Changed",
        "(none recorded)",
        "",
        "## Command Output",
        "(none recorded)",
        "",
        "## Verification Results",
        "- [ ] (unverified) - report tool was not invoked",
        "",
        "## Follow-ups",
        "- Investigate why AlwaysOnReportTool was not called.",
        "",
        "## Notes",
    ] {
        lines.push(line.into());
    }
    lines.push(format!("- fallback: {reason}"));
    lines.join("\n") + "\n"
}

/// JS `String.prototype.trim` whitespace.
fn js_trim(value: &str) -> &str {
    value.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

fn js_trim_end(value: &str) -> &str {
    value.trim_end_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

/// An insertion-ordered section map (`Record<string, string>` assignment
/// keeps a re-assigned key in its first position).
#[derive(Default)]
struct Sections(Vec<(String, String)>);
impl Sections {
    fn set(&mut self, key: &str, value: String) {
        match self.0.iter_mut().find(|(name, _)| name == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key.to_string(), value)),
        }
    }
    fn get(&self, key: &str) -> Option<&String> {
        self.0.iter().find(|(name, _)| name == key).map(|(_, v)| v)
    }
    fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
    /// `Object.keys` order: canonical array indices ascending first, then
    /// the other keys in insertion order.
    fn keys(&self) -> Vec<&String> {
        let index = |key: &str| -> Option<u32> {
            if key == "0" {
                return Some(0);
            }
            if key.starts_with('0') || key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            key.parse::<u32>().ok().filter(|value| *value != u32::MAX)
        };
        let mut numeric: Vec<(u32, &String)> = self
            .0
            .iter()
            .filter_map(|(key, _)| index(key).map(|value| (value, key)))
            .collect();
        numeric.sort_by_key(|(value, _)| *value);
        let mut keys: Vec<&String> = numeric.into_iter().map(|(_, key)| key).collect();
        keys.extend(
            self.0
                .iter()
                .filter(|(key, _)| index(key).is_none())
                .map(|(key, _)| key),
        );
        keys
    }
}

/// `parseReportMarkdown` (ReportContract.ts:88-182).
pub(crate) fn parse_report_markdown(content: &str, metadata: &ReportMetadata<'_>) -> ParsedReport {
    let mut fallbacks: Vec<String> = Vec::new();
    let required = |name: &str| REPORT_REQUIRED_SECTIONS.contains(&name);
    let normalized = content.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut cursor = 0;
    while cursor < lines.len() && js_trim(lines[cursor]).is_empty() {
        cursor += 1;
    }
    let title_line = lines.get(cursor).copied().unwrap_or("");
    let title = if let Some(rest) = title_line.strip_prefix("# ") {
        let candidate = js_trim(rest);
        if required(candidate) {
            fallbacks.push("title-missing".into());
            "Always-On Discovery Run".to_string()
        } else {
            cursor += 1;
            candidate.to_string()
        }
    } else {
        fallbacks.push("title-missing".into());
        "Always-On Discovery Run".to_string()
    };
    while cursor < lines.len() && js_trim(lines[cursor]).is_empty() {
        cursor += 1;
    }
    while cursor < lines.len() && lines[cursor].starts_with('>') {
        cursor += 1;
    }
    while cursor < lines.len() && js_trim(lines[cursor]).is_empty() {
        cursor += 1;
    }

    let mut sections = Sections::default();
    let mut current: Option<String> = None;
    let mut buffer: Vec<&str> = Vec::new();
    let flush = |sections: &mut Sections, current: &mut Option<String>, buffer: &mut Vec<&str>| {
        if let Some(name) = current.take() {
            sections.set(&name, js_trim_end(&buffer.join("\n")).to_string());
        }
        buffer.clear();
    };
    for line in &lines[cursor..] {
        if let Some(rest) = line.strip_prefix("## ") {
            flush(&mut sections, &mut current, &mut buffer);
            current = Some(js_trim(rest).to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            let candidate = js_trim(rest);
            if required(candidate) {
                flush(&mut sections, &mut current, &mut buffer);
                current = Some(candidate.to_string());
                fallbacks.push(format!("h1-downgraded({candidate})"));
                continue;
            }
        }
        if current.is_some() {
            buffer.push(line);
        }
    }
    flush(&mut sections, &mut current, &mut buffer);

    for section in REPORT_REQUIRED_SECTIONS {
        if !sections.has(section) {
            let value = if section == "Notes" {
                format!("- fallback: section-missing({section})")
            } else {
                "(empty)".to_string()
            };
            sections.set(section, value);
            fallbacks.push(format!("section-missing({section})"));
        }
    }
    if !fallbacks.is_empty() {
        let mut notes: Vec<String> = sections
            .get("Notes")
            .map(|notes| notes.split('\n').map(str::to_string).collect())
            .unwrap_or_default();
        for fallback in &fallbacks {
            let entry = format!("- fallback: {fallback}");
            if !notes.contains(&entry) {
                notes.push(entry);
            }
        }
        sections.set("Notes", notes.join("\n"));
    }

    // `rebuildReport` (ReportContract.ts:184-211).
    let mut out = vec![format!("# {title}"), String::new()];
    out.extend(metadata_lines(metadata));
    for section in REPORT_REQUIRED_SECTIONS {
        out.push(String::new());
        out.push(format!("## {section}"));
        out.push(
            sections
                .get(section)
                .cloned()
                .unwrap_or_else(|| "(empty)".into()),
        );
    }
    for key in sections.keys() {
        if !required(key) {
            out.push(String::new());
            out.push(format!("## {key}"));
            out.push(sections.get(key).cloned().unwrap_or_default());
        }
    }
    ParsedReport {
        fallbacks,
        raw_content: out.join("\n") + "\n",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negative_assert_eq;

    fn metadata() -> ReportMetadata<'static> {
        ReportMetadata {
            run_id: "r",
            plan_id: "p",
            started_at: "s",
            finished_at: "f",
            outcome: "executed",
            workspace_strategy: "snapshot-copy",
            workspace_handle: "/w",
        }
    }

    // ReportContract.ts:157-173: missing sections are filled and noted, a
    // required h1 is downgraded, extra sections trail.
    #[test]
    fn fills_and_notes_missing_sections() {
        let parsed = parse_report_markdown(
            "# T\n\n## Plan Reference\np\n\n# Notes\n- ok\n\n## Extra\nx\n",
            &metadata(),
        );
        negative_assert_eq!(
            parsed.raw_content,
            "# T\n\n> Always-On Discovery Run Report\n> runId: r\n> planId: p\n> startedAt: s\n> finishedAt: f\n> outcome: executed\n> workspaceStrategy: snapshot-copy\n> workspaceHandle: /w\n\n## Plan Reference\np\n\n## Steps Performed\n(empty)\n\n## Files Changed\n(empty)\n\n## Command Output\n(empty)\n\n## Verification Results\n(empty)\n\n## Follow-ups\n(empty)\n\n## Notes\n- ok\n- fallback: h1-downgraded(Notes)\n- fallback: section-missing(Steps Performed)\n- fallback: section-missing(Files Changed)\n- fallback: section-missing(Command Output)\n- fallback: section-missing(Verification Results)\n- fallback: section-missing(Follow-ups)\n\n## Extra\nx\n".to_string(),
            "rebuilt report"
        );
    }
}

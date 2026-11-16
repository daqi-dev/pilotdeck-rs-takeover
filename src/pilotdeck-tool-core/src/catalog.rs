//! Pure tool catalog metadata and concurrency batch partitioning.

use pilotdeck_protocol::JsValue;

/// Whether a tool implementation is strictly read-only and concurrency-safe,
/// or may produce side effects / requires serial isolation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ToolConcurrencyPolicy {
    ConcurrentReadOnly,
    #[default]
    SerialSideEffect,
}

/// Pure metadata entry describing one tool in a session catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCatalogEntry {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub kind: String,
    pub input_schema: JsValue,
    pub concurrency: ToolConcurrencyPolicy,
}

impl ToolCatalogEntry {
    /// Returns true when `candidate` matches the canonical tool name or one of its aliases.
    pub fn matches_name(&self, candidate: &str) -> bool {
        self.name == candidate || self.aliases.iter().any(|alias| alias == candidate)
    }
}

/// Candidate call within an assistant tool-call batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallCandidate {
    pub call_id: String,
    pub tool_name: String,
    pub read_only_call: bool,
}

/// One execution slice produced by [`partition_tool_batch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolBatchSlice {
    pub concurrent: bool,
    pub call_ids: Vec<String>,
}

fn find_catalog_entry<'a>(
    catalog: &'a [ToolCatalogEntry],
    tool_name: &str,
) -> Option<&'a ToolCatalogEntry> {
    catalog
        .iter()
        .find(|entry| entry.name == tool_name)
        .or_else(|| {
            catalog
                .iter()
                .find(|entry| entry.aliases.iter().any(|alias| alias == tool_name))
        })
}

/// Partition an ordered batch of tool calls into contiguous parallel read-only
/// slices and isolated single-item serial slices.
///
/// Contiguous calls whose catalog entry has
/// [`ToolConcurrencyPolicy::ConcurrentReadOnly`] and whose `read_only_call`
/// flag is `true` are grouped into `concurrent: true` slices. Every
/// non-readonly, side-effecting, or unknown tool call is isolated into a
/// single-item `concurrent: false` serial slice (fail-closed).
pub fn partition_tool_batch(
    catalog: &[ToolCatalogEntry],
    calls: &[ToolCallCandidate],
) -> Vec<ToolBatchSlice> {
    let mut slices: Vec<ToolBatchSlice> = Vec::new();
    for call in calls {
        let entry = find_catalog_entry(catalog, &call.tool_name);
        let can_run_concurrent_readonly = call.read_only_call
            && entry
                .is_some_and(|item| item.concurrency == ToolConcurrencyPolicy::ConcurrentReadOnly);

        if can_run_concurrent_readonly {
            if let Some(last) = slices.last_mut()
                && last.concurrent
            {
                last.call_ids.push(call.call_id.clone());
                continue;
            }
            slices.push(ToolBatchSlice {
                concurrent: true,
                call_ids: vec![call.call_id.clone()],
            });
        } else {
            slices.push(ToolBatchSlice {
                concurrent: false,
                call_ids: vec![call.call_id.clone()],
            });
        }
    }
    slices
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_protocol::parse_json;

    fn entry(name: &str, aliases: &[&str], concurrency: ToolConcurrencyPolicy) -> ToolCatalogEntry {
        ToolCatalogEntry {
            name: name.to_owned(),
            aliases: aliases.iter().map(|s| (*s).to_owned()).collect(),
            description: format!("tool {name}"),
            kind: "custom".to_owned(),
            input_schema: parse_json(r#"{"type":"object"}"#).unwrap(),
            concurrency,
        }
    }

    fn candidate(call_id: &str, tool_name: &str, read_only_call: bool) -> ToolCallCandidate {
        ToolCallCandidate {
            call_id: call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            read_only_call,
        }
    }

    #[test]
    fn groups_contiguous_readonly_and_isolates_side_effects_and_unknowns() {
        let catalog = vec![
            entry(
                "read_file",
                &["ReadFile"],
                ToolConcurrencyPolicy::ConcurrentReadOnly,
            ),
            entry(
                "get_current_time",
                &[],
                ToolConcurrencyPolicy::ConcurrentReadOnly,
            ),
            entry("write_file", &[], ToolConcurrencyPolicy::SerialSideEffect),
            entry(
                "execute_code",
                &[],
                ToolConcurrencyPolicy::ConcurrentReadOnly,
            ),
        ];
        let calls = vec![
            candidate("c1", "ReadFile", true),
            candidate("c2", "get_current_time", true),
            candidate("c3", "write_file", false),
            candidate("c4", "write_file", true),
            candidate("c5", "execute_code", false),
            candidate("c6", "read_file", true),
            candidate("c7", "execute_code", true),
            candidate("c8", "unknown_tool", true),
        ];

        let slices = partition_tool_batch(&catalog, &calls);
        assert_eq!(
            slices,
            vec![
                ToolBatchSlice {
                    concurrent: true,
                    call_ids: vec!["c1".into(), "c2".into()],
                },
                ToolBatchSlice {
                    concurrent: false,
                    call_ids: vec!["c3".into()],
                },
                ToolBatchSlice {
                    concurrent: false,
                    call_ids: vec!["c4".into()],
                },
                ToolBatchSlice {
                    concurrent: false,
                    call_ids: vec!["c5".into()],
                },
                ToolBatchSlice {
                    concurrent: true,
                    call_ids: vec!["c6".into(), "c7".into()],
                },
                ToolBatchSlice {
                    concurrent: false,
                    call_ids: vec!["c8".into()],
                },
            ]
        );
    }

    #[test]
    fn empty_batch_produces_no_slices() {
        assert!(partition_tool_batch(&[], &[]).is_empty());
    }
}

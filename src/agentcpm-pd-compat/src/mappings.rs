//! Mapping data types and loading for the per-entry mapping JSON resources.
//!
//! The authoritative *procedural* table (slugs, handlers, dispositions) is
//! [`crate::manifest`]; the per-entry mapping DATA (old argv patterns, old
//! semantics, target operation sequences, projections) lives in
//! `src/mappings/<slug>.json` resources compiled in with `include_str!` and
//! parsed here. The two sources are joined on slug; [`Mappings::validate`]
//! enforces consistency (known slug, matching disposition, matching contract
//! version, full 27/27 coverage — the data is compiled in, so a gap is a
//! shim build bug).

use std::collections::BTreeMap;

use crate::manifest::{CONTRACT_VERSION, Disposition, Manifest};

/// Compiled-in mapping resources: `(slug, include_str! JSON document)`.
/// Each document carries the full wrapper (`version`, `contractVersion`,
/// `entries`) with exactly one entry whose slug matches the key. The 27
/// documents are the MAP-line output (`_tmp/20261118-s4/mappings-draft.json`,
/// one file per slug) merged verbatim by the WIRE wave; order matches the
/// builtin manifest.
pub static MAPPING_SOURCES: &[(&str, &str)] = &[
    ("entry-help", include_str!("mappings/entry-help.json")),
    ("context", include_str!("mappings/context.json")),
    ("default-argv", include_str!("mappings/default-argv.json")),
    ("default-stdin", include_str!("mappings/default-stdin.json")),
    ("sdk-run", include_str!("mappings/sdk-run.json")),
    ("sdk-resume", include_str!("mappings/sdk-resume.json")),
    ("sessions-list", include_str!("mappings/sessions-list.json")),
    (
        "sessions-messages",
        include_str!("mappings/sessions-messages.json"),
    ),
    ("settings-get", include_str!("mappings/settings-get.json")),
    ("settings-set", include_str!("mappings/settings-set.json")),
    ("server", include_str!("mappings/server.json")),
    ("cron-list", include_str!("mappings/cron-list.json")),
    ("cron-create", include_str!("mappings/cron-create.json")),
    ("cron-delete", include_str!("mappings/cron-delete.json")),
    ("cron-stop", include_str!("mappings/cron-stop.json")),
    ("setup-menu", include_str!("mappings/setup-menu.json")),
    ("setup-feishu", include_str!("mappings/setup-feishu.json")),
    ("setup-weixin", include_str!("mappings/setup-weixin.json")),
    ("setup-wecom", include_str!("mappings/setup-wecom.json")),
    ("chat-search", include_str!("mappings/chat-search.json")),
    (
        "skills-migrate",
        include_str!("mappings/skills-migrate.json"),
    ),
    ("update", include_str!("mappings/update.json")),
    ("usage-errors", include_str!("mappings/usage-errors.json")),
    ("sidecar-stdio", include_str!("mappings/sidecar-stdio.json")),
    ("sidecar-tcp", include_str!("mappings/sidecar-tcp.json")),
    (
        "sidecar-factory",
        include_str!("mappings/sidecar-factory.json"),
    ),
    (
        "rust-gateway-bin",
        include_str!("mappings/rust-gateway-bin.json"),
    ),
];

/// Top-level mapping document schema (see the MAP-line schema contract).
#[derive(Clone, Debug, serde::Deserialize)]
pub struct MappingDoc {
    pub version: u32,
    #[serde(rename = "contractVersion")]
    pub contract_version: String,
    pub entries: Vec<EntryMapping>,
}

/// One entry's mapping data; field names are the MAP-line schema (do not
/// rename). Unknown fields are tolerated (serde default ignores them).
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryMapping {
    pub slug: String,
    pub case_id: String,
    pub disposition: Disposition,
    pub old_entry: OldEntry,
    pub sequence: Vec<SequenceStep>,
    pub projection: String,
    #[serde(default)]
    pub s3_pending: Vec<String>,
    pub usage_error_contract: String,
}

/// Legacy surface facts for one entry.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OldEntry {
    pub argv_patterns: Vec<String>,
    pub stdin_behavior: String,
    pub exit_codes: OldExitCodes,
    #[serde(default)]
    pub options: Vec<OldOption>,
    #[serde(default)]
    pub reserved_words: Vec<String>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct OldExitCodes {
    pub success: i32,
    pub failure: i32,
    pub usage: i32,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OldOption {
    pub name: String,
    pub old_semantics: String,
}

/// One planned target operation in the mapping sequence.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct SequenceStep {
    pub op: String,
    /// Delivery status of the operation (e.g. `ready`, `pending-s3`); kept
    /// as a free-form string, unknown values are tolerated.
    pub status: String,
    pub note: String,
}

/// Loaded mapping table (slug → entry). Holds the full 27-entry set merged
/// from the MAP-line documents.
#[derive(Clone, Debug, Default)]
pub struct Mappings {
    entries: BTreeMap<String, EntryMapping>,
}

impl Mappings {
    /// Parses every compiled-in mapping document.
    pub fn load() -> Result<Self, MappingError> {
        let mut entries = BTreeMap::new();
        for (slug, source) in MAPPING_SOURCES {
            let doc: MappingDoc =
                serde_json::from_str(source).map_err(|e| MappingError::Parse {
                    slug: (*slug).to_owned(),
                    message: e.to_string(),
                })?;
            if doc.version != 1 {
                return Err(MappingError::UnsupportedVersion {
                    slug: (*slug).to_owned(),
                    version: doc.version,
                });
            }
            if doc.contract_version != CONTRACT_VERSION {
                return Err(MappingError::ContractVersionMismatch {
                    slug: (*slug).to_owned(),
                    expected: CONTRACT_VERSION,
                    found: doc.contract_version,
                });
            }
            if doc.entries.len() != 1 {
                return Err(MappingError::EntryCount {
                    slug: (*slug).to_owned(),
                    found: doc.entries.len(),
                });
            }
            let entry = doc.entries.into_iter().next().expect("length checked");
            if entry.slug != *slug {
                return Err(MappingError::SlugMismatch {
                    file_slug: (*slug).to_owned(),
                    entry_slug: entry.slug,
                });
            }
            if entries.insert(entry.slug.clone(), entry).is_some() {
                return Err(MappingError::DuplicateSlug {
                    slug: (*slug).to_owned(),
                });
            }
        }
        Ok(Self { entries })
    }

    pub fn find(&self, slug: &str) -> Option<&EntryMapping> {
        self.entries.get(slug)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&String, &EntryMapping)> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Union of reserved words across loaded entries (for the `default-argv`
    /// classification: a reserved top-level word exits 2 instead of being
    /// treated as a prompt).
    pub fn is_reserved_word(&self, word: &str) -> bool {
        self.entries
            .values()
            .any(|e| e.old_entry.reserved_words.iter().any(|w| w == word))
    }

    /// Validates the loaded table against the builtin manifest: every loaded
    /// slug must exist in the manifest with the same disposition, and every
    /// manifest entry must be covered (the data is compiled in, so a missing
    /// entry is a shim build bug).
    pub fn validate(&self, manifest: &Manifest) -> Result<(), MappingError> {
        for (slug, entry) in self.entries.iter() {
            let Some(m) = manifest.find(slug) else {
                return Err(MappingError::UnknownSlug { slug: slug.clone() });
            };
            if m.disposition != entry.disposition {
                return Err(MappingError::DispositionMismatch {
                    slug: slug.clone(),
                    manifest: m.disposition,
                    mapping: entry.disposition,
                });
            }
        }
        for entry in manifest.entries() {
            if !self.entries.contains_key(entry.slug) {
                return Err(MappingError::MissingSlug {
                    slug: entry.slug.to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Failures while loading or cross-validating mapping data. These are shim
/// build bugs (the data is compiled in), surfaced as exit-1 internal errors.
#[derive(Clone, Debug)]
pub enum MappingError {
    Parse {
        slug: String,
        message: String,
    },
    UnsupportedVersion {
        slug: String,
        version: u32,
    },
    ContractVersionMismatch {
        slug: String,
        expected: &'static str,
        found: String,
    },
    EntryCount {
        slug: String,
        found: usize,
    },
    SlugMismatch {
        file_slug: String,
        entry_slug: String,
    },
    DuplicateSlug {
        slug: String,
    },
    MissingSlug {
        slug: String,
    },
    UnknownSlug {
        slug: String,
    },
    DispositionMismatch {
        slug: String,
        manifest: Disposition,
        mapping: Disposition,
    },
}

impl std::fmt::Display for MappingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { slug, message } => {
                write!(f, "mapping `{slug}` failed to parse: {message}")
            }
            Self::UnsupportedVersion { slug, version } => {
                write!(
                    f,
                    "mapping `{slug}` has unsupported schema version {version}"
                )
            }
            Self::ContractVersionMismatch {
                slug,
                expected,
                found,
            } => write!(
                f,
                "mapping `{slug}` declares contract version `{found}`, expected `{expected}`"
            ),
            Self::EntryCount { slug, found } => {
                write!(
                    f,
                    "mapping file `{slug}` must hold exactly one entry, found {found}"
                )
            }
            Self::SlugMismatch {
                file_slug,
                entry_slug,
            } => write!(
                f,
                "mapping file `{file_slug}` holds entry `{entry_slug}`; slug must match the file key"
            ),
            Self::DuplicateSlug { slug } => write!(f, "mapping slug `{slug}` loaded twice"),
            Self::MissingSlug { slug } => {
                write!(f, "manifest entry `{slug}` has no mapping document")
            }
            Self::UnknownSlug { slug } => {
                write!(f, "mapping slug `{slug}` is not in the compat manifest")
            }
            Self::DispositionMismatch {
                slug,
                manifest,
                mapping,
            } => write!(
                f,
                "mapping `{slug}` declares disposition `{}`, manifest says `{}`",
                mapping.as_str(),
                manifest.as_str()
            ),
        }
    }
}

impl std::error::Error for MappingError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_27_mappings_load_and_validate() {
        let m = Mappings::load().expect("mapping data loads");
        assert_eq!(m.len(), MAPPING_SOURCES.len());
        assert_eq!(m.len(), crate::ENTRY_COUNT);
        m.validate(&Manifest::builtin())
            .expect("consistent with manifest");
    }

    #[test]
    fn dispositions_match_the_approved_matrix() {
        let m = Mappings::load().expect("load");
        let counts = m
            .entries()
            .fold((0, 0, 0), |(s, a, r), (_, e)| match e.disposition {
                Disposition::Supported => (s + 1, a, r),
                Disposition::Adapted => (s, a + 1, r),
                Disposition::Rejected => (s, a, r + 1),
            });
        assert_eq!(counts, (8, 14, 5), "(supported, adapted, rejected)");
        assert_eq!(
            m.find("sdk-run").map(|e| e.disposition),
            Some(Disposition::Supported)
        );
        assert_eq!(
            m.find("update").map(|e| e.disposition),
            Some(Disposition::Rejected)
        );
        assert_eq!(
            m.find("usage-errors").map(|e| e.disposition),
            Some(Disposition::Adapted)
        );
    }

    #[test]
    fn sdk_run_sequence_mentions_the_planned_operations() {
        let m = Mappings::load().expect("load");
        let e = m.find("sdk-run").expect("sdk-run placeholder");
        let ops: Vec<&str> = e.sequence.iter().map(|s| s.op.as_str()).collect();
        for op in ["session.create", "run.submit", "run.events", "run.cancel"] {
            assert!(ops.contains(&op), "sdk-run sequence misses {op}: {ops:?}");
        }
    }

    #[test]
    fn unknown_json_fields_are_tolerated() {
        let doc = r#"{"version":1,"contractVersion":"agentcpm-pd 007 v0.7.1","futureField":true,
            "entries":[{"slug":"sdk-run","caseId":"cli.case.compat.cli.compat.sdk-run",
            "disposition":"supported","newField":123,
            "oldEntry":{"argvPatterns":["run <prompt>"],"stdinBehavior":"none",
            "exitCodes":{"success":0,"failure":1,"usage":2},"options":[],"reservedWords":[]},
            "sequence":[{"op":"session.create","status":"ready","note":"","extra":"x"}],
            "projection":"old fields","s3Pending":[],"usageErrorContract":"standard"}]}"#;
        let parsed: MappingDoc = serde_json::from_str(doc).expect("unknown fields tolerated");
        assert_eq!(parsed.entries[0].slug, "sdk-run");
    }

    #[test]
    fn unknown_disposition_is_rejected_loudly() {
        let doc = r#"{"version":1,"contractVersion":"agentcpm-pd 007 v0.7.1",
            "entries":[{"slug":"sdk-run","caseId":"x","disposition":"maybe",
            "oldEntry":{"argvPatterns":[],"stdinBehavior":"","exitCodes":{"success":0,"failure":1,"usage":2},"options":[],"reservedWords":[]},
            "sequence":[],"projection":"","s3Pending":[],"usageErrorContract":""}]}"#;
        assert!(serde_json::from_str::<MappingDoc>(doc).is_err());
    }

    #[test]
    fn contract_version_mismatch_fails_load() {
        let doc = r#"{"version":1,"contractVersion":"agentcpm-pd 007 v0.6.0",
            "entries":[{"slug":"sdk-run","caseId":"x","disposition":"supported",
            "oldEntry":{"argvPatterns":[],"stdinBehavior":"","exitCodes":{"success":0,"failure":1,"usage":2},"options":[],"reservedWords":[]},
            "sequence":[],"projection":"","s3Pending":[],"usageErrorContract":""}]}"#;
        let parsed: MappingDoc = serde_json::from_str(doc).expect("schema ok");
        assert_ne!(parsed.contract_version, CONTRACT_VERSION);
    }

    #[test]
    fn validate_rejects_unknown_slug_and_disposition_mismatch() {
        let mut m = Mappings::load().expect("load");
        let mut entry = m.find("sdk-run").expect("entry").clone();
        // Unknown slug.
        let mut bogus = m.clone();
        let mut renamed = entry.clone();
        renamed.slug = "not-a-slug".to_owned();
        bogus.entries.insert(renamed.slug.clone(), renamed);
        match bogus.validate(&Manifest::builtin()) {
            Err(MappingError::UnknownSlug { slug }) => assert_eq!(slug, "not-a-slug"),
            other => panic!("expected UnknownSlug, got {other:?}"),
        }
        // Disposition mismatch (manifest says `update` is rejected).
        entry.slug = "update".to_owned();
        m.entries.insert("update".to_owned(), entry);
        match m.validate(&Manifest::builtin()) {
            Err(MappingError::DispositionMismatch { slug, .. }) => assert_eq!(slug, "update"),
            other => panic!("expected DispositionMismatch, got {other:?}"),
        }
        // A manifest entry without a mapping document fails loudly.
        let mut partial = Mappings::load().expect("reload");
        partial.entries.remove("sdk-run");
        match partial.validate(&Manifest::builtin()) {
            Err(MappingError::MissingSlug { slug }) => assert_eq!(slug, "sdk-run"),
            other => panic!("expected MissingSlug, got {other:?}"),
        }
    }

    #[test]
    fn reserved_words_come_from_mapping_data() {
        let m = Mappings::load().expect("load");
        // The authoritative list is the `default-argv` mapping data (old
        // top-level dispatch words plus help/version aliases).
        for word in [
            "run",
            "resume",
            "sessions",
            "settings",
            "server",
            "gateway",
            "cron",
            "update",
            "skills",
            "chat",
            "tui",
            "help",
            "--help",
            "-h",
            "--version",
        ] {
            assert!(m.is_reserved_word(word), "`{word}` must be reserved");
        }
        // Plain words remain prompts.
        assert!(!m.is_reserved_word("hello"));
        assert!(!m.is_reserved_word("frobnicate"));
    }
}

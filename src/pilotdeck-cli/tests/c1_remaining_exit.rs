//! Formal CLI regression for migration ordering under Node's POSIX locales.
//! Measured from cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85 with Node v22.22.0:
//! `src/cli/pilotdeck.ts skills migrate --source <fixture> --from cc --json`.
//! The baseline's migrateSkills.ts:269 sorts every discovered basename with
//! localeCompare before validating slugs, so invalid Unicode names still make
//! this order observable in the report.
use std::{fs, process::Command};

#[test]
fn skills_migrate_preserves_node_posix_locale_prefixes() {
    let root = std::env::temp_dir().join(format!("pdrs-c1-skills-locale-{}", std::process::id()));
    let source = root.join("source");
    for slug in ["z", "ä", "côte", "coté"] {
        let path = source.join(slug);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("SKILL.md"),
            "---\nname: sample\ndescription: A sample skill for testing local migration.\n---\nBody.\n",
        )
        .unwrap();
    }
    for (locale, expected) in [
        ("C", ["ä", "coté", "côte", "z"]),
        ("sv_SE_a", ["coté", "côte", "z", "ä"]),
        ("fr_CA_bad", ["ä", "côte", "coté", "z"]),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_pilotdeck"))
            .env_clear()
            .env("HOME", root.join("home"))
            .env("PILOT_HOME", root.join("pilot"))
            .env("LANG", locale)
            .env("LC_ALL", locale)
            .current_dir(&root)
            .args(["skills", "migrate", "--source"])
            .arg(&source)
            .args(["--from", "cc", "--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{locale}: {output:?}");
        assert!(output.stderr.is_empty(), "{locale}: {output:?}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let custom: Vec<_> = report["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["sourceKind"] == "custom")
            .collect();
        let slugs: Vec<_> = custom
            .iter()
            .map(|item| item["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, expected, "{locale}");
        for item in custom {
            assert_eq!(
                item["status"],
                if item["slug"] == "z" {
                    "would_migrate"
                } else {
                    "error"
                },
                "{locale}: {item}"
            );
        }
        assert!(!root.join("pilot").exists(), "dry-run must not write");
    }
    fs::remove_dir_all(root).unwrap();
}

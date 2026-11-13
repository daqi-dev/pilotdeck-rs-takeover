//! Process-level config failures, including the implemented StaffDeck binding
//! transform. Baseline cc7c4e6 CLI evidence: B1/failure-probes/baseline.json.
use std::{fs, process::Command};

#[test]
fn staffdeck_binding_and_missing_provider_report_config_errors() {
    let root = std::env::temp_dir().join(format!("pdrs-d1-config-exit-{}", std::process::id()));
    fs::create_dir_all(root.join("project")).unwrap();
    fs::create_dir_all(root.join("home")).unwrap();
    let config = root.join("pilotdeck.yaml");
    for (binding, yaml, code, expected) in [
        ("true", "a: [\n", 1, "Failed to parse default config YAML."),
        (
            "true",
            "schemaVersion: 1\n",
            1,
            "Config must contain an agent section.",
        ),
        (
            "false",
            "schemaVersion: 1\nagent: {model: constructor/m}\nmodel: {}\n",
            1,
            "Model config must contain at least one provider.",
        ),
    ] {
        fs::write(&config, yaml).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_pilotdeck"))
            .current_dir(root.join("project"))
            .env_clear()
            .env("HOME", root.join("home"))
            .env("PILOT_HOME", root.join("home"))
            .env("PILOTDECK_CONFIG_PATH", &config)
            .env("PILOTDECK_BUNDLED_SKILLS_DIR", root.join("missing-skills"))
            .env("STAFFDECK_PROJECT_BINDING_ENABLED", binding)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .ends_with(&format!("{expected}\n"))
        );
    }
    fs::remove_dir_all(root).unwrap();
}

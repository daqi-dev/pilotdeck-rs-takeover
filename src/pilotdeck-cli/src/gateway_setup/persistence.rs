//! `gatewaySetup.ts` replaces one adapter in the parsed value and serializes
//! the entire document. Existing comments are intentionally not retained.
use super::{Failure, JsObject, JsString, JsValue, Paths, WECOM_DEFAULT_WS_URL, load_yaml_config};
use pilotdeck_gateway::channel::weixin::WeixinLoginResult;
use pilotdeck_yaml::stringify::{EditableDocument, StringifyOptions};
use std::path::Path;

pub(super) struct WeComConfig {
    pub bot_id: String,
    pub secret: String,
    pub websocket_url: String,
    pub dm_policy: String,
    pub allow_from: Vec<String>,
    pub group_policy: String,
    pub group_allow_from: Vec<String>,
    pub groups: Option<JsValue>,
}

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsValue {
    let mut object = JsObject::new();
    for (key, value) in entries {
        object.insert(key.into(), value);
    }
    JsValue::Object(object)
}

fn replace_adapter(paths: &Paths, name: &str, adapter: JsValue) -> Result<(), Failure> {
    let current = load_yaml_config(paths).unwrap_or(JsValue::Null);
    let mut value = if matches!(current, JsValue::Null | JsValue::Undefined) {
        JsValue::Object(JsObject::new())
    } else {
        current
    };
    match &mut value {
        JsValue::Object(config) => {
            let old = config
                .get("adapters")
                .cloned()
                .unwrap_or(JsValue::Undefined);
            let mut adapters = if super::truthy(Some(&old)) {
                old
            } else {
                JsValue::Object(JsObject::new())
            };
            match &mut adapters {
                JsValue::Object(object) => {
                    object.insert(name.into(), adapter);
                }
                // JavaScript permits named properties on arrays, but yaml.stringify
                // serializes only their indexed values.
                JsValue::Array(_) => (),
                other => return Err(property_error(name, other)),
            }
            config.insert("adapters".into(), adapters);
        }
        JsValue::Array(_) => (),
        other => return Err(property_error("adapters", other)),
    }
    let document =
        pilotdeck_yaml::parse_document(&JsString::from(""), pilotdeck_yaml::Options::default());
    let mut document = EditableDocument::new(document);
    document.set_in(&[], &value).map_err(yaml_error)?;
    let yaml = document
        .to_yaml_string_with_options(StringifyOptions {
            line_width: 0,
            single_quote: Some(false),
        })
        .map_err(yaml_error)?;
    write_file(&paths.yaml, &yaml)
}

fn property_error(name: &str, value: &JsValue) -> Failure {
    let kind = match value {
        JsValue::String(_) => "string",
        JsValue::Bool(_) => "boolean",
        JsValue::Number(_) => "number",
        _ => "object",
    };
    Failure::Error(format!(
        "Cannot create property '{name}' on {kind} '{}'",
        super::js_to_string(value)
    ))
}

fn yaml_error(error: pilotdeck_yaml::stringify::StringifyError) -> Failure {
    use pilotdeck_yaml::stringify::StringifyError;
    match error {
        StringifyError::Error(message) | StringifyError::Unsupported(message) => {
            Failure::Error(message)
        }
    }
}

fn write_file(path: &Path, contents: &str) -> Result<(), Failure> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| io_error(error, "mkdir", parent))?;
    }
    std::fs::write(path, contents).map_err(|error| io_error(error, "open", path))
}

fn io_error(error: std::io::Error, operation: &str, path: &Path) -> Failure {
    // Node's fs error message is the thrown Error displayed by the CLI.
    let (code, description) = match error.kind() {
        std::io::ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        std::io::ErrorKind::PermissionDenied => ("EACCES", "permission denied"),
        std::io::ErrorKind::AlreadyExists => ("EEXIST", "file already exists"),
        std::io::ErrorKind::NotADirectory => ("ENOTDIR", "not a directory"),
        std::io::ErrorKind::IsADirectory => ("EISDIR", "illegal operation on a directory"),
        _ => return Failure::Error(error.to_string()),
    };
    Failure::Error(format!(
        "{code}: {description}, {operation} '{}'",
        path.display()
    ))
}

pub(super) fn write_feishu(
    paths: &Paths,
    app_id: &str,
    secret: &str,
    domain: &str,
) -> Result<(), Failure> {
    replace_adapter(
        paths,
        "feishu",
        object([
            ("enabled", JsValue::Bool(true)),
            ("appId", app_id.into()),
            ("appSecret", secret.into()),
            ("connectionMode", "stream".into()),
            ("domainName", domain.into()),
        ]),
    )
}

pub(super) fn write_wecom(paths: &Paths, cfg: WeComConfig) -> Result<(), Failure> {
    let mut extra = JsObject::new();
    extra.insert("secret".into(), JsValue::String(cfg.secret.into()));
    extra.insert(
        "websocket_url".into(),
        if cfg.websocket_url.is_empty() {
            WECOM_DEFAULT_WS_URL.into()
        } else {
            JsValue::String(cfg.websocket_url.into())
        },
    );
    extra.insert("dm_policy".into(), JsValue::String(cfg.dm_policy.into()));
    extra.insert(
        "group_policy".into(),
        JsValue::String(cfg.group_policy.into()),
    );
    for (key, values) in [
        ("allow_from", cfg.allow_from),
        ("group_allow_from", cfg.group_allow_from),
    ] {
        if !values.is_empty() {
            extra.insert(
                key.into(),
                JsValue::Array(
                    values
                        .into_iter()
                        .map(|value| JsValue::String(value.into()))
                        .collect(),
                ),
            );
        }
    }
    if let Some(groups) = cfg.groups {
        extra.insert("groups".into(), groups);
    }
    replace_adapter(
        paths,
        "wecom",
        object([
            ("enabled", JsValue::Bool(true)),
            ("token", JsValue::String(cfg.bot_id.into())),
            ("extra", JsValue::Object(extra)),
        ]),
    )
}

pub(super) fn enable_weixin(paths: &Paths) -> Result<(), Failure> {
    replace_adapter(paths, "weixin", object([("enabled", JsValue::Bool(true))]))
}

pub(super) fn save_weixin_credentials(
    paths: &Paths,
    creds: &WeixinLoginResult,
) -> Result<(), Failure> {
    let value = object([
        ("baseUrl", creds.base_url.as_str().into()),
        ("botToken", creds.bot_token.as_str().into()),
        ("accountId", creds.account_id.as_str().into()),
    ]);
    let json = pilotdeck_protocol::stringify_json(&value)
        .map_err(|error| Failure::Error(error.to_string()))?
        .unwrap();
    // Correct the baseline's mkdir(HOME/.pilotdeck) / write(PILOT_HOME/...) mismatch:
    // create the directory that actually receives the configured credentials.
    write_file(&paths.weixin_credentials, &crate::json_pretty(&json))
}

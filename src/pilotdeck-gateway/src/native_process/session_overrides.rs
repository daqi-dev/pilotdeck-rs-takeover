//! #1864: the per-gateway session selection store the three selector
//! methods write and the next session composes from — the native
//! counterpart of the TS registry maps (`sessionOverrides`,
//! `sdkThinkingOverrides`, `sdkSessionConfigs.outputStyle`,
//! createLocalGateway.ts:622-744). Keyed by the session key itself
//! (`JsString` hashes its UTF-16 units, so a lone surrogate never folds
//! onto U+FFFD and shares a slot with a different key); presence is the
//! override, so a stored JS `null` thinking value is a value (TS deletes
//! only on `undefined`, ProjectRuntimeRegistry.ts:906-912).
use pilotdeck_protocol::{JsString, JsValue};
use std::collections::HashMap;

#[derive(Default)]
struct Inner {
    permission_modes: HashMap<JsString, JsString>,
    thinking: HashMap<JsString, JsValue>,
    /// `sdkSessionConfigs`: the session's whole SDK config. The output-style
    /// selection is its `outputStyle` field, as in the baseline, so a later
    /// `sdkSessionConfig` replaces a selection made by `set_output_style`.
    sdk_configs: HashMap<JsString, JsValue>,
    /// `sdkSessionConfigs` membership with its `sdkSessionProjects` root:
    /// the sessions `reloadOutputStylesForSdk` closes
    /// (ProjectRuntimeRegistry.ts:1111-1121).
    sdk_config_projects: HashMap<JsString, std::path::PathBuf>,
    /// `GatewaySessionPermissionModeRegistry`: the live mode a session's next
    /// turn starts in, and the mode it had before entering plan mode.
    live_modes: HashMap<JsString, LiveMode>,
}

struct LiveMode {
    current: JsString,
    base_before_plan: Option<JsString>,
}

/// The minimal equivalent registry #1839 allows: one shared instance per
/// gateway process (created in `assembly`, handed to both `Requests` and
/// the session factory's prepare closure).
#[derive(Default)]
pub struct SessionOverrides {
    inner: std::sync::Mutex<Inner>,
}

impl SessionOverrides {
    pub fn set_permission_mode(&self, key: &JsString, mode: JsString) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .permission_modes
            .insert(key.clone(), mode);
    }

    pub fn permission_mode(&self, key: &JsString) -> Option<JsString> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .permission_modes
            .get(key)
            .cloned()
    }

    /// `permissionModes.set`: an explicit selection forgets the pre-plan mode.
    pub fn set_live_mode(&self, key: &JsString, mode: JsString) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live_modes
            .insert(
                key.clone(),
                LiveMode {
                    current: mode,
                    base_before_plan: None,
                },
            );
    }

    /// `permissionModes.transition`: entering plan mode remembers the mode to
    /// return to; any other mode replaces the entry.
    pub fn transition_live_mode(
        &self,
        key: &JsString,
        mode: JsString,
        fallback_base: Option<JsString>,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let base_before_plan = if mode.equals_str("plan") {
            let previous = inner.live_modes.get(key);
            previous
                .and_then(|previous| previous.base_before_plan.clone())
                .or_else(|| previous.map(|previous| previous.current.clone()))
                .or(fallback_base)
        } else {
            None
        };
        inner.live_modes.insert(
            key.clone(),
            LiveMode {
                current: mode,
                base_before_plan,
            },
        );
    }

    /// `permissionModes.get`.
    pub fn live_mode(&self, key: &JsString) -> Option<JsString> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live_modes
            .get(key)
            .map(|mode| mode.current.clone())
    }

    /// `permissionModes.getBase`.
    pub fn live_base_mode(&self, key: &JsString) -> Option<JsString> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live_modes
            .get(key)
            .and_then(|mode| mode.base_before_plan.clone())
    }

    /// `permissionModes.clear`.
    pub fn clear_live_mode(&self, key: &JsString) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live_modes
            .remove(key);
    }

    /// `applyFlagSettingsForSdk` clearing `permissions.defaultMode`
    /// (ProjectRuntimeRegistry.ts:935-941): drop only the stored mode.
    pub fn clear_permission_mode(&self, key: &JsString) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .permission_modes
            .remove(key);
    }

    pub fn set_thinking(&self, key: &JsString, thinking: JsValue) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .thinking
            .insert(key.clone(), thinking);
    }

    pub fn clear_thinking(&self, key: &JsString) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .thinking
            .remove(key);
    }

    pub fn thinking(&self, key: &JsString) -> Option<JsValue> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .thinking
            .get(key)
            .cloned()
    }

    /// `setOutputStyleForSdk`: `{ ...previous, outputStyle: name }`.
    pub fn set_output_style(&self, key: &JsString, name: JsString) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = inner
            .sdk_configs
            .get(key)
            .and_then(JsValue::as_object)
            .cloned()
            .unwrap_or_default();
        next.insert("outputStyle".into(), JsValue::String(name));
        inner.sdk_configs.insert(key.clone(), JsValue::Object(next));
    }

    /// `setOutputStyleForSdk(null)`: `{ ...previous }` without `outputStyle`.
    pub fn clear_output_style(&self, key: &JsString) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = pilotdeck_protocol::JsObject::new();
        if let Some(previous) = inner.sdk_configs.get(key).and_then(JsValue::as_object) {
            for (name, value) in previous.entries() {
                if !name.equals_str("outputStyle") {
                    next.insert(name.clone(), value.clone());
                }
            }
        }
        inner.sdk_configs.insert(key.clone(), JsValue::Object(next));
    }

    /// `sdkSessionConfigs.get(key)`.
    pub fn sdk_config(&self, key: &JsString) -> Option<JsValue> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sdk_configs
            .get(key)
            .cloned()
    }

    /// `sdkSessionConfigs.set(key, config)`.
    pub fn set_sdk_config(&self, key: &JsString, config: JsValue) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sdk_configs
            .insert(key.clone(), config);
    }

    /// `sdkSessionConfigs.set(key, ...)` + `sdkSessionProjects.set(key, root)`.
    pub fn mark_sdk_config(&self, key: &JsString, project_root: std::path::PathBuf) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sdk_config_projects
            .insert(key.clone(), project_root);
    }

    /// The sessions holding an SDK session config, with their project roots.
    pub fn sdk_config_sessions(&self) -> Vec<(JsString, std::path::PathBuf)> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sdk_config_projects
            .iter()
            .map(|(key, root)| (key.clone(), root.clone()))
            .collect()
    }

    pub fn output_style(&self, key: &JsString) -> Option<JsString> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sdk_configs
            .get(key)
            .and_then(|config| config.get("outputStyle"))
            .and_then(JsValue::as_string)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negative_assert_eq;

    /// `GatewaySessionPermissionModeRegistry` (cc7c4e6): `set` drops the
    /// pre-plan mode, `transition` into plan keeps the first one it saw, and
    /// leaving plan forgets it.
    #[test]
    fn live_mode_keeps_the_mode_before_plan_until_plan_is_left() {
        let store = SessionOverrides::default();
        let key: JsString = "s".into();
        assert_eq!(store.live_mode(&key), None);
        store.transition_live_mode(&key, "plan".into(), Some("default".into()));
        assert_eq!(store.live_mode(&key), Some("plan".into()));
        assert_eq!(store.live_base_mode(&key), Some("default".into()));
        store.set_live_mode(&key, "bypassPermissions".into());
        assert_eq!(store.live_base_mode(&key), None);
        store.transition_live_mode(&key, "plan".into(), Some("default".into()));
        assert_eq!(store.live_base_mode(&key), Some("bypassPermissions".into()));
        store.transition_live_mode(&key, "plan".into(), None);
        assert_eq!(store.live_base_mode(&key), Some("bypassPermissions".into()));
        store.transition_live_mode(&key, "bypassPermissions".into(), None);
        assert_eq!(store.live_mode(&key), Some("bypassPermissions".into()));
        assert_eq!(store.live_base_mode(&key), None);
        store.clear_live_mode(&key);
        assert_eq!(store.live_mode(&key), None);
    }

    /// #1864 fix-r1: the store keys on the raw `JsString`, so a session key
    /// carrying a lone surrogate and the key that holds the U+FFFD it would
    /// lossily fold onto keep separate slots (the pre-fix
    /// `String::from_utf16_lossy` keying made both land in one).
    #[test]
    fn lone_surrogate_keys_do_not_fold_onto_replacement_chars() {
        let store = SessionOverrides::default();
        let surrogate = JsString::from_utf16(vec![0xD800]);
        let replacement = JsString::from_utf16(vec![0xFFFD]);
        store.set_permission_mode(&surrogate, "plan".into());
        store.set_permission_mode(&replacement, "bypassPermissions".into());
        negative_assert_eq!(
            store.permission_mode(&surrogate).as_ref(),
            Some(&JsString::from("plan")),
            "#1864 the lone-surrogate key keeps its own slot"
        );
        negative_assert_eq!(
            store.permission_mode(&replacement).as_ref(),
            Some(&JsString::from("bypassPermissions")),
            "#1864 the replacement-character key keeps its own slot"
        );
    }
}

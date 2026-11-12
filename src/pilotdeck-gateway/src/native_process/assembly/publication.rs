//! One atomic application publication; configured absence never falls back to
//! a manager from an earlier generation.
use crate::{
    runtime::{
        always_on::{AlwaysOnManager, StandaloneAlwaysOn},
        cron::CronManager,
    },
    switch::CronStopHandle,
};
use pilotdeck_protocol::JsString;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) struct SubsystemSnapshot {
    pub cron: Option<Arc<CronManager>>,
    pub always_on: Option<Arc<AlwaysOnManager>>,
    pub standalone: Option<Arc<StandaloneAlwaysOn>>,
    pub cron_stop: Option<Arc<CronStopHandle>>,
    pub extra_tools: Vec<JsString>,
}
#[derive(Default)]
pub(crate) struct SubsystemPublication(Mutex<Option<SubsystemSnapshot>>);
impl SubsystemPublication {
    pub fn publish(&self, snapshot: SubsystemSnapshot) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
    }
    pub fn snapshot(&self) -> Option<SubsystemSnapshot> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl SubsystemSnapshot {
    pub fn matches_managers(
        &self,
        cron: Option<&Arc<CronManager>>,
        always_on: Option<&Arc<AlwaysOnManager>>,
    ) -> bool {
        fn same<T>(left: Option<&Arc<T>>, right: Option<&Arc<T>>) -> bool {
            match (left, right) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
        }
        same(self.cron.as_ref(), cron) && same(self.always_on.as_ref(), always_on)
    }
}

#[cfg(test)]
mod tests {
    use super::super::automation::CronFactoryInput;
    use super::*;
    #[tokio::test]
    async fn production_publication_never_falls_back_to_stale_fixture_controls() {
        let (_scratch, mut requests) = crate::native_process::management_tests::setup();
        let factory = super::super::cron::native_factory(super::super::cron::NativeCronHooks {
            session_overrides: Arc::default(),
            on_turn_event: None,
            on_result_delivery: None,
        });
        let provider = factory(CronFactoryInput {
            section: crate::metadata::object([(
                "enabled",
                pilotdeck_protocol::JsValue::Bool(false),
            )]),
            pilot_home: requests.options.home.clone(),
        })
        .unwrap();
        let old = provider.cron_manager().unwrap();
        requests.cron = Some(old.clone());
        let publication = Arc::new(SubsystemPublication::default());
        requests.subsystems = Some(publication.clone());
        assert!(requests.current_cron().is_none());
        let empty = SubsystemSnapshot {
            cron: None,
            always_on: None,
            standalone: None,
            cron_stop: None,
            extra_tools: Vec::new(),
        };
        publication.publish(empty.clone());
        assert!(requests.current_cron().is_none());
        let provider = factory(CronFactoryInput {
            section: crate::metadata::object([(
                "enabled",
                pilotdeck_protocol::JsValue::Bool(false),
            )]),
            pilot_home: requests.options.home.clone(),
        })
        .unwrap();
        let current = provider.cron_manager().unwrap();
        publication.publish(SubsystemSnapshot {
            cron: Some(current.clone()),
            ..empty.clone()
        });
        assert!(Arc::ptr_eq(&requests.current_cron().unwrap(), &current));
        assert!(!Arc::ptr_eq(&requests.current_cron().unwrap(), &old));
        publication.publish(empty);
        assert!(requests.current_cron().is_none());
    }
}

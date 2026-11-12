//! Process-local bounded replay. This owns wire retention, never host Session
//! durability or permission to repeat an accepted execution.
use pilotdeck_protocol::{JsString, JsValue, stringify_json};
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Binding {
    pub module_instance_id: JsString,
    pub connection_generation: JsString,
}
#[derive(Clone, Debug)]
pub struct ReplayLimits {
    pub max_events_per_stream: usize,
    pub max_bytes_per_stream: usize,
    pub terminal_ttl: Duration,
}
impl Default for ReplayLimits {
    fn default() -> Self {
        Self {
            max_events_per_stream: 1024,
            max_bytes_per_stream: 1_048_576,
            terminal_ttl: Duration::from_secs(30),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReplayError {
    BindingMismatch,
    CursorExpired,
    InvalidAckCursor,
}
impl ReplayError {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::BindingMismatch => "BINDING_MISMATCH",
            Self::CursorExpired => "CURSOR_EXPIRED",
            Self::InvalidAckCursor => "INVALID_ACK_CURSOR",
        }
    }
}
pub(super) struct Replay {
    pub events: Vec<JsValue>,
    pub through: f64,
}
struct Event {
    value: JsValue,
    sequence: f64,
    bytes: usize,
}
struct Stream {
    binding: Binding,
    events: VecDeque<Event>,
    bytes: usize,
    last: f64,
    truncated: f64,
    terminal: bool,
    expires: Option<Instant>,
}
pub(super) struct ReplayStore {
    limits: ReplayLimits,
    streams: HashMap<JsString, Stream>,
}
impl ReplayStore {
    pub(super) fn new(limits: ReplayLimits) -> Result<Self, &'static str> {
        if limits.max_events_per_stream == 0 {
            return Err("maxEventsPerStream must be a positive integer.");
        }
        if limits.max_bytes_per_stream == 0 {
            return Err("maxBytesPerStream must be a positive integer.");
        }
        if limits.terminal_ttl.is_zero() {
            return Err("ttlMs must be a positive integer.");
        }
        Ok(Self {
            limits,
            streams: HashMap::new(),
        })
    }
    pub(super) fn start(
        &mut self,
        id: JsString,
        binding: Binding,
        now: Instant,
    ) -> Result<(), String> {
        self.prune(now);
        if self.streams.contains_key(&id) {
            return Err(format!(
                "Sidecar stream {} already exists.",
                String::from_utf16_lossy(id.units())
            ));
        }
        self.streams.insert(
            id,
            Stream {
                binding,
                events: VecDeque::new(),
                bytes: 0,
                last: -1.0,
                truncated: -1.0,
                terminal: false,
                expires: None,
            },
        );
        Ok(())
    }
    pub(super) fn append(
        &mut self,
        id: &JsString,
        event: JsValue,
        now: Instant,
    ) -> Result<(), String> {
        self.prune(now);
        let stream = self.streams.get_mut(id).ok_or_else(|| {
            format!(
                "Sidecar stream {} has expired.",
                String::from_utf16_lossy(id.units())
            )
        })?;
        let sequence = event
            .get("sequence")
            .and_then(JsValue::as_number)
            .unwrap_or(f64::NAN);
        if !sequence.is_finite() || sequence != stream.last + 1.0 {
            return Err("Sidecar stream sequence is not contiguous.".into());
        }
        if stream.terminal {
            return Err("Sidecar stream already reached a terminal event.".into());
        }
        let bytes = stringify_json(&event)
            .map_err(|error| error.to_string())?
            .ok_or("Stream event has no JSON representation.")?
            .len();
        let terminal = event.get("final") == Some(&JsValue::Bool(true));
        stream.events.push_back(Event {
            value: event,
            sequence,
            bytes,
        });
        stream.bytes = stream.bytes.saturating_add(bytes);
        stream.last = sequence;
        stream.terminal = terminal;
        if terminal {
            stream.expires = now.checked_add(self.limits.terminal_ttl);
        }
        while stream.events.len() > self.limits.max_events_per_stream
            || stream.bytes > self.limits.max_bytes_per_stream
        {
            if let Some(event) = stream.events.pop_front() {
                stream.bytes -= event.bytes;
                stream.truncated = event.sequence;
            } else {
                break;
            }
        }
        Ok(())
    }
    pub(super) fn resume(
        &mut self,
        id: &JsString,
        previous: &Binding,
        cursor: f64,
        now: Instant,
    ) -> Result<Replay, ReplayError> {
        self.prune(now);
        let stream = self.streams.get_mut(id).ok_or(ReplayError::CursorExpired)?;
        if &stream.binding != previous {
            return Err(ReplayError::BindingMismatch);
        }
        if !cursor.is_finite()
            || cursor.fract() != 0.0
            || cursor < -1.0
            || cursor > stream.last
            || cursor < stream.truncated
        {
            return Err(ReplayError::CursorExpired);
        }
        if stream.terminal {
            stream.expires = now.checked_add(self.limits.terminal_ttl);
        }
        Ok(Replay {
            events: stream
                .events
                .iter()
                .filter(|event| event.sequence > cursor)
                .map(|event| event.value.clone())
                .collect(),
            through: stream.last,
        })
    }
    pub(super) fn acknowledge(
        &mut self,
        id: &JsString,
        cursor: f64,
        now: Instant,
    ) -> Result<(), ReplayError> {
        self.prune(now);
        let stream = self.streams.get_mut(id).ok_or(ReplayError::CursorExpired)?;
        if !cursor.is_finite() || cursor.fract() != 0.0 || cursor < 0.0 || cursor > stream.last {
            return Err(ReplayError::InvalidAckCursor);
        }
        if cursor < stream.truncated {
            return Err(ReplayError::CursorExpired);
        }
        while stream
            .events
            .front()
            .is_some_and(|event| event.sequence <= cursor)
        {
            let event = stream.events.pop_front().unwrap();
            stream.bytes -= event.bytes;
            stream.truncated = event.sequence;
        }
        if stream.terminal {
            stream.expires = now.checked_add(self.limits.terminal_ttl);
        }
        Ok(())
    }
    pub(super) fn rebind(&mut self, id: &JsString, binding: Binding, now: Instant) -> bool {
        self.prune(now);
        let Some(stream) = self.streams.get_mut(id) else {
            return false;
        };
        stream.binding = binding;
        if stream.terminal {
            stream.expires = now.checked_add(self.limits.terminal_ttl);
        }
        true
    }
    pub(super) fn remove(&mut self, id: &JsString) {
        self.streams.remove(id);
    }
    pub(super) fn prune(&mut self, now: Instant) {
        self.streams
            .retain(|_, stream| stream.expires.is_none_or(|expires| expires > now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding(generation: &str) -> Binding {
        Binding {
            module_instance_id: "instance".into(),
            connection_generation: generation.into(),
        }
    }
    fn event(sequence: u32, terminal: bool) -> JsValue {
        pilotdeck_protocol::parse_json(&format!(r#"{{"kind":"event","streamId":"stream","sequence":{sequence},"final":{terminal},"payload":{{"text":"é"}}}}"#)).unwrap()
    }
    #[test]
    fn resume_ack_trim_rebind_and_terminal_expiry_follow_live_store() {
        let now = Instant::now();
        let mut store = ReplayStore::new(ReplayLimits {
            max_events_per_stream: 2,
            terminal_ttl: Duration::from_secs(3),
            ..Default::default()
        })
        .unwrap();
        let id = JsString::from("stream");
        store.start(id.clone(), binding("one"), now).unwrap();
        store.append(&id, event(0, false), now).unwrap();
        store.append(&id, event(1, false), now).unwrap();
        store.acknowledge(&id, 0.0, now).unwrap();
        assert_eq!(
            store.resume(&id, &binding("one"), -1.0, now).err(),
            Some(ReplayError::CursorExpired)
        );
        assert_eq!(
            store.acknowledge(&id, 2.0, now),
            Err(ReplayError::InvalidAckCursor)
        );
        let replay = store
            .resume(&id, &binding("one"), 0.0, now + Duration::from_secs(100))
            .unwrap();
        assert_eq!(
            replay.events,
            vec![event(1, false)],
            "active executions do not expire"
        );
        assert_eq!(replay.through, 1.0);
        assert!(store.rebind(&id, binding("two"), now));
        assert_eq!(
            store.resume(&id, &binding("one"), 1.0, now).err(),
            Some(ReplayError::BindingMismatch)
        );
        store.append(&id, event(2, true), now).unwrap();
        assert!(
            store
                .resume(&id, &binding("two"), 1.0, now + Duration::from_secs(2))
                .is_ok()
        );
        assert!(
            store
                .resume(&id, &binding("two"), 1.0, now + Duration::from_secs(4))
                .is_ok()
        );
        assert_eq!(
            store
                .resume(&id, &binding("two"), 1.0, now + Duration::from_secs(7))
                .err(),
            Some(ReplayError::CursorExpired)
        );
    }
    #[test]
    fn count_and_utf8_byte_caps_expire_only_lost_prefix_and_never_skip_sequence() {
        let now = Instant::now();
        let id = JsString::from("stream");
        for limits in [
            ReplayLimits {
                max_events_per_stream: 1,
                ..Default::default()
            },
            ReplayLimits {
                max_bytes_per_stream: stringify_json(&event(1, false)).unwrap().unwrap().len(),
                ..Default::default()
            },
        ] {
            let mut store = ReplayStore::new(limits).unwrap();
            store.start(id.clone(), binding("one"), now).unwrap();
            store.append(&id, event(0, false), now).unwrap();
            store.append(&id, event(1, false), now).unwrap();
            assert_eq!(
                store.resume(&id, &binding("one"), -1.0, now).err(),
                Some(ReplayError::CursorExpired)
            );
            assert_eq!(
                store.resume(&id, &binding("one"), 0.0, now).unwrap().events,
                vec![event(1, false)]
            );
            assert!(store.append(&id, event(3, false), now).is_err());
            assert!(store.start(id.clone(), binding("other"), now).is_err());
        }
    }
}

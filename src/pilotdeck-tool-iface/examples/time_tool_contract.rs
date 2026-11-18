//! Minimal `TimeToolContract` wiring: the embedding party supplies the trusted
//! read-only clock and timezone ports, and the adapter exposes the fixed
//! `get_current_time` tool spec.
//!
//! Run with `cargo run -p pilotdeck-tool-iface --example time_tool_contract`.
#![forbid(unsafe_code)]

use std::sync::Arc;

use pilotdeck_tool_iface::{Clock, TimeToolContract, ZoneError, ZoneResolver};
use pilotdeck_transport::system_clock::ZonedDateTimeParts;

struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> Result<f64, String> {
        Ok(0.0)
    }
}

struct UtcZones;

impl ZoneResolver for UtcZones {
    fn host_timezone(&self) -> String {
        "UTC".into()
    }

    fn parts_at(&self, _timezone: &str, _epoch_ms: i64) -> Result<ZonedDateTimeParts, ZoneError> {
        Ok(ZonedDateTimeParts {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            weekday: 4,
            offset_seconds: 0,
        })
    }
}

fn main() {
    let tool = TimeToolContract::new(Arc::new(FixedClock), Arc::new(UtcZones));
    let spec = tool.spec();
    println!("{}: {}", spec.name, spec.description);
}

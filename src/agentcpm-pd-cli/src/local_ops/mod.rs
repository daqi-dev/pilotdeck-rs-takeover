//! Table-driven local operation dispatch (S3 substrate).
//!
//! `execution='local'` operations run in-process instead of over `/v1/ws`.
//! Each domain file registers its handlers (`reg.op("<op>", ...)`), and
//! `commands.rs::run_local_operation` becomes a table lookup: an
//! unregistered local operation answers `CAPABILITY_UNAVAILABLE` exactly
//! as before. The registered ids are also marked implemented in the
//! protocol crate's global set (`is_implemented`).
//!
//! Adding a local operation in S3 means touching this module list once (the
//! `pub mod` declaration plus the `register` call) and the new domain file.

pub mod input;
pub mod lifecycle;
pub mod schema;
pub mod targets;

use std::io::Write;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use serde_json::Value;

use crate::{GlobalOptions, OutputFormat};

/// One local operation: receives the resolved globals, the assembled input,
/// the unary output format, and the stdout sink; returns the process exit
/// code. The boxed future borrows the caller's sink.
pub type LocalOpFuture<'a> = Pin<Box<dyn Future<Output = i32> + 'a>>;

pub type LocalOpHandler =
    for<'a> fn(&'a GlobalOptions, &'a Value, OutputFormat, &'a mut dyn Write) -> LocalOpFuture<'a>;

pub struct LocalOpRegistry {
    handlers: HashMap<&'static str, LocalOpHandler>,
}

impl LocalOpRegistry {
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<LocalOpRegistry> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            let mut reg = Self {
                handlers: HashMap::new(),
            };
            schema::register(&mut reg);
            lifecycle::register(&mut reg);
            targets::register(&mut reg);
            input::register(&mut reg);
            reg.finish();
            reg
        })
    }

    /// Registers the local handler for `method_id`. A duplicate registration
    /// panics at startup — two domains claiming one operation is a wiring
    /// bug, not something to resolve silently.
    pub fn op(&mut self, method_id: &'static str, handler: LocalOpHandler) {
        if self.handlers.insert(method_id, handler).is_some() {
            panic!("duplicate local op registration for `{method_id}`");
        }
    }

    fn finish(&self) {
        let ids: Vec<&'static str> = self.handlers.keys().copied().collect();
        agentcpm_pd_protocol::registry::mark_implemented(&ids);
    }

    /// The local handler for `method_id`, if this build registered one.
    pub fn lookup(&self, method_id: &str) -> Option<LocalOpHandler> {
        self.handlers.get(method_id).copied()
    }
}

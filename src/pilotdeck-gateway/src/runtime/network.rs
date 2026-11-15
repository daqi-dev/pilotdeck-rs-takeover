//! Original network arbitration, shared with the model runtime.
//! Keep the Gateway paths and their existing visibility for all consumers.
pub use pilotdeck_model_runtime::network::{
    Cancellation, ExchangeFuture, UNDICI_BODY_TIMEOUT, UNDICI_BODY_TIMEOUT_MS, node_timer_delay_ms,
    node_timer_duration,
};
pub(crate) use pilotdeck_model_runtime::network::{
    Exchange, body_frame, body_frame_at, body_frame_with_abort, body_frame_with_prearmed_parent,
    delay, exchange, exchange_continuing, exchange_prearmed_continuing,
    exchange_with_prearmed_parent,
};

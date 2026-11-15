//! Shared Node-compatible ICU collation; same helper as Gateway project_files.
use icu_collator::CollatorBorrowed;
use icu_locale_core::Locale;
pub use pilotdeck_agent::collation::node_locale;
pub fn node_collator(locale: &Locale) -> Result<CollatorBorrowed<'static>, (&'static str, String)> {
    pilotdeck_agent::collation::build_node_collator(locale, true).ok_or_else(|| {
        (
            "gateway_request_failed",
            "Collation data unavailable.".into(),
        )
    })
}

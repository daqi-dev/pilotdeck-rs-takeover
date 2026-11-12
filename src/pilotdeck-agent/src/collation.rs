//! Node `String.prototype.localeCompare` with no locale argument: the process
//! default locale (Node v22.22.0 / ICU POSIX default, measured in the gateway's
//! node-locale/oracle.json) and the pinned ICU collator. Shared by the gateway
//! and the agent loop (`CachePlan.ts` sorts tools with `localeCompare` in
//! whichever process assembles the request).
use icu_collator::{Collator, CollatorBorrowed, CollatorPreferences};
use icu_locale_core::Locale;

/// The collator `localeCompare` / `Intl.Collator` uses for `locale`.
///
/// Generated ICU 78.2 / CLDR 48 collation data uses the pinned Node oracle's
/// Unihan root, with matching tailorings. Unsupported collation extensions
/// retain Node's fallback to the language default, then root.
///
/// Two spellings of the same locale do not collate alike in Node, and the
/// difference is measured, not stylistic (v22.22.0 / ICU 78.2,
/// `src/pilotdeck-cli/tests/fixtures/skills-fr-ca-probe.json`):
///
/// * the **process default** (`localeCompare` with no locale argument, plain
///   `Intl.Collator()`) reports `resolvedOptions().locale == "fr-Latn-CA"`
///   under `LANG=fr_Latn_CA.UTF-8` yet still orders like `fr-CA`
///   (`coté > côte`), so a Latin script tag does **not** opt out;
/// * an **explicit** `new Intl.Collator("fr-Latn-CA")` orders like generic
///   `fr` (`coté < côte`), the value recorded in the gateway's
///   `tests/fixtures/native-collation/oracle.json`.
///
/// `process_default` selects the first spelling. `None` means no collation
/// data could be loaded at all.
pub fn build_node_collator(
    locale: &Locale,
    process_default: bool,
) -> Option<CollatorBorrowed<'static>> {
    let mut preferences: CollatorPreferences = locale.clone().into();
    let create = |preferences: CollatorPreferences| {
        Collator::try_new(preferences, Default::default()).map(|collator| {
            // The pinned baked metadata omits fr-CA's backward-secondary bit.
            // Restore the Node/ICU78 default; explicit collations (e.g. emoji)
            // retain their own metadata. See native-collation/oracle.json.
            let latin_or_none = |script: Option<icu_locale_core::subtags::Script>| {
                script.is_none_or(|script| script.to_string() == "Latn")
            };
            if locale.id.language.as_str() == "fr"
                && (process_default && latin_or_none(locale.id.script)
                    || !process_default && locale.id.script.is_none())
                && locale
                    .id
                    .region
                    .is_some_and(|region| region.as_str() == "CA")
                && preferences.collation_type.is_none()
            {
                collator.with_backward_second_level(true)
            } else {
                collator
            }
        })
    };
    create(preferences)
        .or_else(|_| {
            preferences.collation_type = None;
            create(preferences)
        })
        .or_else(|_| Collator::try_new(Default::default(), Default::default()))
        .ok()
}

pub fn node_locale(env: impl FnMut(&str) -> Option<String>) -> Locale {
    let value = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(env)
        .unwrap_or_else(|| "en-US".into());
    // Empty values deliberately win precedence: Node resolves them to und.
    let (base, modifier) = value.split_once('@').unwrap_or((&value, ""));
    let language = base.split('.').next().unwrap_or("").replace('_', "-");
    if language == "C" || language == "POSIX" {
        return "en-US".parse().expect("locale literal");
    }
    // ICU's POSIX parser keeps the valid prefix of malformed locale names;
    // a single-letter component is not a POSIX language/script/region/variant.
    let mut prefix = language
        .split('-')
        .take_while(|part| part.len() != 1)
        .collect::<Vec<_>>();
    let mut locale = loop {
        if prefix.is_empty() {
            break Locale::UNKNOWN;
        }
        if let Ok(locale) = prefix.join("-").parse::<Locale>() {
            break locale;
        }
        prefix.pop();
    };
    // Legacy POSIX modifiers become variants; ICU keyword assignments are
    // separate and are not a bare variant (e.g. @collation=phonebook).
    if !modifier.is_empty() && !modifier.contains('=') {
        let variant = format!("{locale}-{modifier}");
        locale = variant.parse().unwrap_or_else(|_| {
            format!("{locale}-x-lvariant-{modifier}")
                .parse()
                .unwrap_or(locale)
        });
    }
    locale
}

//! Named time zones resolved the way Node v22.22.0's `Intl.DateTimeFormat`
//! resolves an explicit `timeZone` option (the baseline's `get_current_time`
//! tool and every cron timezone check go through that constructor).
//!
//! Node carries its own ICU time-zone data and never reads the host's
//! `/usr/share/zoneinfo` for such a name, so neither the accepted names nor
//! their rules depend on the host. This module therefore answers from bundled
//! data only — the host tree is not consulted:
//!
//! * IANA tzdb names, canonical and `backward` links alike (`US/Pacific`,
//!   `Asia/Calcutta`, `GB`, `PRC`, `EST5EDT`, `Etc/GMT+5` …): TZif from the
//!   `jiff-tzdb` crate, tzdb release 2025b compiled by `zic` (see
//!   [`TZDB_VERSION`]). Ubuntu 24.04 moved those links into the optional
//!   `tzdata-legacy` package, which is why reading the host tree was not
//!   enough. `Factory` is in tzdb but ICU rejects it, so it is excluded.
//! * Identifiers ICU accepts that tzdb does not have: the Java-era
//!   three-letter IDs, two IDs tzdb has since dropped, and the thirteen
//!   `SystemV/*` zones. The alias targets and the `SystemV` rules below are
//!   measured, not copied: every name was run through Node v22.22.0 (ICU 78.2
//!   and ICU 77.1, identical results) for acceptance,
//!   `resolvedOptions().timeZone`, and the UTC offset at weekly samples
//!   1850–2100 plus hourly samples 2025–2027.
//!
//! Matching is ASCII case-insensitive like ICU. Bundled tzdb 2025b agrees
//! with ICU 78.2's tzdb 2025c at every sample above except America/Tijuana
//! and its links between 1953 and 1975 (the 2025c history correction).

/// The tzdb release the bundled TZif data was compiled from.
#[allow(dead_code)] // read by tests; flagged dead in the test-only #[path] include
pub const TZDB_VERSION: Option<&str> = jiff_tzdb::VERSION;

/// ICU-only aliases → the tzdb zone with the same rules. Keys are matched
/// ASCII case-insensitively.
const ICU_ONLY_ALIASES: &[(&str, &str)] = &[
    ("ACT", "Australia/Darwin"),
    ("AET", "Australia/Sydney"),
    ("AGT", "America/Argentina/Buenos_Aires"),
    ("ART", "Africa/Cairo"),
    ("AST", "America/Anchorage"),
    ("BET", "America/Sao_Paulo"),
    ("BST", "Asia/Dhaka"),
    ("CAT", "Africa/Maputo"),
    ("CNT", "America/St_Johns"),
    ("CST", "America/Chicago"),
    ("CTT", "Asia/Shanghai"),
    ("Canada/East-Saskatchewan", "America/Regina"),
    ("EAT", "Africa/Nairobi"),
    ("ECT", "Europe/Paris"),
    ("IET", "America/Indiana/Indianapolis"),
    ("IST", "Asia/Kolkata"),
    ("JST", "Asia/Tokyo"),
    ("MIT", "Pacific/Apia"),
    ("NET", "Asia/Yerevan"),
    ("NST", "Pacific/Auckland"),
    ("PLT", "Asia/Karachi"),
    ("PNT", "America/Phoenix"),
    ("PRT", "America/Puerto_Rico"),
    ("PST", "America/Los_Angeles"),
    ("SST", "Pacific/Guadalcanal"),
    ("US/Pacific-New", "America/Los_Angeles"),
    ("VST", "Asia/Ho_Chi_Minh"),
];

/// ICU's `SystemV/*` zones: a fixed standard offset (hours west of UTC) and,
/// for the `…DT` ones, one hour of daylight time from the last Sunday of
/// April 02:00 standard time to the last Sunday of October 02:00 daylight
/// time in every year from 1902 on, standard time before that; 1974 runs
/// from January 6 to the last Sunday of November and 1975 starts on the last
/// Sunday of February. No tzdb zone has these rules (the US moved its rule in
/// 1987 and 2007); every transition from 1902 to 2200 was measured in Node.
const SYSTEM_V: &[(&str, i32, bool)] = &[
    ("SystemV/AST4", 4, false),
    ("SystemV/AST4ADT", 4, true),
    ("SystemV/CST6", 6, false),
    ("SystemV/CST6CDT", 6, true),
    ("SystemV/EST5", 5, false),
    ("SystemV/EST5EDT", 5, true),
    ("SystemV/HST10", 10, false),
    ("SystemV/MST7", 7, false),
    ("SystemV/MST7MDT", 7, true),
    ("SystemV/PST8", 8, false),
    ("SystemV/PST8PDT", 8, true),
    ("SystemV/YST9", 9, false),
    ("SystemV/YST9YDT", 9, true),
];

enum Source {
    Tzif(&'static [u8]),
    SystemV { hours_west: i32, daylight: bool },
}

fn lookup(name: &str) -> Option<Source> {
    // ICU identifiers are ASCII; a non-ASCII spelling never names a zone.
    if !name.is_ascii() {
        return None;
    }
    if let Some(&(_, hours_west, daylight)) = SYSTEM_V
        .iter()
        .find(|(id, ..)| id.eq_ignore_ascii_case(name))
    {
        return Some(Source::SystemV {
            hours_west,
            daylight,
        });
    }
    let tzdb_name = ICU_ONLY_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or(name, |&(_, target)| target);
    let (canonical, bytes) = jiff_tzdb::get(tzdb_name)?;
    (canonical != "Factory").then_some(Source::Tzif(bytes))
}

/// Whether Node 22's `Intl.DateTimeFormat` accepts `name` as a named zone
/// (UTC-offset identifiers such as `+08:00` are not named zones).
#[allow(dead_code)] // called from the gateway; flagged dead in the test-only #[path] include
pub fn is_named_zone(name: &str) -> bool {
    lookup(name).is_some()
}

/// What kind of zone identifier `name` is when spelled exactly (ICU's host
/// time-zone detection matches identifiers case-sensitively, unlike the
/// `timeZone` option).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExactZone {
    /// A tzdb name or link (`US/Pacific`, `Asia/Calcutta`, `EST`, `UTC`).
    Tzdb,
    /// One of ICU's three-letter (or retired) identifiers with no tzdb entry
    /// (`PST`, `IST`, `US/Pacific-New`).
    IcuOnly,
    /// An ICU `SystemV/*` zone; `daylight` for the `…DT` ones.
    SystemV { daylight: bool },
}

/// `name`'s kind when it is spelled exactly as ICU spells the identifier.
#[allow(dead_code)] // used through system_clock; flagged dead in the test-only #[path] include
pub fn exact_zone(name: &str) -> Option<ExactZone> {
    if let Some(&(_, _, daylight)) = SYSTEM_V.iter().find(|(id, ..)| *id == name) {
        return Some(ExactZone::SystemV { daylight });
    }
    if ICU_ONLY_ALIASES.iter().any(|(alias, _)| *alias == name) {
        return Some(ExactZone::IcuOnly);
    }
    let (stored, _) = jiff_tzdb::get(name)?;
    (stored == name && stored != "Factory").then_some(ExactZone::Tzdb)
}

/// ICU's canonical identifier for a zone name it accepts (what
/// `resolvedOptions().timeZone` reports for it), exact spelling.
#[allow(dead_code)] // used through system_clock; flagged dead in the test-only #[path] include
pub fn icu_canonical(name: &str) -> &str {
    ICU_CANONICAL
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, canonical)| canonical)
}

/// The zone's standard (non-daylight) UTC offset in seconds going forward:
/// the recurring rule's, else the last standard local time type's.
#[allow(dead_code)] // used through system_clock; flagged dead in the test-only #[path] include
pub fn standard_offset(zone: &tz::TimeZone) -> i32 {
    use tz::timezone::TransitionRule;
    match zone.as_ref().extra_rule() {
        Some(TransitionRule::Fixed(rule)) => rule.ut_offset(),
        Some(TransitionRule::Alternate(rule)) => rule.std().ut_offset(),
        None => {
            let zone = zone.as_ref();
            let types = zone.local_time_types();
            let last = zone
                .transitions()
                .last()
                .map_or(0, |transition| transition.local_time_type_index());
            types
                .get(last)
                .filter(|kind| !kind.is_dst())
                .or_else(|| types.iter().rev().find(|kind| !kind.is_dst()))
                .map_or(0, |kind| kind.ut_offset())
        }
    }
}

/// The zone's rules, or `None` when ICU would reject the name.
pub fn named_zone(name: &str) -> Option<tz::TimeZone> {
    match lookup(name)? {
        Source::Tzif(bytes) => tz::TimeZone::from_tz_data(bytes).ok(),
        Source::SystemV {
            hours_west,
            daylight,
        } => system_v(hours_west, daylight),
    }
}

/// The years ICU lists explicitly; the recurring rule takes over afterwards.
const SYSTEM_V_LISTED_YEARS: std::ops::RangeInclusive<i32> = 1902..=1975;

fn system_v(hours_west: i32, daylight: bool) -> Option<tz::TimeZone> {
    use tz::timezone::{AlternateTime, MonthWeekDay, RuleDay, Transition, TransitionRule};
    let standard = tz::LocalTimeType::new(-hours_west * 3_600, false, None).ok()?;
    if !daylight {
        return tz::TimeZone::new(Vec::new(), vec![standard], Vec::new(), None).ok();
    }
    let summer = tz::LocalTimeType::new((1 - hours_west) * 3_600, true, None).ok()?;
    let last_sunday = |month| {
        MonthWeekDay::new(month, 5, 0)
            .ok()
            .map(RuleDay::MonthWeekDay)
    };
    let rule = AlternateTime::new(
        standard,
        summer,
        last_sunday(4)?,
        2 * 3_600,
        last_sunday(10)?,
        2 * 3_600,
    )
    .ok()?;
    // Both switches happen at 02:00 local time of the clock in force before
    // the switch: standard time for the start, daylight time for the end.
    let at = |day: i64, hours_west: i32| day * 86_400 + i64::from((2 + hours_west) * 3_600);
    let mut transitions = Vec::new();
    for year in SYSTEM_V_LISTED_YEARS {
        let start = match year {
            1974 => super::days_from_civil(1974, 1, 6),
            1975 => last_sunday_of(1975, 2),
            _ => last_sunday_of(year, 4),
        };
        let end = last_sunday_of(year, if year == 1974 { 11 } else { 10 });
        transitions.push(Transition::new(at(start, hours_west), 1));
        transitions.push(Transition::new(at(end, hours_west - 1), 0));
    }
    tz::TimeZone::new(
        transitions,
        vec![standard, summer],
        Vec::new(),
        Some(TransitionRule::Alternate(rule)),
    )
    .ok()
}

/// Days since 1970-01-01 of the last Sunday of `month` (1-based) in `year`.
fn last_sunday_of(year: i32, month: i32) -> i64 {
    let next_month = if month == 12 {
        super::days_from_civil(year + 1, 1, 1)
    } else {
        super::days_from_civil(year, month + 1, 1)
    };
    let last_day = next_month - 1;
    // 1970-01-01 was a Thursday; weekday 0 is Sunday.
    last_day - (last_day + 4).rem_euclid(7)
}

/// `resolvedOptions().timeZone` for every zone name whose ICU canonical
/// identifier is not the name itself, as Node v22.22.0 (ICU 77.1, tz 2025b)
/// reports it for `new Intl.DateTimeFormat("en-US", { timeZone: name })`.
/// Measured over every name [`named_zone`] accepts (the 597 tzdb 2025b names
/// plus the ICU-only identifiers above); the rest canonicalize to
/// themselves. CLDR keeps canonical identifiers stable across ICU releases
/// (it never renamed `Asia/Calcutta` to `Asia/Kolkata`), so the table
/// changes only when a later ICU learns new zones.
const ICU_CANONICAL: &[(&str, &str)] = &[
    ("ACT", "Australia/Darwin"),
    ("AET", "Australia/Sydney"),
    ("AGT", "America/Buenos_Aires"),
    ("ART", "Africa/Cairo"),
    ("AST", "America/Anchorage"),
    ("Africa/Asmara", "Africa/Asmera"),
    ("Africa/Timbuktu", "Africa/Bamako"),
    ("America/Argentina/Buenos_Aires", "America/Buenos_Aires"),
    ("America/Argentina/Catamarca", "America/Catamarca"),
    ("America/Argentina/ComodRivadavia", "America/Catamarca"),
    ("America/Argentina/Cordoba", "America/Cordoba"),
    ("America/Argentina/Jujuy", "America/Jujuy"),
    ("America/Argentina/Mendoza", "America/Mendoza"),
    ("America/Atikokan", "America/Coral_Harbour"),
    ("America/Atka", "America/Adak"),
    ("America/Ensenada", "America/Tijuana"),
    ("America/Fort_Wayne", "America/Indianapolis"),
    ("America/Indiana/Indianapolis", "America/Indianapolis"),
    ("America/Kentucky/Louisville", "America/Louisville"),
    ("America/Knox_IN", "America/Indiana/Knox"),
    ("America/Montreal", "America/Toronto"),
    ("America/Nipigon", "America/Toronto"),
    ("America/Nuuk", "America/Godthab"),
    ("America/Pangnirtung", "America/Iqaluit"),
    ("America/Porto_Acre", "America/Rio_Branco"),
    ("America/Rainy_River", "America/Winnipeg"),
    ("America/Rosario", "America/Cordoba"),
    ("America/Santa_Isabel", "America/Tijuana"),
    ("America/Shiprock", "America/Denver"),
    ("America/Thunder_Bay", "America/Toronto"),
    ("America/Virgin", "America/St_Thomas"),
    ("America/Yellowknife", "America/Edmonton"),
    ("Antarctica/South_Pole", "Antarctica/McMurdo"),
    ("Asia/Ashkhabad", "Asia/Ashgabat"),
    ("Asia/Choibalsan", "Asia/Ulaanbaatar"),
    ("Asia/Chongqing", "Asia/Shanghai"),
    ("Asia/Chungking", "Asia/Shanghai"),
    ("Asia/Dacca", "Asia/Dhaka"),
    ("Asia/Harbin", "Asia/Shanghai"),
    ("Asia/Ho_Chi_Minh", "Asia/Saigon"),
    ("Asia/Istanbul", "Europe/Istanbul"),
    ("Asia/Kashgar", "Asia/Urumqi"),
    ("Asia/Kathmandu", "Asia/Katmandu"),
    ("Asia/Kolkata", "Asia/Calcutta"),
    ("Asia/Macao", "Asia/Macau"),
    ("Asia/Tel_Aviv", "Asia/Jerusalem"),
    ("Asia/Thimbu", "Asia/Thimphu"),
    ("Asia/Ujung_Pandang", "Asia/Makassar"),
    ("Asia/Ulan_Bator", "Asia/Ulaanbaatar"),
    ("Asia/Yangon", "Asia/Rangoon"),
    ("Atlantic/Faroe", "Atlantic/Faeroe"),
    ("Atlantic/Jan_Mayen", "Arctic/Longyearbyen"),
    ("Australia/ACT", "Australia/Sydney"),
    ("Australia/Canberra", "Australia/Sydney"),
    ("Australia/Currie", "Australia/Hobart"),
    ("Australia/LHI", "Australia/Lord_Howe"),
    ("Australia/NSW", "Australia/Sydney"),
    ("Australia/North", "Australia/Darwin"),
    ("Australia/Queensland", "Australia/Brisbane"),
    ("Australia/South", "Australia/Adelaide"),
    ("Australia/Tasmania", "Australia/Hobart"),
    ("Australia/Victoria", "Australia/Melbourne"),
    ("Australia/West", "Australia/Perth"),
    ("Australia/Yancowinna", "Australia/Broken_Hill"),
    ("BET", "America/Sao_Paulo"),
    ("BST", "Asia/Dhaka"),
    ("Brazil/Acre", "America/Rio_Branco"),
    ("Brazil/DeNoronha", "America/Noronha"),
    ("Brazil/East", "America/Sao_Paulo"),
    ("Brazil/West", "America/Manaus"),
    ("CAT", "Africa/Maputo"),
    ("CET", "Europe/Brussels"),
    ("CNT", "America/St_Johns"),
    ("CST", "America/Chicago"),
    ("CST6CDT", "America/Chicago"),
    ("CTT", "Asia/Shanghai"),
    ("Canada/Atlantic", "America/Halifax"),
    ("Canada/Central", "America/Winnipeg"),
    ("Canada/East-Saskatchewan", "America/Regina"),
    ("Canada/Eastern", "America/Toronto"),
    ("Canada/Mountain", "America/Edmonton"),
    ("Canada/Newfoundland", "America/St_Johns"),
    ("Canada/Pacific", "America/Vancouver"),
    ("Canada/Saskatchewan", "America/Regina"),
    ("Canada/Yukon", "America/Whitehorse"),
    ("Chile/Continental", "America/Santiago"),
    ("Chile/EasterIsland", "Pacific/Easter"),
    ("Cuba", "America/Havana"),
    ("EAT", "Africa/Nairobi"),
    ("ECT", "Europe/Paris"),
    ("EET", "Europe/Athens"),
    ("EST", "America/Panama"),
    ("EST5EDT", "America/New_York"),
    ("Egypt", "Africa/Cairo"),
    ("Eire", "Europe/Dublin"),
    ("Etc/GMT", "UTC"),
    ("Etc/GMT+0", "UTC"),
    ("Etc/GMT-0", "UTC"),
    ("Etc/GMT0", "UTC"),
    ("Etc/Greenwich", "UTC"),
    ("Etc/UCT", "UTC"),
    ("Etc/UTC", "UTC"),
    ("Etc/Universal", "UTC"),
    ("Etc/Zulu", "UTC"),
    ("Europe/Belfast", "Europe/London"),
    ("Europe/Kyiv", "Europe/Kiev"),
    ("Europe/Nicosia", "Asia/Nicosia"),
    ("Europe/Tiraspol", "Europe/Chisinau"),
    ("Europe/Uzhgorod", "Europe/Kiev"),
    ("Europe/Zaporozhye", "Europe/Kiev"),
    ("GB", "Europe/London"),
    ("GB-Eire", "Europe/London"),
    ("GMT", "UTC"),
    ("GMT+0", "UTC"),
    ("GMT-0", "UTC"),
    ("GMT0", "UTC"),
    ("Greenwich", "UTC"),
    ("HST", "Pacific/Honolulu"),
    ("Hongkong", "Asia/Hong_Kong"),
    ("IET", "America/Indianapolis"),
    ("IST", "Asia/Calcutta"),
    ("Iceland", "Atlantic/Reykjavik"),
    ("Iran", "Asia/Tehran"),
    ("Israel", "Asia/Jerusalem"),
    ("JST", "Asia/Tokyo"),
    ("Jamaica", "America/Jamaica"),
    ("Japan", "Asia/Tokyo"),
    ("Kwajalein", "Pacific/Kwajalein"),
    ("Libya", "Africa/Tripoli"),
    ("MET", "Europe/Brussels"),
    ("MIT", "Pacific/Apia"),
    ("MST", "America/Phoenix"),
    ("MST7MDT", "America/Denver"),
    ("Mexico/BajaNorte", "America/Tijuana"),
    ("Mexico/BajaSur", "America/Mazatlan"),
    ("Mexico/General", "America/Mexico_City"),
    ("NET", "Asia/Yerevan"),
    ("NST", "Pacific/Auckland"),
    ("NZ", "Pacific/Auckland"),
    ("NZ-CHAT", "Pacific/Chatham"),
    ("Navajo", "America/Denver"),
    ("PLT", "Asia/Karachi"),
    ("PNT", "America/Phoenix"),
    ("PRC", "Asia/Shanghai"),
    ("PRT", "America/Puerto_Rico"),
    ("PST", "America/Los_Angeles"),
    ("PST8PDT", "America/Los_Angeles"),
    ("Pacific/Chuuk", "Pacific/Truk"),
    ("Pacific/Johnston", "Pacific/Honolulu"),
    ("Pacific/Kanton", "Pacific/Enderbury"),
    ("Pacific/Pohnpei", "Pacific/Ponape"),
    ("Pacific/Samoa", "Pacific/Pago_Pago"),
    ("Pacific/Yap", "Pacific/Truk"),
    ("Poland", "Europe/Warsaw"),
    ("Portugal", "Europe/Lisbon"),
    ("ROC", "Asia/Taipei"),
    ("ROK", "Asia/Seoul"),
    ("SST", "Pacific/Guadalcanal"),
    ("Singapore", "Asia/Singapore"),
    ("Turkey", "Europe/Istanbul"),
    ("UCT", "UTC"),
    ("US/Alaska", "America/Anchorage"),
    ("US/Aleutian", "America/Adak"),
    ("US/Arizona", "America/Phoenix"),
    ("US/Central", "America/Chicago"),
    ("US/East-Indiana", "America/Indianapolis"),
    ("US/Eastern", "America/New_York"),
    ("US/Hawaii", "Pacific/Honolulu"),
    ("US/Indiana-Starke", "America/Indiana/Knox"),
    ("US/Michigan", "America/Detroit"),
    ("US/Mountain", "America/Denver"),
    ("US/Pacific", "America/Los_Angeles"),
    ("US/Pacific-New", "America/Los_Angeles"),
    ("US/Samoa", "Pacific/Pago_Pago"),
    ("Universal", "UTC"),
    ("VST", "Asia/Saigon"),
    ("W-SU", "Europe/Moscow"),
    ("WET", "Europe/Lisbon"),
    ("Zulu", "UTC"),
];

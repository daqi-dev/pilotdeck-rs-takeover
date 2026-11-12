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

// SPDX-License-Identifier: GPL-3.0-only
//
// Locale helpers: honor the user's `LC_TIME` settings for the 12h/24h clock and
// localized day/month names, via POSIX `nl_langinfo`. Falls back to English and
// a 24-hour clock when the locale can't be read (e.g. a minimal sandbox).

use std::ffi::CStr;
use std::sync::OnceLock;

use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike};

/// Load the user's environment locale into the C library exactly once. C programs
/// start in the "C" locale, so without this `nl_langinfo` would ignore `LC_TIME`.
fn ensure_locale() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        // SAFETY: standard libc call; passing an empty string selects the
        // locale from the environment.
        unsafe {
            libc::setlocale(libc::LC_ALL, c"".as_ptr());
        }
    });
}

/// Read an `nl_langinfo` string item, returning `None` when empty/unavailable.
fn langinfo(item: libc::nl_item) -> Option<String> {
    ensure_locale();
    // SAFETY: `nl_langinfo` returns a pointer to a static, NUL-terminated string
    // owned by the C library; we copy it out immediately.
    unsafe {
        let ptr = libc::nl_langinfo(item);
        if ptr.is_null() {
            return None;
        }
        let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        (!s.is_empty()).then_some(s)
    }
}

/// The current local time-zone abbreviation (e.g. "PDT", "CET", "JST"), derived
/// generically from the system zone via `strftime("%Z")`. Empty if unavailable.
///
/// This is friendlier than chrono's `%Z` on `Local`, which falls back to a
/// numeric offset like "-07:00".
#[must_use]
pub fn timezone_abbrev() -> String {
    ensure_locale();
    // SAFETY: standard libc time calls; the abbreviation is copied out of the
    // stack buffer before returning.
    unsafe {
        // `localtime_r` is specified to behave as if `tzset()` had been called.
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&raw const t, &raw mut tm).is_null() {
            return String::new();
        }
        let mut buf = [0 as libc::c_char; 32];
        let n = libc::strftime(buf.as_mut_ptr(), buf.len(), c"%Z".as_ptr(), &raw const tm);
        if n == 0 {
            return String::new();
        }
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Whether the user's locale prefers a 24-hour clock.
#[must_use]
pub fn uses_24h_clock() -> bool {
    static CELL: OnceLock<bool> = OnceLock::new();
    *CELL.get_or_init(|| {
        // A locale's time format uses %I/%r/%p only for a 12-hour clock.
        let fmt = langinfo(libc::T_FMT).unwrap_or_default();
        !(fmt.contains("%I") || fmt.contains("%r") || fmt.contains("%p"))
    })
}

/// Localized abbreviated weekday name (e.g. "Mon", "lun.").
fn weekday_abbrev(date: NaiveDate) -> String {
    // ABDAY_1 is Sunday.
    let idx = i32::try_from(date.weekday().num_days_from_sunday()).unwrap_or(0);
    langinfo(libc::ABDAY_1 + idx).unwrap_or_else(|| date.format("%a").to_string())
}

/// Localized abbreviated month name (e.g. "Jun", "juin").
fn month_abbrev(date: NaiveDate) -> String {
    // ABMON_1 is January.
    let idx = i32::try_from(date.month0()).unwrap_or(0);
    langinfo(libc::ABMON_1 + idx).unwrap_or_else(|| date.format("%b").to_string())
}

fn to_12h(hour: u32) -> (u32, &'static str) {
    match hour % 24 {
        0 => (12, "AM"),
        h @ 1..=11 => (h, "AM"),
        12 => (12, "PM"),
        h => (h - 12, "PM"),
    }
}

/// Hour-of-day grid axis label, e.g. "9 AM" (12h) or "09" (24h).
#[must_use]
pub fn hour_axis_label(hour: u32) -> String {
    if uses_24h_clock() {
        format!("{:02}", hour % 24)
    } else {
        let (h, ap) = to_12h(hour);
        format!("{h} {ap}")
    }
}

/// Format a time honoring the locale clock, e.g. "9:00 AM" or "09:00".
#[must_use]
pub fn format_time(dt: &DateTime<Local>) -> String {
    let (h, m) = (dt.hour(), dt.minute());
    if uses_24h_clock() {
        format!("{h:02}:{m:02}")
    } else {
        let (h12, ap) = to_12h(h);
        format!("{h12}:{m:02} {ap}")
    }
}

/// Short localized day-column header, e.g. "Mon 16".
#[must_use]
pub fn day_header(date: NaiveDate) -> String {
    format!("{} {}", weekday_abbrev(date), date.day())
}

/// Short localized date like "Jun 16", for the week-range label.
#[must_use]
pub fn short_date(date: NaiveDate) -> String {
    format!("{} {}", month_abbrev(date), date.day())
}

/// Longer localized day label like "Mon, Jun 16", for the composed message.
#[must_use]
pub fn long_date(date: NaiveDate) -> String {
    format!(
        "{}, {} {}",
        weekday_abbrev(date),
        month_abbrev(date),
        date.day()
    )
}

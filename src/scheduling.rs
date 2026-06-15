// SPDX-License-Identifier: GPL-3.0-only
//
// Text composition for the Scheduling Helper.
//
// The picker is a drag-to-select week grid: the user drags out availability
// spans, and this module turns those spans into a human-readable message
// (grouped by day, overlapping/adjacent spans merged into ranges). Times and
// day names are formatted via `crate::locale` to honor the user's locale.

use chrono::{DateTime, Local, NaiveDate};

/// A user-selected availability span.
pub type Span = (DateTime<Local>, DateTime<Local>);

/// Sort spans and merge overlapping or touching ones within the same day.
#[must_use]
pub fn merge_spans(spans: &[Span]) -> Vec<Span> {
    let mut sorted = spans.to_vec();
    sorted.sort_by_key(|(start, _)| *start);

    let mut merged: Vec<Span> = Vec::new();
    for (start, end) in sorted {
        if let Some(last) = merged.last_mut()
            && last.0.date_naive() == start.date_naive()
            && last.1 >= start
        {
            if end > last.1 {
                last.1 = end;
            }
            continue;
        }
        merged.push((start, end));
    }
    merged
}

/// Format a span range, collapsing a shared meridiem on 12-hour locales
/// ("9:00 – 9:30 AM"); on 24-hour locales the two times are simply joined.
fn fmt_range(start: &DateTime<Local>, end: &DateTime<Local>) -> String {
    let start_s = crate::locale::format_time(start);
    let end_s = crate::locale::format_time(end);
    match (start_s.rsplit_once(' '), end_s.rsplit_once(' ')) {
        (Some((s_time, s_mer)), Some((_, e_mer))) if s_mer == e_mer => {
            format!("{s_time} – {end_s}")
        }
        _ => format!("{start_s} – {end_s}"),
    }
}

/// Compose the availability message from a set of selected spans.
///
/// `intro` is the already-localized opening line. Spans are merged, grouped into
/// one line per day, and formatted using the user's locale. Returns an empty
/// string when nothing is selected.
#[must_use]
pub fn compose_availability(spans: &[Span], intro: &str) -> String {
    let merged = merge_spans(spans);
    if merged.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current_day: Option<NaiveDate> = None;
    let mut parts: Vec<String> = Vec::new();

    let flush = |day: NaiveDate, parts: &[String], lines: &mut Vec<String>| {
        lines.push(format!(
            "• {}: {}",
            crate::locale::long_date(day),
            parts.join(", ")
        ));
    };

    for (start, end) in &merged {
        let day = start.date_naive();
        if current_day != Some(day) {
            if let Some(d) = current_day {
                flush(d, &parts, &mut lines);
            }
            current_day = Some(day);
            parts = Vec::new();
        }
        parts.push(fmt_range(start, end));
    }
    if let Some(d) = current_day {
        flush(d, &parts, &mut lines);
    }

    format!("{intro}\n\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    // merge_spans is locale-independent, so these assertions are deterministic.

    #[test]
    fn merges_overlapping_same_day() {
        let m = merge_spans(&[
            (dt(2026, 6, 15, 9, 0), dt(2026, 6, 15, 10, 0)),
            (dt(2026, 6, 15, 9, 30), dt(2026, 6, 15, 11, 0)),
        ]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0], (dt(2026, 6, 15, 9, 0), dt(2026, 6, 15, 11, 0)));
    }

    #[test]
    fn merges_touching_spans() {
        let m = merge_spans(&[
            (dt(2026, 6, 15, 9, 0), dt(2026, 6, 15, 10, 0)),
            (dt(2026, 6, 15, 10, 0), dt(2026, 6, 15, 11, 0)),
        ]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].1, dt(2026, 6, 15, 11, 0));
    }

    #[test]
    fn keeps_gap_same_day() {
        let m = merge_spans(&[
            (dt(2026, 6, 15, 9, 0), dt(2026, 6, 15, 10, 0)),
            (dt(2026, 6, 15, 14, 0), dt(2026, 6, 15, 15, 0)),
        ]);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn sorts_unordered_and_separates_days() {
        let m = merge_spans(&[
            (dt(2026, 6, 16, 9, 0), dt(2026, 6, 16, 10, 0)),
            (dt(2026, 6, 15, 9, 0), dt(2026, 6, 15, 10, 0)),
        ]);
        assert_eq!(m.len(), 2);
        assert!(m[0].0 < m[1].0, "spans should be chronological");
    }

    #[test]
    fn compose_empty_is_empty() {
        assert_eq!(compose_availability(&[], "Intro:"), "");
    }

    #[test]
    fn compose_uses_intro_and_one_line_per_day() {
        let msg = compose_availability(
            &[
                (dt(2026, 6, 15, 9, 0), dt(2026, 6, 15, 10, 0)),
                (dt(2026, 6, 16, 9, 0), dt(2026, 6, 16, 10, 0)),
            ],
            "Intro:",
        );
        assert!(msg.starts_with("Intro:"), "got: {msg}");
        assert_eq!(msg.matches('•').count(), 2);
    }
}

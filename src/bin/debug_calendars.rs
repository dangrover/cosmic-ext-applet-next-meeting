// SPDX-License-Identifier: GPL-3.0-only
//
// Debug utility to explore Evolution Data Server calendar data.
// Run with: cargo run --bin debug_calendars

use calcard::icalendar::{ICalendar, ICalendarProperty, ICalendarValue};
use std::collections::HashMap;
use zbus::{
    Connection,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::session().await?;

    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("list") => list_calendars(&conn).await?,
        Some("events") => {
            let uid = args
                .get(2)
                .ok_or("Usage: debug_calendars events <calendar_uid> [limit]")?;
            let limit = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
            show_events(&conn, uid, limit).await?;
        }
        Some("raw") => {
            let uid = args
                .get(2)
                .ok_or("Usage: debug_calendars raw <calendar_uid> [limit] [lines]")?;
            let limit = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
            let max_lines = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(20);
            show_raw_events(&conn, uid, limit, max_lines).await?;
        }
        Some("refresh") => {
            let uid = args
                .get(2)
                .ok_or("Usage: debug_calendars refresh <calendar_uid>")?;
            refresh_calendar(&conn, uid).await?;
        }
        _ => {
            println!("Calendar Debug Utility");
            println!();
            println!("Usage:");
            println!(
                "  cargo run --bin debug_calendars list              - List all calendar sources"
            );
            println!(
                "  cargo run --bin debug_calendars events <uid> [n]  - Show n parsed events from calendar"
            );
            println!(
                "  cargo run --bin debug_calendars raw <uid> [n] [lines] - Show n raw ICS objects (lines per event, 0=all)"
            );
        }
    }

    Ok(())
}

async fn list_calendars(conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let proxy = zbus::Proxy::new(
        conn,
        "org.gnome.evolution.dataserver.Sources5",
        "/org/gnome/evolution/dataserver/SourceManager",
        "org.freedesktop.DBus.ObjectManager",
    )
    .await?;

    let reply = proxy.call_method("GetManagedObjects", &()).await?;
    let objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> =
        reply.body().deserialize()?;

    println!("{:<45} {:<30} Type", "UID", "Display Name");
    println!("{}", "-".repeat(90));

    for (_path, interfaces) in objects {
        if let Some(props) = interfaces.get("org.gnome.evolution.dataserver.Source") {
            let uid = props.get("UID").and_then(|v| {
                if let Ok(Value::Str(s)) = v.downcast_ref::<Value>() {
                    Some(s.to_string())
                } else {
                    None
                }
            });

            let data = props.get("Data").and_then(|v| {
                if let Ok(Value::Str(s)) = v.downcast_ref::<Value>() {
                    Some(s.to_string())
                } else {
                    None
                }
            });

            if let (Some(uid), Some(data)) = (uid, data) {
                let display_name = parse_field(&data, "DisplayName").unwrap_or_else(|| uid.clone());
                let identity = parse_field(&data, "Identity").unwrap_or_default();

                let source_type = if data.contains("[Calendar]") {
                    if identity.starts_with("tasks::") {
                        "Task List"
                    } else {
                        "Calendar"
                    }
                } else if data.contains("[Address Book]") {
                    "Contacts"
                } else if data.contains("[Mail Account]") {
                    "Mail"
                } else if data.contains("[Collection]") {
                    "Collection"
                } else {
                    "Other"
                };

                // Only show calendars and task lists
                if data.contains("[Calendar]") {
                    println!("{uid:<45} {display_name:<30} {source_type}");
                }
            }
        }
    }

    Ok(())
}

async fn show_events(
    conn: &Connection,
    uid: &str,
    limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let events = get_calendar_events(conn, uid).await?;

    println!("Found {} total events in calendar", events.len());
    println!();

    let now = chrono::Local::now();
    let mut future_events: Vec<_> = events
        .iter()
        .filter_map(|ics| parse_event(ics))
        .filter(|e| e.start > now)
        .collect();

    future_events.sort_by_key(|e| e.start);

    println!("Future events ({} found):", future_events.len());
    println!("{:<25} Title", "Start");
    println!("{}", "-".repeat(80));

    for event in future_events.iter().take(limit) {
        println!(
            "{:<25} {}",
            event.start.format("%Y-%m-%d %H:%M"),
            event.title
        );
    }

    Ok(())
}

async fn show_raw_events(
    conn: &Connection,
    uid: &str,
    limit: usize,
    max_lines: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let events = get_calendar_events(conn, uid).await?;

    println!(
        "Showing {} of {} raw ICS objects:\n",
        limit.min(events.len()),
        events.len()
    );

    for (i, ics) in events.iter().take(limit).enumerate() {
        println!("=== Event {} ===", i + 1);
        // Show raw ICS (0 = unlimited)
        for (j, line) in ics.lines().enumerate() {
            if max_lines == 0 || j < max_lines {
                println!("  {line}");
            }
        }
        if max_lines > 0 && ics.lines().count() > max_lines {
            println!("  ... ({} more lines)", ics.lines().count() - max_lines);
        }

        // Also show what calcard parses (wrap in VCALENDAR if needed)
        let wrapped = if ics.trim().starts_with("BEGIN:VEVENT") {
            format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{ics}\r\nEND:VCALENDAR")
        } else {
            ics.clone()
        };
        match ICalendar::parse(&wrapped) {
            Ok(cal) => {
                println!("  [calcard] parsed {} components", cal.components.len());
                for comp in &cal.components {
                    // Show SUMMARY
                    if let Some(entry) = comp.property(&ICalendarProperty::Summary) {
                        let value = entry.values.iter().find_map(|v| {
                            if let ICalendarValue::Text(s) = v {
                                Some(s.as_str())
                            } else {
                                None
                            }
                        });
                        println!("  [calcard] SUMMARY = {value:?}");
                    }
                    // Show LOCATION
                    if let Some(entry) = comp.property(&ICalendarProperty::Location) {
                        let value = entry.values.iter().find_map(|v| {
                            if let ICalendarValue::Text(s) = v {
                                Some(s.as_str())
                            } else {
                                None
                            }
                        });
                        println!("  [calcard] LOCATION = {value:?}");
                    }
                    // Show DTSTART
                    if let Some(entry) = comp.property(&ICalendarProperty::Dtstart) {
                        println!("  [calcard] DTSTART = {:?}", entry.values);
                    }
                }
            }
            Err(e) => println!("  [calcard] parse error: {e:?}"),
        }
        println!();
    }

    Ok(())
}

async fn get_calendar_events(
    conn: &Connection,
    uid: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let factory = zbus::Proxy::new(
        conn,
        "org.gnome.evolution.dataserver.Calendar8",
        "/org/gnome/evolution/dataserver/CalendarFactory",
        "org.gnome.evolution.dataserver.CalendarFactory",
    )
    .await?;

    let reply = factory.call_method("OpenCalendar", &(uid,)).await?;
    let (calendar_path, bus_name): (String, String) = reply.body().deserialize()?;

    let calendar = zbus::Proxy::new(
        conn,
        bus_name.as_str(),
        calendar_path.as_str(),
        "org.gnome.evolution.dataserver.Calendar",
    )
    .await?;

    // Initialize the backend (required before any calendar operations)
    let _ = calendar.call_method("Open", &()).await;

    let reply = calendar.call_method("GetObjectList", &("",)).await?;
    let events: Vec<String> = reply.body().deserialize()?;

    Ok(events)
}

struct ParsedEvent {
    title: String,
    start: chrono::DateTime<chrono::Local>,
}

fn parse_event(ics: &str) -> Option<ParsedEvent> {
    let mut title = None;
    let mut start = None;

    for line in ics.lines() {
        let line = line.trim();
        if line.starts_with("SUMMARY") {
            title = line.split(':').nth(1).map(ToString::to_string);
        } else if line.starts_with("DTSTART") {
            start = parse_ical_datetime(line);
        }
    }

    Some(ParsedEvent {
        title: title.unwrap_or_else(|| "Untitled".to_string()),
        start: start?,
    })
}

fn parse_ical_datetime(line: &str) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::{Local, NaiveDateTime, TimeZone};

    // Extract the value part (after the colon)
    let value = line.split(':').next_back()?;
    let value = value.trim();

    // Handle UTC times (ending with Z)
    if let Some(value) = value.strip_suffix('Z')
        && value.len() >= 15
    {
        let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
        return Some(chrono::Utc.from_utc_datetime(&naive).with_timezone(&Local));
    }

    // Handle local times (YYYYMMDDTHHMMSS)
    if value.len() >= 15 && value.contains('T') {
        let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
        return Local.from_local_datetime(&naive).single();
    }

    // Handle date-only (YYYYMMDD)
    if value.len() == 8 {
        let naive =
            NaiveDateTime::parse_from_str(&format!("{value}T000000"), "%Y%m%dT%H%M%S").ok()?;
        return Local.from_local_datetime(&naive).single();
    }

    None
}

async fn refresh_calendar(conn: &Connection, uid: &str) -> Result<(), Box<dyn std::error::Error>> {
    let factory = zbus::Proxy::new(
        conn,
        "org.gnome.evolution.dataserver.Calendar8",
        "/org/gnome/evolution/dataserver/CalendarFactory",
        "org.gnome.evolution.dataserver.CalendarFactory",
    )
    .await?;

    let reply = factory.call_method("OpenCalendar", &(uid,)).await?;
    let (calendar_path, bus_name): (String, String) = reply.body().deserialize()?;
    println!("Opened: path={calendar_path} bus={bus_name}");

    let calendar = zbus::Proxy::new(
        conn,
        bus_name.as_str(),
        calendar_path.as_str(),
        "org.gnome.evolution.dataserver.Calendar",
    )
    .await?;

    match calendar.call_method("Open", &()).await {
        Ok(_) => println!("Open: OK"),
        Err(e) => println!("Open: {e}"),
    }

    match calendar.call_method("Refresh", &()).await {
        Ok(_) => println!("Refresh: OK"),
        Err(e) => println!("Refresh: {e}"),
    }

    println!("Waiting 5 seconds for sync...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    match calendar.call_method("GetObjectList", &("",)).await {
        Ok(reply) => {
            let events: Vec<String> = reply.body().deserialize()?;
            println!("Events after refresh: {}", events.len());
        }
        Err(e) => println!("GetObjectList error: {e}"),
    }

    Ok(())
}

fn parse_field(data: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}=");
    for line in data.lines() {
        let line = line.trim();
        if line.starts_with(&prefix) {
            return Some(line.strip_prefix(&prefix)?.to_string());
        }
    }
    None
}

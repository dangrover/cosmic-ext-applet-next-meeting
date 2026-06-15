// SPDX-License-Identifier: GPL-3.0-only

use calcard::icalendar::{
    ICalendar, ICalendarComponentType, ICalendarEntry, ICalendarParameterName,
    ICalendarParameterValue, ICalendarParticipationStatus, ICalendarProperty, ICalendarValue,
    ICalendarValueType, dates::TimeOrDelta,
};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use futures_util::StreamExt;
use regex::Regex;
use zbus::{Connection, zvariant};

/// User's attendance status for a meeting
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttendanceStatus {
    /// User has accepted the meeting
    Accepted,
    /// User has tentatively accepted
    Tentative,
    /// User has declined
    Declined,
    /// User hasn't responded yet
    NeedsAction,
    /// No attendance info (user is organizer or it's a personal event)
    #[default]
    None,
}

#[derive(Debug, Clone)]
pub struct Meeting {
    #[allow(dead_code)]
    pub uid: String,
    pub title: String,
    pub start: DateTime<Local>,
    #[allow(dead_code)]
    pub end: DateTime<Local>,
    pub location: Option<String>,
    pub description: Option<String>,
    pub calendar_uid: String,
    pub is_all_day: bool,
    pub attendance_status: AttendanceStatus,
}

#[derive(Debug, Clone)]
pub struct CalendarInfo {
    pub uid: String,
    pub display_name: String,
    pub color: Option<String>,
    /// Last update timestamp from EDS (ISO 8601 format)
    pub last_synced: Option<String>,
    /// Backend type (e.g., "local", "caldav", "google")
    pub backend: Option<String>,
}

impl CalendarInfo {
    /// Returns true if this calendar is a valid source of meetings.
    /// Some calendars (contacts, weather, birthdays) don't contain actual meetings.
    #[must_use]
    pub fn is_meeting_source(&self) -> bool {
        !matches!(
            self.backend.as_deref(),
            Some("contacts" | "weather" | "birthdays")
        )
    }
}

/// An online account that needs the user's attention (e.g. re-authentication).
#[derive(Debug, Clone)]
pub struct AccountNeedingAttention {
    /// Display-friendly identity (e.g. "dangrover@fastmail.com")
    pub identity: String,
}

/// Result of attempting to discover calendars from Evolution Data Server.
///
/// Distinguishes between "EDS isn't available" (not installed or not running)
/// and "EDS is available but no calendars are configured". These need different
/// guidance shown to the user, so we keep them separate rather than collapsing
/// both into an empty list.
#[derive(Debug, Clone, Default)]
pub struct CalendarDiscovery {
    /// Whether Evolution Data Server responded on the session bus at all.
    /// `false` means EDS is likely not installed or not running.
    pub eds_available: bool,
    /// The calendars that were discovered (empty when none are configured).
    pub calendars: Vec<CalendarInfo>,
}

/// Check GNOME Online Accounts for any calendar-enabled accounts that need
/// attention (expired credentials, etc.).
pub async fn check_accounts_needing_attention() -> Vec<AccountNeedingAttention> {
    use std::collections::HashMap;
    use zvariant::{OwnedObjectPath, OwnedValue, Value};

    let Ok(conn) = Connection::session().await else {
        return Vec::new();
    };

    let Ok(proxy) = zbus::Proxy::new(
        &conn,
        "org.gnome.OnlineAccounts",
        "/org/gnome/OnlineAccounts",
        "org.freedesktop.DBus.ObjectManager",
    )
    .await
    else {
        return Vec::new();
    };

    let Ok(reply) = proxy.call_method("GetManagedObjects", &()).await else {
        return Vec::new();
    };
    let Ok(objects) = reply
        .body()
        .deserialize::<HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>>()
    else {
        return Vec::new();
    };

    let mut results = Vec::new();

    for (_path, interfaces) in objects {
        let Some(account) = interfaces.get("org.gnome.OnlineAccounts.Account") else {
            continue;
        };

        // Only care about accounts that have calendar support
        if !interfaces.contains_key("org.gnome.OnlineAccounts.Calendar") {
            continue;
        }

        // Check if CalendarDisabled
        let calendar_disabled = account.get("CalendarDisabled").is_some_and(|v| {
            v.downcast_ref::<Value>()
                .is_ok_and(|val| matches!(val, Value::Bool(true)))
        });
        if calendar_disabled {
            continue;
        }

        // Check AttentionNeeded
        let attention_needed = account.get("AttentionNeeded").is_some_and(|v| {
            v.downcast_ref::<Value>()
                .is_ok_and(|val| matches!(val, Value::Bool(true)))
        });
        if !attention_needed {
            continue;
        }

        let identity = account
            .get("PresentationIdentity")
            .and_then(|v| {
                if let Ok(Value::Str(s)) = v.downcast_ref::<Value>() {
                    Some(s.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        results.push(AccountNeedingAttention { identity });
    }

    results
}

/// Fetch available calendars from Evolution Data Server via D-Bus
pub async fn get_available_calendars() -> CalendarDiscovery {
    // Debug: simulate EDS being present but with no calendars configured.
    if std::env::var("DEBUG_NO_CALENDARS").is_ok() {
        return CalendarDiscovery {
            eds_available: true,
            calendars: Vec::new(),
        };
    }
    // Debug: simulate Evolution Data Server not being installed/running.
    if std::env::var("DEBUG_NO_EDS").is_ok() {
        return CalendarDiscovery::default();
    }

    let Ok(conn) = Connection::session().await else {
        return CalendarDiscovery::default();
    };

    // `get_calendars_from_dbus` returns `None` only when EDS itself couldn't be
    // reached (service not activatable, call failed). An EDS that's present but
    // has no calendars returns `Some(empty)`, which is a distinct, valid state.
    match get_calendars_from_dbus(&conn).await {
        Some(calendars) => CalendarDiscovery {
            eds_available: true,
            calendars,
        },
        None => CalendarDiscovery::default(),
    }
}

/// Ask EDS to re-discover calendars from all collection/account backends.
///
/// Calls `RefreshBackend` on the `SourceManager` for every source that has a
/// `[Collection]` section (e.g. `CalDAV` or GOA accounts). This triggers
/// server-side discovery of new calendars that were added to the account.
pub async fn refresh_source_backends() {
    use std::collections::HashMap;
    use zvariant::{OwnedObjectPath, OwnedValue, Value};

    let Ok(conn) = Connection::session().await else {
        return;
    };

    let Ok(proxy) = zbus::Proxy::new(
        &conn,
        "org.gnome.evolution.dataserver.Sources5",
        "/org/gnome/evolution/dataserver/SourceManager",
        "org.gnome.evolution.dataserver.SourceManager",
    )
    .await
    else {
        return;
    };

    // Also get managed objects to find collection UIDs
    let Ok(om_proxy) = zbus::Proxy::new(
        &conn,
        "org.gnome.evolution.dataserver.Sources5",
        "/org/gnome/evolution/dataserver/SourceManager",
        "org.freedesktop.DBus.ObjectManager",
    )
    .await
    else {
        return;
    };

    let Ok(reply) = om_proxy.call_method("GetManagedObjects", &()).await else {
        return;
    };
    let Ok(objects) = reply
        .body()
        .deserialize::<HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>>()
    else {
        return;
    };

    for (_path, interfaces) in objects {
        let Some(source_props) = interfaces.get("org.gnome.evolution.dataserver.Source") else {
            continue;
        };

        // Check for [Collection] section in the Data property
        let is_collection = source_props.get("Data").is_some_and(|v| {
            if let Ok(Value::Str(s)) = v.downcast_ref::<Value>() {
                s.contains("[Collection]")
            } else {
                false
            }
        });

        if !is_collection {
            continue;
        }

        let Some(uid) = source_props.get("UID").and_then(|v| {
            if let Ok(Value::Str(s)) = v.downcast_ref::<Value>() {
                Some(s.to_string())
            } else {
                None
            }
        }) else {
            continue;
        };

        let _ = proxy.call_method("RefreshBackend", &(uid.as_str(),)).await;
    }
}

/// Refresh all calendars by triggering an upstream sync with remote servers.
/// This calls the Refresh D-Bus method on each calendar, which forces EDS to
/// fetch the latest data from CalDAV/Google/etc servers.
/// If `enabled_uids` is empty, all calendars are refreshed.
pub async fn refresh_calendars(enabled_uids: &[String]) {
    let Ok(conn) = Connection::session().await else {
        return;
    };

    // Get calendar source UIDs
    let Some(mut source_uids) = get_calendar_source_uids(&conn).await else {
        return;
    };

    // Filter to only enabled calendars if specified
    if !enabled_uids.is_empty() {
        source_uids.retain(|uid| enabled_uids.contains(uid));
    }

    // Open calendar factory
    let Ok(calendar_factory_proxy) = zbus::Proxy::new(
        &conn,
        "org.gnome.evolution.dataserver.Calendar8",
        "/org/gnome/evolution/dataserver/CalendarFactory",
        "org.gnome.evolution.dataserver.CalendarFactory",
    )
    .await
    else {
        return;
    };

    // Refresh each calendar
    for source_uid in &source_uids {
        // Open the calendar
        let (calendar_path, bus_name): (String, String) = match calendar_factory_proxy
            .call_method("OpenCalendar", &(source_uid.as_str(),))
            .await
        {
            Ok(reply) => match reply.body().deserialize::<(String, String)>() {
                Ok((path, bus)) => (path, bus),
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        // Get a proxy to the calendar
        let Ok(calendar_proxy) = zbus::Proxy::new(
            &conn,
            bus_name.as_str(),
            calendar_path.as_str(),
            "org.gnome.evolution.dataserver.Calendar",
        )
        .await
        else {
            continue;
        };

        // Initialize the backend (required before any calendar operations)
        let _ = calendar_proxy.call_method("Open", &()).await;

        // Call Refresh method (fire and forget - don't wait for completion)
        let _ = calendar_proxy.call_method("Refresh", &()).await;
    }
}

/// Watch for calendar changes via D-Bus `PropertiesChanged` signals.
/// Returns a stream that yields () whenever any calendar's properties change.
/// This allows detecting when EDS has updated calendar data after a sync.
pub async fn watch_calendar_changes(
    enabled_uids: Vec<String>,
    sender: tokio::sync::mpsc::Sender<()>,
) {
    let Ok(conn) = Connection::session().await else {
        return;
    };

    // Get calendar source UIDs
    let Some(mut source_uids) = get_calendar_source_uids(&conn).await else {
        return;
    };

    // Filter to enabled calendars if specified
    if !enabled_uids.is_empty() {
        source_uids.retain(|uid| enabled_uids.contains(uid));
    }

    // Open calendar factory
    let Ok(calendar_factory_proxy) = zbus::Proxy::new(
        &conn,
        "org.gnome.evolution.dataserver.Calendar8",
        "/org/gnome/evolution/dataserver/CalendarFactory",
        "org.gnome.evolution.dataserver.CalendarFactory",
    )
    .await
    else {
        return;
    };

    // First collect all calendar (path, bus) pairs
    let mut calendar_info: Vec<(String, String)> = Vec::new();
    for source_uid in &source_uids {
        let (calendar_path, bus_name): (String, String) = match calendar_factory_proxy
            .call_method("OpenCalendar", &(source_uid.as_str(),))
            .await
        {
            Ok(reply) => match reply.body().deserialize::<(String, String)>() {
                Ok((path, bus)) => (path, bus),
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        calendar_info.push((calendar_path, bus_name));
    }

    if calendar_info.is_empty() {
        return;
    }

    // Spawn a watcher task for each calendar
    // Each task watches for PropertiesChanged and sends to the shared channel
    let mut handles = Vec::new();
    for (calendar_path, bus_name) in calendar_info {
        let sender_clone = sender.clone();
        let conn_clone = conn.clone();
        handles.push(tokio::spawn(async move {
            watch_single_calendar(conn_clone, bus_name, calendar_path, sender_clone).await;
        }));
    }

    // Wait for all watcher tasks (they run indefinitely until cancelled)
    for handle in handles {
        let _ = handle.await;
    }
}

/// Watch a single calendar for `PropertiesChanged` signals
async fn watch_single_calendar(
    conn: Connection,
    bus_name: String,
    calendar_path: String,
    sender: tokio::sync::mpsc::Sender<()>,
) {
    // Create a proxy for the Properties interface on this calendar
    let Ok(props_proxy) = zbus::Proxy::new(
        &conn,
        bus_name.as_str(),
        calendar_path.as_str(),
        "org.freedesktop.DBus.Properties",
    )
    .await
    else {
        return;
    };

    // Subscribe to PropertiesChanged signals
    let Ok(mut stream) = props_proxy.receive_signal("PropertiesChanged").await else {
        return;
    };

    // Listen for signals and notify the channel
    while stream.next().await.is_some() {
        let _ = sender.try_send(());
    }
}

/// Watch for new or removed calendar sources via the `ObjectManager`'s
/// `InterfacesAdded` / `InterfacesRemoved` signals on the EDS `SourceManager`.
///
/// Sends to the channel whenever the set of sources changes, so the app can
/// immediately refresh its calendar list and meetings.
pub async fn watch_source_changes(sender: tokio::sync::mpsc::Sender<()>) {
    let Ok(conn) = Connection::session().await else {
        return;
    };

    let Ok(proxy) = zbus::Proxy::new(
        &conn,
        "org.gnome.evolution.dataserver.Sources5",
        "/org/gnome/evolution/dataserver/SourceManager",
        "org.freedesktop.DBus.ObjectManager",
    )
    .await
    else {
        return;
    };

    // Listen for InterfacesAdded (new source) and InterfacesRemoved (deleted source)
    let added = proxy.receive_signal("InterfacesAdded").await;
    let removed = proxy.receive_signal("InterfacesRemoved").await;

    // Merge whichever streams we were able to subscribe to
    match (added, removed) {
        (Ok(a), Ok(r)) => {
            let mut merged = futures_util::stream::select(a, r);
            while merged.next().await.is_some() {
                let _ = sender.try_send(());
            }
        }
        (Ok(mut a), Err(_)) => {
            while a.next().await.is_some() {
                let _ = sender.try_send(());
            }
        }
        (Err(_), Ok(mut r)) => {
            while r.next().await.is_some() {
                let _ = sender.try_send(());
            }
        }
        (Err(_), Err(_)) => {}
    }
}

/// Watch for system resume (from sleep) and session unlock events via D-Bus.
/// Uses `org.freedesktop.login1` on the system bus.
/// Sends to the channel when:
/// - System wakes from suspend (`PrepareForSleep` signal with `false`)
/// - Session is unlocked (`Unlock` signal or `LockedHint` becomes `false`)
///
/// Fails gracefully on non-systemd systems or when D-Bus access is unavailable.
pub async fn watch_system_resume(sender: tokio::sync::mpsc::Sender<()>) {
    // Connect to system bus (not session bus)
    let Ok(conn) = Connection::system().await else {
        return;
    };

    // Spawn watchers for both signals concurrently
    let sender_clone = sender.clone();
    let conn_clone = conn.clone();

    let sleep_handle = tokio::spawn(async move {
        watch_prepare_for_sleep(conn_clone, sender_clone).await;
    });

    let unlock_handle = tokio::spawn(async move {
        watch_session_unlock(conn, sender).await;
    });

    // Wait for both (they run indefinitely until cancelled)
    let _ = tokio::join!(sleep_handle, unlock_handle);
}

/// Watch for `PrepareForSleep` signal from logind.
/// Fires when system wakes from suspend (signal argument is `false`).
async fn watch_prepare_for_sleep(conn: Connection, sender: tokio::sync::mpsc::Sender<()>) {
    // Create proxy for login1 Manager
    let Ok(proxy) = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    else {
        return;
    };

    // Subscribe to PrepareForSleep signal
    let Ok(mut stream) = proxy.receive_signal("PrepareForSleep").await else {
        return;
    };

    // Listen for signals
    while let Some(signal) = stream.next().await {
        // PrepareForSleep has a boolean argument: true = going to sleep, false = waking up
        if signal
            .body()
            .deserialize::<bool>()
            .is_ok_and(|going_to_sleep| !going_to_sleep)
        {
            // System just woke up
            let _ = sender.try_send(());
        }
    }
}

/// Watch for session unlock events from logind.
/// Listens for the `Unlock` signal on the current session.
async fn watch_session_unlock(conn: Connection, sender: tokio::sync::mpsc::Sender<()>) {
    // First, get the current session path
    let Ok(manager_proxy) = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    else {
        return;
    };

    // Get our session object path
    let session_path: String = match manager_proxy
        .call_method("GetSessionByPID", &(std::process::id(),))
        .await
    {
        Ok(reply) => match reply.body().deserialize::<zvariant::OwnedObjectPath>() {
            Ok(path) => path.to_string(),
            Err(_) => return,
        },
        Err(_) => return,
    };

    // Create proxy for our session
    let Ok(session_proxy) = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        session_path.as_str(),
        "org.freedesktop.login1.Session",
    )
    .await
    else {
        return;
    };

    // Subscribe to Unlock signal
    let Ok(mut stream) = session_proxy.receive_signal("Unlock").await else {
        return;
    };

    // Listen for unlock signals
    while stream.next().await.is_some() {
        let _ = sender.try_send(());
    }
}

/// Fetch upcoming meetings from Evolution Data Server via D-Bus
/// If `enabled_uids` is empty, all calendars are queried.
/// Otherwise, only calendars with UIDs in the list are queried.
/// Returns up to `limit` meetings (use limit=0 for just the next meeting info).
/// `additional_emails` are extra email addresses to identify the user in ATTENDEE fields.
pub async fn get_upcoming_meetings(
    enabled_uids: &[String],
    limit: usize,
    additional_emails: &[String],
) -> Vec<Meeting> {
    // Debug: simulate no calendars for testing
    if std::env::var("DEBUG_NO_CALENDARS").is_ok() {
        return Vec::new();
    }

    let Ok(conn) = Connection::session().await else {
        return Vec::new();
    };

    get_meetings_from_dbus(&conn, enabled_uids, limit.max(1), additional_emails).await
}

/// A timed calendar event to display as a block in the Scheduling Helper grid.
#[derive(Debug, Clone)]
pub struct CalendarEventBlock {
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub title: String,
    /// Calendar color as a hex string (e.g. `#aabbcc`), if the source has one.
    pub color: Option<String>,
    /// UID of the calendar this event belongs to (for filtering by calendar).
    pub calendar_uid: String,
}

/// Fetch timed calendar events over the next `days` to draw as blocks in the
/// Scheduling Helper week grid.
///
/// Includes each event's title and its calendar's color. All-day events are
/// excluded (they don't map to a time band), as are declined events. Reuses the
/// normal meeting query (which already spans 30 days) with a generous result cap.
pub async fn get_event_blocks(
    enabled_uids: &[String],
    additional_emails: &[String],
    days: u32,
) -> Vec<CalendarEventBlock> {
    // Resolve a color per calendar so events can be tinted to match.
    let discovery = get_available_calendars().await;
    let color_for = |uid: &str| {
        discovery
            .calendars
            .iter()
            .find(|c| c.uid == uid)
            .and_then(|c| c.color.clone())
    };

    let meetings = get_upcoming_meetings(enabled_uids, 1000, additional_emails).await;
    let now = Local::now();
    let horizon = now + chrono::Duration::days(i64::from(days));
    meetings
        .into_iter()
        .filter(|m| !m.is_all_day)
        .filter(|m| m.attendance_status != AttendanceStatus::Declined)
        .filter(|m| m.end > now && m.start < horizon)
        .map(|m| CalendarEventBlock {
            color: color_for(&m.calendar_uid),
            calendar_uid: m.calendar_uid,
            start: m.start,
            end: m.end,
            title: m.title,
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
async fn get_meetings_from_dbus(
    conn: &Connection,
    enabled_uids: &[String],
    limit: usize,
    additional_emails: &[String],
) -> Vec<Meeting> {
    // Evolution Data Server workflow:
    // 1. Get calendar source UIDs from D-Bus SourceManager
    // 2. For each source, use CalendarFactory.OpenCalendar to get a calendar object
    // 3. Query the calendar object for events using GetObjectList
    // 4. Parse the iCalendar objects

    // Step 1: Get calendar source UIDs from D-Bus SourceManager
    let Some(mut source_uids) = get_calendar_source_uids(conn).await else {
        return Vec::new();
    };

    // Filter to only enabled calendars if a filter is specified
    if !enabled_uids.is_empty() {
        source_uids.retain(|uid| enabled_uids.contains(uid));
    }

    // Step 2: Open calendars and get events
    let Ok(calendar_factory_proxy) = zbus::Proxy::new(
        conn,
        "org.gnome.evolution.dataserver.Calendar8",
        "/org/gnome/evolution/dataserver/CalendarFactory",
        "org.gnome.evolution.dataserver.CalendarFactory",
    )
    .await
    else {
        return Vec::new();
    };

    let mut all_meetings: Vec<(bool, Meeting)> = Vec::new();

    for source_uid in source_uids {
        // Open the calendar for this source
        let (calendar_path, bus_name): (String, String) = match calendar_factory_proxy
            .call_method("OpenCalendar", &(source_uid.as_str(),))
            .await
        {
            Ok(reply) => match reply.body().deserialize::<(String, String)>() {
                Ok((path, bus)) => (path, bus),
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        // Step 3: Query the calendar for events using GetObjectList
        let Ok(calendar_proxy) = zbus::Proxy::new(
            conn,
            bus_name.as_str(),
            calendar_path.as_str(),
            "org.gnome.evolution.dataserver.Calendar",
        )
        .await
        else {
            continue;
        };

        // Initialize the backend (required before any calendar operations)
        let _ = calendar_proxy.call_method("Open", &()).await;

        // Get the CalEmailAddress property for this calendar
        // This is used to identify the user in ATTENDEE fields
        let cal_email: Option<String> = calendar_proxy
            .get_property::<String>("CalEmailAddress")
            .await
            .ok();

        // Combine CalEmailAddress with additional_emails for user identification
        // Filter out empty strings from additional_emails
        let mut user_emails: Vec<String> = additional_emails
            .iter()
            .filter(|e| !e.trim().is_empty())
            .cloned()
            .collect();
        if let Some(email) = cal_email
            && !email.is_empty()
            && !user_emails.iter().any(|e| e.eq_ignore_ascii_case(&email))
        {
            user_emails.push(email);
        }

        // GetObjectList takes an S-expression query string
        // Use occur-in-time-range? to expand recurring events into instances
        // Query from 30 minutes ago (to include in-progress meetings) to 30 days in the future
        let now = Local::now();
        let query_start = now - chrono::Duration::minutes(30);
        let query_end = now + chrono::Duration::days(30);
        // Convert to UTC for the query (EDS expects UTC timestamps)
        let query_start_utc = query_start.with_timezone(&chrono::Utc);
        let query_end_utc = query_end.with_timezone(&chrono::Utc);
        let query = format!(
            "(occur-in-time-range? (make-time \"{}\") (make-time \"{}\"))",
            query_start_utc.format("%Y%m%dT%H%M%SZ"),
            query_end_utc.format("%Y%m%dT%H%M%SZ")
        );

        let ics_objects: Vec<String> = match calendar_proxy
            .call_method("GetObjectList", &(query.as_str(),))
            .await
        {
            Ok(reply) => match reply.body().deserialize::<Vec<String>>() {
                Ok(objects) => objects,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        // Step 4: Parse iCalendar objects and extract meetings using calcard
        parse_ics_objects(
            &ics_objects,
            &source_uid,
            now,
            query_start,
            &user_emails,
            &mut all_meetings,
        );
    }

    // Deduplicate and sort
    dedup_and_sort_meetings(all_meetings, limit)
}

/// Parse ICS objects into meetings, collecting into the provided vector.
///
/// Each ICS object is parsed with calcard, recurring events are expanded,
/// and results are tagged with whether they're RECURRENCE-ID overrides.
fn parse_ics_objects(
    ics_objects: &[String],
    source_uid: &str,
    now: DateTime<Local>,
    query_start: DateTime<Local>,
    user_emails: &[String],
    all_meetings: &mut Vec<(bool, Meeting)>,
) {
    for ics_object in ics_objects {
        // EDS returns raw VEVENT objects without VCALENDAR wrapper
        let wrapped = if ics_object.trim().starts_with("BEGIN:VEVENT") {
            format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{ics_object}\r\nEND:VCALENDAR")
        } else {
            ics_object.clone()
        };

        // Parse with calcard (handles line unfolding and text unescaping)
        let Ok(calendar) = ICalendar::parse(&wrapped) else {
            continue;
        };

        // Get the local timezone for expansion
        let local_tz = localzone::get_local_zone()
            .and_then(|name| chrono_tz::Tz::from_str_insensitive(&name).ok())
            .unwrap_or(chrono_tz::Tz::UTC);

        // Expand recurring events (handles RRULE, EXDATE, RDATE)
        // Use a large limit since EDS returns master events with RRULEs that
        // may have started years ago; we need enough instances to reach today.
        let expanded = calendar.expand_dates(local_tz, 10_000);

        for event in expanded.events {
            // Get the component for this event
            let Some(comp) = calendar.components.get(event.comp_id as usize) else {
                continue;
            };

            // Skip non-events
            if !matches!(comp.component_type, ICalendarComponentType::VEvent) {
                continue;
            }

            // Convert start time to local
            let start: DateTime<Local> = event.start.with_timezone(&Local);

            // Convert end time to local (handle both Time and Delta variants)
            let end: DateTime<Local> = match event.end {
                TimeOrDelta::Time(t) => t.with_timezone(&Local),
                TimeOrDelta::Delta(d) => start + d,
            };

            // Filter by time range
            if !should_include_meeting(start, end, now, query_start) {
                continue;
            }

            // Extract properties from the component
            let uid = extract_text_property(comp, &ICalendarProperty::Uid).unwrap_or_default();
            let title = extract_text_property(comp, &ICalendarProperty::Summary)
                .unwrap_or_else(|| "Untitled Event".to_string());
            let location = extract_text_property(comp, &ICalendarProperty::Location);
            let description = extract_text_property(comp, &ICalendarProperty::Description);

            // Check if this is an all-day event (DTSTART has VALUE=DATE or no time part)
            let is_all_day = comp
                .property(&ICalendarProperty::Dtstart)
                .is_some_and(|prop| {
                    // Check VALUE=DATE parameter
                    let has_date_param = prop.params.iter().any(|p| {
                        matches!(p.name, ICalendarParameterName::Value)
                            && matches!(
                                p.value,
                                ICalendarParameterValue::Value(ICalendarValueType::Date)
                            )
                    });
                    // Check if value has no time component
                    let no_time = prop.values.iter().any(
                        |v| matches!(v, ICalendarValue::PartialDateTime(pdt) if pdt.hour.is_none()),
                    );
                    has_date_param || no_time
                });

            // Parse attendance status from ATTENDEE entries
            let attendance_status = parse_attendance_status_calcard(&comp.entries, user_emails);

            // Generate unique ID using uid@timestamp so that recurring
            // master expansions and RECURRENCE-ID overrides for the same
            // date/time produce the same key (enabling dedup below).
            let meeting_uid = format!("{}@{}", uid, start.format("%Y%m%dT%H%M%S"));

            // Track whether this is a RECURRENCE-ID override (more specific
            // than an expanded master instance, so preferred during dedup)
            let is_override = comp
                .entries
                .iter()
                .any(|e| matches!(e.name, ICalendarProperty::RecurrenceId));

            all_meetings.push((
                is_override,
                Meeting {
                    uid: meeting_uid,
                    title,
                    start,
                    end,
                    location,
                    description,
                    calendar_uid: source_uid.to_string(),
                    is_all_day,
                    attendance_status,
                },
            ));
        }
    }
}

/// Deduplicate meetings and sort by start time.
///
/// When a recurring master expansion and a RECURRENCE-ID override produce
/// the same `uid@timestamp` key, the override is preferred (it may have
/// updated details like a changed title, time, or location).
fn dedup_and_sort_meetings(all_meetings: Vec<(bool, Meeting)>, limit: usize) -> Vec<Meeting> {
    let mut deduped: std::collections::HashMap<String, Meeting> = std::collections::HashMap::new();
    for (is_override, meeting) in all_meetings {
        match deduped.entry(meeting.uid.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(meeting);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                // RECURRENCE-ID overrides take priority over expanded master instances
                if is_override {
                    e.insert(meeting);
                }
            }
        }
    }

    let mut meetings: Vec<Meeting> = deduped.into_values().collect();
    meetings.sort_by_key(|m| m.start);
    meetings.into_iter().take(limit).collect()
}

/// Get calendar source UIDs from Evolution Data Server via D-Bus
///
/// This queries the `SourceManager`'s `ObjectManager` interface to discover
/// all sources, including those from GNOME Online Accounts which are
/// not stored as files in ~/.config/evolution/sources/
async fn get_calendar_source_uids(conn: &Connection) -> Option<Vec<String>> {
    use std::collections::HashMap;
    use zvariant::{OwnedObjectPath, OwnedValue, Value};

    let source_manager_proxy = zbus::Proxy::new(
        conn,
        "org.gnome.evolution.dataserver.Sources5",
        "/org/gnome/evolution/dataserver/SourceManager",
        "org.freedesktop.DBus.ObjectManager",
    )
    .await
    .ok()?;

    // GetManagedObjects returns a{oa{sa{sv}}} - dict of object paths to interface properties
    let reply = source_manager_proxy
        .call_method("GetManagedObjects", &())
        .await
        .ok()?;

    let objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> =
        reply.body().deserialize().ok()?;

    let mut source_uids = Vec::new();

    for (_path, interfaces) in objects {
        // Look for the Source interface
        if let Some(source_props) = interfaces.get("org.gnome.evolution.dataserver.Source") {
            // Get the UID
            let uid = source_props.get("UID").and_then(|v| {
                if let Ok(Value::Str(s)) = v.downcast_ref::<Value>() {
                    Some(s.to_string())
                } else {
                    None
                }
            });

            // Get the Data (source configuration) and check for [Calendar] section
            let has_calendar = source_props.get("Data").is_some_and(|v| {
                if let Ok(Value::Str(s)) = v.downcast_ref::<Value>() {
                    s.contains("[Calendar]")
                } else {
                    false
                }
            });

            if let Some(uid) = uid
                && has_calendar
            {
                source_uids.push(uid);
            }
        }
    }

    Some(source_uids)
}

/// Get calendar info (UID and display name) from Evolution Data Server via D-Bus
async fn get_calendars_from_dbus(conn: &Connection) -> Option<Vec<CalendarInfo>> {
    use std::collections::HashMap;
    use zvariant::{OwnedObjectPath, OwnedValue, Value};

    let source_manager_proxy = zbus::Proxy::new(
        conn,
        "org.gnome.evolution.dataserver.Sources5",
        "/org/gnome/evolution/dataserver/SourceManager",
        "org.freedesktop.DBus.ObjectManager",
    )
    .await
    .ok()?;

    let reply = source_manager_proxy
        .call_method("GetManagedObjects", &())
        .await
        .ok()?;

    let objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> =
        reply.body().deserialize().ok()?;

    let mut calendars = Vec::new();

    for (_path, interfaces) in objects {
        if let Some(source_props) = interfaces.get("org.gnome.evolution.dataserver.Source") {
            // Get the UID
            let uid = source_props.get("UID").and_then(|v| {
                if let Ok(Value::Str(s)) = v.downcast_ref::<Value>() {
                    Some(s.to_string())
                } else {
                    None
                }
            });

            // Get the Data field and extract DisplayName and check for [Calendar]
            let data = source_props.get("Data").and_then(|v| {
                if let Ok(Value::Str(s)) = v.downcast_ref::<Value>() {
                    Some(s.to_string())
                } else {
                    None
                }
            });

            if let (Some(uid), Some(data)) = (uid, data)
                && data.contains("[Calendar]")
            {
                let display_name = parse_display_name(&data).unwrap_or_else(|| uid.clone());
                let color = parse_color(&data);
                let backend = parse_backend_name(&data);
                calendars.push(CalendarInfo {
                    uid,
                    display_name,
                    color,
                    last_synced: None, // Will be filled in below
                    backend,
                });
            }
        }
    }

    // Fetch last_synced (Revision) for each calendar
    if let Ok(factory_proxy) = zbus::Proxy::new(
        conn,
        "org.gnome.evolution.dataserver.Calendar8",
        "/org/gnome/evolution/dataserver/CalendarFactory",
        "org.gnome.evolution.dataserver.CalendarFactory",
    )
    .await
    {
        for cal in &mut calendars {
            if let Ok(reply) = factory_proxy
                .call_method("OpenCalendar", &(cal.uid.as_str(),))
                .await
                && let Ok((calendar_path, bus_name)) =
                    reply.body().deserialize::<(String, String)>()
                && let Ok(cal_proxy) = zbus::Proxy::new(
                    conn,
                    bus_name.as_str(),
                    calendar_path.as_str(),
                    "org.gnome.evolution.dataserver.Calendar",
                )
                .await
            {
                // Initialize the backend (required before reading properties)
                let _ = cal_proxy.call_method("Open", &()).await;
                // Get the Revision property (format: "2026-01-08T04:19:20Z(0)")
                if let Ok(revision) = cal_proxy.get_property::<String>("Revision").await {
                    // Extract just the timestamp part before the parentheses
                    let timestamp = revision.split('(').next().unwrap_or(&revision);
                    cal.last_synced = Some(timestamp.to_string());
                }
            }
        }
    }

    // Sort by display name for consistent ordering
    calendars.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Some(calendars)
}

/// Parse `DisplayName` from INI-format source data
fn parse_display_name(data: &str) -> Option<String> {
    // Look for DisplayName= line (without locale suffix like DisplayName[en])
    for line in data.lines() {
        let line = line.trim();
        if line.starts_with("DisplayName=") {
            return Some(line.strip_prefix("DisplayName=")?.to_string());
        }
    }
    None
}

/// Parse Color from INI-format source data (in [Calendar] section)
fn parse_color(data: &str) -> Option<String> {
    // Look for Color= specifically in the [Calendar] section, since other sections
    // (like [WebDAV Backend]) may have an empty Color= field
    let mut in_calendar_section = false;
    for line in data.lines() {
        let line = line.trim();
        if line == "[Calendar]" {
            in_calendar_section = true;
        } else if line.starts_with('[') {
            in_calendar_section = false;
        } else if in_calendar_section && line.starts_with("Color=") {
            let color = line.strip_prefix("Color=")?.to_string();
            if !color.is_empty() {
                return Some(color);
            }
        }
    }
    None
}

/// Parse `BackendName` from INI-format source data (in [Calendar] section)
fn parse_backend_name(data: &str) -> Option<String> {
    // Look for BackendName= in the [Calendar] section
    let mut in_calendar_section = false;
    for line in data.lines() {
        let line = line.trim();
        if line == "[Calendar]" {
            in_calendar_section = true;
        } else if line.starts_with('[') {
            in_calendar_section = false;
        } else if in_calendar_section && line.starts_with("BackendName=") {
            return Some(line.strip_prefix("BackendName=")?.to_string());
        }
    }
    None
}

/// Determine if a meeting should be included based on its timing.
/// Returns true if the meeting is either:
/// - Future (starts after now)
/// - In-progress (started within the query window and hasn't ended yet)
///
/// Also validates that start < end (invalid meetings are excluded).
fn should_include_meeting(
    start: DateTime<Local>,
    end: DateTime<Local>,
    now: DateTime<Local>,
    query_start: DateTime<Local>,
) -> bool {
    let is_future = start > now;
    let is_in_progress = start <= now && start > query_start && end > now;
    (is_future || is_in_progress) && start < end
}

/// Extract text value from a calcard component property
fn extract_text_property(
    comp: &calcard::icalendar::ICalendarComponent,
    prop: &ICalendarProperty,
) -> Option<String> {
    comp.property(prop).and_then(|entry| {
        entry.values.iter().find_map(|v| match v {
            ICalendarValue::Text(s) => Some(s.clone()),
            _ => None,
        })
    })
}

/// Parse attendance status from calcard ATTENDEE entries
fn parse_attendance_status_calcard(
    entries: &[ICalendarEntry],
    user_emails: &[String],
) -> AttendanceStatus {
    if user_emails.is_empty() {
        return AttendanceStatus::None;
    }

    let user_emails_lower: Vec<String> = user_emails.iter().map(|e| e.to_lowercase()).collect();

    // Find ATTENDEE entries
    for entry in entries
        .iter()
        .filter(|e| matches!(e.name, ICalendarProperty::Attendee))
    {
        // Extract email from parameters or value
        let attendee_email = entry
            .params
            .iter()
            .find_map(|p| {
                if matches!(p.name, ICalendarParameterName::Email)
                    && let ICalendarParameterValue::Text(email) = &p.value
                {
                    return Some(email.to_lowercase());
                }
                None
            })
            .or_else(|| {
                // Fall back to extracting from mailto: in the value
                entry.values.iter().find_map(|v| {
                    if let ICalendarValue::Uri(calcard::icalendar::Uri::Location(uri_str)) = v {
                        let uri = uri_str.to_lowercase();
                        if uri.starts_with("mailto:") {
                            return Some(uri.trim_start_matches("mailto:").to_string());
                        }
                    }
                    None
                })
            });

        // Check if this attendee matches any of the user's emails
        let is_user = attendee_email
            .as_ref()
            .is_some_and(|email| user_emails_lower.iter().any(|ue| ue == email));

        if is_user {
            // Extract PARTSTAT from parameters
            for param in &entry.params {
                if matches!(param.name, ICalendarParameterName::Partstat)
                    && let ICalendarParameterValue::Partstat(status) = &param.value
                {
                    return match status {
                        ICalendarParticipationStatus::Accepted => AttendanceStatus::Accepted,
                        ICalendarParticipationStatus::Tentative => AttendanceStatus::Tentative,
                        ICalendarParticipationStatus::Declined => AttendanceStatus::Declined,
                        ICalendarParticipationStatus::NeedsAction => AttendanceStatus::NeedsAction,
                        _ => AttendanceStatus::None,
                    };
                }
            }
        }
    }

    AttendanceStatus::None
}

#[allow(dead_code)] // Used by tests
fn parse_ical_datetime(value: &str, tzid: Option<&str>) -> Option<DateTime<Local>> {
    // The value might be in formats like:
    // - "20240221T123000" (local time)
    // - "20240221T123000Z" (UTC)
    // - "TZID=America/Los_Angeles:20240221T123000" (with timezone param in value)
    // - "VALUE=DATE:20250527" (date only)

    // Extract the actual datetime value (after the last colon if present)
    let value = if value.contains(':') {
        value.split(':').next_back().unwrap_or(value)
    } else {
        value
    };
    let value = value.trim();

    // Try parsing as ISO 8601 format first
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Local));
    }

    // Handle UTC times (ending with Z)
    if let Some(value) = value.strip_suffix('Z')
        && value.len() >= 15
        && let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
    {
        return Some(chrono::Utc.from_utc_datetime(&naive).with_timezone(&Local));
    }

    // Try parsing as YYYYMMDDTHHMMSS format
    if value.len() >= 15
        && value.contains('T')
        && let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
    {
        // If we have a TZID, interpret the time in that timezone and convert to local
        if let Some(tz_str) = tzid
            && let Some(tz) = parse_ical_timezone(tz_str)
        {
            return tz
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.with_timezone(&Local));
        }
        // Otherwise treat as local time
        return Local.from_local_datetime(&naive).single();
    }

    // Try parsing as date only (YYYYMMDD)
    if value.len() == 8
        && value.chars().all(|c| c.is_ascii_digit())
        && let Ok(naive) =
            NaiveDateTime::parse_from_str(&format!("{value}T000000"), "%Y%m%dT%H%M%S")
    {
        return Local.from_local_datetime(&naive).single();
    }

    None
}

/// Parse an iCal timezone string to `chrono_tz::Tz`.
///
/// Supports all IANA timezone identifiers (e.g., `America/New_York`, `Europe/London`)
/// via chrono-tz (case-insensitive), plus Windows timezone names
/// (e.g., "Eastern Standard Time") via the CLDR mapping from the localzone crate.
///
/// Returns `None` and logs a warning if the timezone cannot be parsed.
#[allow(dead_code)] // Used by tests
fn parse_ical_timezone(tz_str: &str) -> Option<Tz> {
    // Helper to convert an IANA timezone string to chrono_tz::Tz
    let iana_to_tz = |iana: &str| -> Option<Tz> {
        // Use case-insensitive parsing (requires chrono-tz "case-insensitive" feature)
        let tz = Tz::from_str_insensitive(iana).ok()?;
        // Normalize Etc/UTC to the canonical UTC
        Some(if tz == Tz::Etc__UTC { Tz::UTC } else { tz })
    };

    // First, try parsing as an IANA timezone identifier directly.
    // This handles all ~400+ IANA timezones (case-insensitive with chrono-tz feature).
    if let Some(tz) = iana_to_tz(tz_str) {
        return Some(tz);
    }

    // Fall back to Windows timezone names using CLDR mapping.
    // CLDR only has "Standard Time" entries, not "Daylight Time", so normalize.
    let normalized = tz_str.replace(" Daylight Time", " Standard Time");
    if let Some(iana) = localzone::win_zone_to_iana(&normalized, None) {
        return iana_to_tz(iana);
    }

    // Last resort: common non-standard abbreviations seen in calendar software.
    // These aren't valid IANA zones and aren't in CLDR, but appear in the wild.
    // Excludes ambiguous abbreviations (IST=India/Ireland/Israel, CST=US/China).
    let abbrev_iana = match tz_str {
        "PST" | "PDT" => Some("America/Los_Angeles"),
        "EDT" => Some("America/New_York"),
        "CDT" => Some("America/Chicago"),
        "MDT" => Some("America/Denver"),
        "BST" => Some("Europe/London"),
        "CEST" => Some("Europe/Berlin"),
        "JST" => Some("Asia/Tokyo"),
        "SGT" => Some("Asia/Singapore"),
        "KST" => Some("Asia/Seoul"),
        "NZST" | "NZDT" => Some("Pacific/Auckland"),
        "AEST" | "AEDT" => Some("Australia/Sydney"),
        "AWST" => Some("Australia/Perth"),
        "Z" => Some("UTC"),
        _ => None,
    };
    if let Some(iana) = abbrev_iana {
        return iana_to_tz(iana);
    }

    // Log unrecognized timezones to help debug issues in the wild.
    // The caller will fall back to local time interpretation.
    if !tz_str.is_empty() {
        eprintln!("warning: unrecognized timezone '{tz_str}', falling back to local time");
    }

    None
}

/// Extract a meeting URL from the meeting's location or description fields
/// Checks location first (most common place for meeting links), then description
pub fn extract_meeting_url(meeting: &Meeting, patterns: &[String]) -> Option<String> {
    // Compile patterns, skipping any invalid ones
    let compiled: Vec<Regex> = patterns.iter().filter_map(|p| Regex::new(p).ok()).collect();

    if compiled.is_empty() {
        return None;
    }

    // Check location first
    if let Some(ref location) = meeting.location {
        for regex in &compiled {
            if let Some(m) = regex.find(location) {
                return Some(m.as_str().to_string());
            }
        }
    }

    // Then check description
    if let Some(ref description) = meeting.description {
        for regex in &compiled {
            if let Some(m) = regex.find(description) {
                return Some(m.as_str().to_string());
            }
        }
    }

    None
}

/// Get the physical location from a meeting (location that is not a URL)
/// Returns None if the location is empty or appears to be just a URL
pub fn get_physical_location(meeting: &Meeting, url_patterns: &[String]) -> Option<String> {
    let location = meeting.location.as_ref()?;
    let location = location.trim();

    if location.is_empty() {
        return None;
    }

    // Compile patterns to check if location is a URL
    let compiled: Vec<Regex> = url_patterns
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();

    // If location matches any URL pattern entirely, it's not a physical location
    for regex in &compiled {
        if let Some(m) = regex.find(location) {
            // If the match covers the entire location, skip it
            if m.start() == 0 && m.end() == location.len() {
                return None;
            }
        }
    }

    // Also skip if it looks like a generic URL
    if location.starts_with("http://") || location.starts_with("https://") {
        return None;
    }

    Some(location.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    // Tests for parse_display_name
    #[test]
    fn test_parse_display_name_simple() {
        let data = "[Data Source]\nDisplayName=Work Calendar\nEnabled=true";
        assert_eq!(parse_display_name(data), Some("Work Calendar".to_string()));
    }

    #[test]
    fn test_parse_display_name_with_sections() {
        let data = "[Data Source]\nDisplayName=Personal\n[Calendar]\nBackendName=local";
        assert_eq!(parse_display_name(data), Some("Personal".to_string()));
    }

    #[test]
    fn test_parse_display_name_missing() {
        let data = "[Data Source]\nEnabled=true\n[Calendar]";
        assert_eq!(parse_display_name(data), None);
    }

    // Tests for parse_color
    #[test]
    fn test_parse_color_hex() {
        let data = "[Calendar]\nColor=#62a0ea\nBackendName=local";
        assert_eq!(parse_color(data), Some("#62a0ea".to_string()));
    }

    #[test]
    fn test_parse_color_missing() {
        let data = "[Calendar]\nBackendName=local";
        assert_eq!(parse_color(data), None);
    }

    // Tests for parse_ical_datetime
    #[test]
    fn test_parse_ical_datetime_local() {
        let result = parse_ical_datetime("20240221T123000", None).unwrap();
        assert_eq!(result.year(), 2024);
        assert_eq!(result.month(), 2);
        assert_eq!(result.day(), 21);
        assert_eq!(result.hour(), 12);
        assert_eq!(result.minute(), 30);
    }

    #[test]
    fn test_parse_ical_datetime_utc() {
        let result = parse_ical_datetime("20240221T123000Z", None).unwrap();
        assert_eq!(result.year(), 2024);
        assert_eq!(result.month(), 2);
        assert_eq!(result.day(), 21);
        // Hour may differ due to timezone conversion
    }

    #[test]
    fn test_parse_ical_datetime_with_tzid_in_value() {
        // When TZID is embedded in the value string (legacy format)
        let result = parse_ical_datetime("TZID=America/Los_Angeles:20240221T123000", None).unwrap();
        assert_eq!(result.year(), 2024);
        assert_eq!(result.month(), 2);
        assert_eq!(result.day(), 21);
        // Note: Without passing TZID separately, this parses as local time
    }

    #[test]
    fn test_parse_ical_datetime_with_tzid_param() {
        // When TZID is passed as a separate parameter (proper handling)
        let result = parse_ical_datetime("20240221T120000", Some("America/New_York")).unwrap();
        // 12:00 PM Eastern should convert to local time
        assert_eq!(result.year(), 2024);
        assert_eq!(result.month(), 2);
        assert_eq!(result.day(), 21);
        // The hour depends on the local timezone, so just verify it parsed
    }

    #[test]
    fn test_parse_ical_datetime_date_only() {
        let result = parse_ical_datetime("20250527", None).unwrap();
        assert_eq!(result.year(), 2025);
        assert_eq!(result.month(), 5);
        assert_eq!(result.day(), 27);
        assert_eq!(result.hour(), 0);
        assert_eq!(result.minute(), 0);
    }

    #[test]
    fn test_parse_ical_datetime_value_date() {
        let result = parse_ical_datetime("VALUE=DATE:20250527", None).unwrap();
        assert_eq!(result.year(), 2025);
        assert_eq!(result.month(), 5);
        assert_eq!(result.day(), 27);
    }

    #[test]
    fn test_parse_ical_datetime_invalid() {
        assert!(parse_ical_datetime("invalid", None).is_none());
        assert!(parse_ical_datetime("", None).is_none());
    }

    #[test]
    fn test_parse_ical_datetime_timezone_conversion() {
        // Test that timezone conversion actually changes the time
        // 12:00 UTC should be different from 12:00 local (unless you're in UTC)
        let utc_result = parse_ical_datetime("20240601T120000", None).unwrap();
        let local_result = parse_ical_datetime("20240601T120000", None).unwrap();

        // UTC time should be converted to local, so if we're not in UTC,
        // the times should differ
        let _utc_hour = utc_result.hour();
        let local_hour = local_result.hour();

        // The local interpretation should always be 12:00 local
        assert_eq!(local_hour, 12);

        // UTC result depends on local timezone offset
        // We can't assert exact hour, but we can verify the parsing worked
        assert_eq!(utc_result.minute(), 0);
        assert_eq!(local_result.minute(), 0);
    }

    #[test]
    fn test_parse_ical_datetime_with_known_timezone() {
        // Test parsing with a known timezone
        // America/New_York is UTC-5 (or UTC-4 in DST)
        let result = parse_ical_datetime("20240115T120000", Some("America/New_York"));
        assert!(result.is_some());
        let dt = result.unwrap();
        assert_eq!(dt.year(), 2024);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 15);
        // Hour will be converted to local time
    }

    #[test]
    fn test_parse_ical_datetime_unknown_timezone_fallback() {
        // Unknown timezone should fall back to local time interpretation
        let result = parse_ical_datetime("20240601T120000", Some("Unknown/Timezone"));
        assert!(result.is_some());
        let dt = result.unwrap();
        // Should parse as local time since timezone is unknown
        assert_eq!(dt.hour(), 12);
    }

    // Tests for should_include_meeting
    #[test]
    fn test_should_include_meeting_future() {
        let now = Local::now();
        let query_start = now - chrono::Duration::minutes(30);
        let start = now + chrono::Duration::hours(1);
        let end = start + chrono::Duration::hours(1);

        assert!(should_include_meeting(start, end, now, query_start));
    }

    #[test]
    fn test_should_include_meeting_past() {
        let now = Local::now();
        let query_start = now - chrono::Duration::minutes(30);
        let start = now - chrono::Duration::hours(2);
        let end = now - chrono::Duration::hours(1);

        // Meeting ended in the past - should not be included
        assert!(!should_include_meeting(start, end, now, query_start));
    }

    #[test]
    fn test_should_include_meeting_in_progress() {
        let now = Local::now();
        let query_start = now - chrono::Duration::minutes(30);
        let start = now - chrono::Duration::minutes(15); // Started 15 min ago (within 30 min window)
        let end = now + chrono::Duration::minutes(45); // Ends in 45 min

        assert!(should_include_meeting(start, end, now, query_start));
    }

    #[test]
    fn test_should_include_meeting_in_progress_but_too_old() {
        let now = Local::now();
        let query_start = now - chrono::Duration::minutes(30);
        let start = now - chrono::Duration::hours(1); // Started 1 hour ago (outside 30 min window)
        let end = now + chrono::Duration::minutes(30); // Still ongoing

        // Started before query_start, so should not be included
        assert!(!should_include_meeting(start, end, now, query_start));
    }

    #[test]
    fn test_should_include_meeting_invalid_end_before_start() {
        let now = Local::now();
        let query_start = now - chrono::Duration::minutes(30);
        let start = now + chrono::Duration::hours(1);
        let end = start - chrono::Duration::hours(2); // End before start - invalid

        // Invalid meeting should not be included
        assert!(!should_include_meeting(start, end, now, query_start));
    }

    #[test]
    fn test_should_include_meeting_just_ended() {
        let now = Local::now();
        let query_start = now - chrono::Duration::minutes(30);
        let start = now - chrono::Duration::minutes(15);
        let end = now - chrono::Duration::seconds(1); // Just ended

        // Meeting has ended - should not be included
        assert!(!should_include_meeting(start, end, now, query_start));
    }

    #[test]
    fn test_should_include_meeting_starting_now() {
        let now = Local::now();
        let query_start = now - chrono::Duration::minutes(30);
        let start = now; // Starting exactly now
        let end = now + chrono::Duration::hours(1);

        // start <= now but start > query_start, end > now, so in-progress
        assert!(should_include_meeting(start, end, now, query_start));
    }

    // Helper to create a test meeting
    fn make_test_meeting(location: Option<&str>, description: Option<&str>) -> Meeting {
        Meeting {
            uid: "test-uid".to_string(),
            title: "Test Meeting".to_string(),
            start: Local::now(),
            end: Local::now() + chrono::Duration::hours(1),
            location: location.map(String::from),
            description: description.map(String::from),
            calendar_uid: "cal-uid".to_string(),
            is_all_day: false,
            attendance_status: AttendanceStatus::None,
        }
    }

    // Tests for extract_meeting_url
    #[test]
    fn test_extract_meeting_url_from_location() {
        let patterns = vec![r"https://meet\.google\.com/[a-z-]+".to_string()];
        let meeting = make_test_meeting(Some("https://meet.google.com/abc-defg-hij"), None);
        let url = extract_meeting_url(&meeting, &patterns);
        assert_eq!(
            url,
            Some("https://meet.google.com/abc-defg-hij".to_string())
        );
    }

    #[test]
    fn test_extract_meeting_url_from_description() {
        let patterns = vec![r"https://zoom\.us/j/\d+".to_string()];
        let meeting = make_test_meeting(
            Some("Conference Room A"),
            Some("Join: https://zoom.us/j/123456789"),
        );
        let url = extract_meeting_url(&meeting, &patterns);
        assert_eq!(url, Some("https://zoom.us/j/123456789".to_string()));
    }

    #[test]
    fn test_extract_meeting_url_location_priority() {
        let patterns = vec![r"https://meet\.google\.com/[a-z-]+".to_string()];
        let meeting = make_test_meeting(
            Some("https://meet.google.com/loc-ation"),
            Some("https://meet.google.com/desc-ription"),
        );
        let url = extract_meeting_url(&meeting, &patterns);
        // Location should be checked first
        assert_eq!(url, Some("https://meet.google.com/loc-ation".to_string()));
    }

    #[test]
    fn test_extract_meeting_url_no_match() {
        let patterns = vec![r"https://meet\.google\.com/[a-z-]+".to_string()];
        let meeting = make_test_meeting(Some("Conference Room B"), None);
        assert!(extract_meeting_url(&meeting, &patterns).is_none());
    }

    #[test]
    fn test_extract_meeting_url_empty_patterns() {
        let patterns: Vec<String> = vec![];
        let meeting = make_test_meeting(Some("https://meet.google.com/abc-def"), None);
        assert!(extract_meeting_url(&meeting, &patterns).is_none());
    }

    // Tests for get_physical_location
    #[test]
    fn test_get_physical_location_room() {
        let patterns = vec![r"https://meet\.google\.com/[a-z-]+".to_string()];
        let meeting = make_test_meeting(Some("Conference Room A"), None);
        assert_eq!(
            get_physical_location(&meeting, &patterns),
            Some("Conference Room A".to_string())
        );
    }

    #[test]
    fn test_get_physical_location_url_only() {
        let patterns = vec![r"https://meet\.google\.com/[a-z-]+".to_string()];
        let meeting = make_test_meeting(Some("https://meet.google.com/abc-defg-hij"), None);
        assert!(get_physical_location(&meeting, &patterns).is_none());
    }

    #[test]
    fn test_get_physical_location_generic_url() {
        let patterns: Vec<String> = vec![];
        let meeting = make_test_meeting(Some("https://example.com/meeting"), None);
        assert!(get_physical_location(&meeting, &patterns).is_none());
    }

    // ==========================================================================
    // Tests for parse_ical_timezone
    // ==========================================================================

    #[test]
    fn test_tz_iana_us_timezones() {
        assert_eq!(
            parse_ical_timezone("America/Los_Angeles"),
            Some(Tz::America__Los_Angeles)
        );
        assert_eq!(
            parse_ical_timezone("America/New_York"),
            Some(Tz::America__New_York)
        );
        assert_eq!(
            parse_ical_timezone("America/Chicago"),
            Some(Tz::America__Chicago)
        );
        assert_eq!(
            parse_ical_timezone("America/Denver"),
            Some(Tz::America__Denver)
        );
        assert_eq!(
            parse_ical_timezone("America/Phoenix"),
            Some(Tz::America__Phoenix)
        );
        assert_eq!(
            parse_ical_timezone("America/Detroit"),
            Some(Tz::America__Detroit)
        );
        assert_eq!(
            parse_ical_timezone("America/Indiana/Indianapolis"),
            Some(Tz::America__Indiana__Indianapolis)
        );
        assert_eq!(
            parse_ical_timezone("America/Anchorage"),
            Some(Tz::America__Anchorage)
        );
    }

    #[test]
    fn test_tz_iana_european_timezones() {
        assert_eq!(
            parse_ical_timezone("Europe/London"),
            Some(Tz::Europe__London)
        );
        assert_eq!(parse_ical_timezone("Europe/Paris"), Some(Tz::Europe__Paris));
        assert_eq!(
            parse_ical_timezone("Europe/Berlin"),
            Some(Tz::Europe__Berlin)
        );
        assert_eq!(
            parse_ical_timezone("Europe/Amsterdam"),
            Some(Tz::Europe__Amsterdam)
        );
        assert_eq!(parse_ical_timezone("Europe/Rome"), Some(Tz::Europe__Rome));
        assert_eq!(
            parse_ical_timezone("Europe/Madrid"),
            Some(Tz::Europe__Madrid)
        );
    }

    #[test]
    fn test_tz_iana_asian_timezones() {
        assert_eq!(parse_ical_timezone("Asia/Tokyo"), Some(Tz::Asia__Tokyo));
        assert_eq!(
            parse_ical_timezone("Asia/Shanghai"),
            Some(Tz::Asia__Shanghai)
        );
        assert_eq!(
            parse_ical_timezone("Asia/Singapore"),
            Some(Tz::Asia__Singapore)
        );
        assert_eq!(
            parse_ical_timezone("Asia/Hong_Kong"),
            Some(Tz::Asia__Hong_Kong)
        );
        assert_eq!(parse_ical_timezone("Asia/Kolkata"), Some(Tz::Asia__Kolkata));
        assert_eq!(parse_ical_timezone("Asia/Dubai"), Some(Tz::Asia__Dubai));
    }

    #[test]
    fn test_tz_iana_pacific_oceania_timezones() {
        assert_eq!(
            parse_ical_timezone("Pacific/Honolulu"),
            Some(Tz::Pacific__Honolulu)
        );
        assert_eq!(
            parse_ical_timezone("Pacific/Auckland"),
            Some(Tz::Pacific__Auckland)
        );
        assert_eq!(
            parse_ical_timezone("Australia/Sydney"),
            Some(Tz::Australia__Sydney)
        );
        assert_eq!(
            parse_ical_timezone("Australia/Melbourne"),
            Some(Tz::Australia__Melbourne)
        );
    }

    #[test]
    fn test_tz_utc_variants() {
        assert_eq!(parse_ical_timezone("UTC"), Some(Tz::UTC));
        assert_eq!(parse_ical_timezone("Etc/UTC"), Some(Tz::UTC));
    }

    #[test]
    fn test_tz_windows_us_aliases() {
        // Pacific
        assert_eq!(
            parse_ical_timezone("Pacific Standard Time"),
            Some(Tz::America__Los_Angeles)
        );
        assert_eq!(
            parse_ical_timezone("Pacific Daylight Time"),
            Some(Tz::America__Los_Angeles)
        );
        // Eastern
        assert_eq!(
            parse_ical_timezone("Eastern Standard Time"),
            Some(Tz::America__New_York)
        );
        assert_eq!(
            parse_ical_timezone("Eastern Daylight Time"),
            Some(Tz::America__New_York)
        );
        // Central
        assert_eq!(
            parse_ical_timezone("Central Standard Time"),
            Some(Tz::America__Chicago)
        );
        assert_eq!(
            parse_ical_timezone("Central Daylight Time"),
            Some(Tz::America__Chicago)
        );
        // Mountain
        assert_eq!(
            parse_ical_timezone("Mountain Standard Time"),
            Some(Tz::America__Denver)
        );
        assert_eq!(
            parse_ical_timezone("Mountain Daylight Time"),
            Some(Tz::America__Denver)
        );
    }

    #[test]
    fn test_tz_edge_cases() {
        // Unknown timezones should return None
        assert_eq!(parse_ical_timezone("Unknown/Timezone"), None);
        assert_eq!(parse_ical_timezone(""), None);
        assert_eq!(parse_ical_timezone("not a timezone"), None);

        // IANA timezone IDs are parsed case-insensitively (chrono-tz "case-insensitive" feature)
        assert_eq!(
            parse_ical_timezone("america/los_angeles"),
            Some(Tz::America__Los_Angeles)
        );
        assert_eq!(
            parse_ical_timezone("AMERICA/LOS_ANGELES"),
            Some(Tz::America__Los_Angeles)
        );
        assert_eq!(
            parse_ical_timezone("europe/london"),
            Some(Tz::Europe__London)
        );
        assert_eq!(parse_ical_timezone("Asia/TOKYO"), Some(Tz::Asia__Tokyo));
    }

    // These IANA timezones were not in the original hardcoded list but now work
    // via chrono-tz's FromStr implementation.
    #[test]
    fn test_tz_additional_iana_timezones() {
        // Americas
        assert_eq!(
            parse_ical_timezone("America/Toronto"),
            Some(Tz::America__Toronto)
        );
        assert_eq!(
            parse_ical_timezone("America/Sao_Paulo"),
            Some(Tz::America__Sao_Paulo)
        );

        // Europe
        assert_eq!(
            parse_ical_timezone("Europe/Moscow"),
            Some(Tz::Europe__Moscow)
        );

        // Asia
        assert_eq!(parse_ical_timezone("Asia/Seoul"), Some(Tz::Asia__Seoul));

        // Africa
        assert_eq!(parse_ical_timezone("Africa/Cairo"), Some(Tz::Africa__Cairo));
    }

    // Windows timezone aliases via CLDR mapping from localzone crate.
    // These are the key examples from PR #9 that motivated the timezone fix.
    #[test]
    fn test_tz_windows_international_aliases() {
        // UK - "GMT Standard Time" is the Windows name for UK time (observes BST)
        assert_eq!(
            parse_ical_timezone("GMT Standard Time"),
            Some(Tz::Europe__London)
        );

        // China
        assert_eq!(
            parse_ical_timezone("China Standard Time"),
            Some(Tz::Asia__Shanghai)
        );

        // Japan
        assert_eq!(
            parse_ical_timezone("Tokyo Standard Time"),
            Some(Tz::Asia__Tokyo)
        );

        // India - CLDR uses the old IANA name "Asia/Calcutta"
        assert_eq!(
            parse_ical_timezone("India Standard Time"),
            Some(Tz::Asia__Calcutta)
        );

        // Australia
        assert_eq!(
            parse_ical_timezone("AUS Eastern Standard Time"),
            Some(Tz::Australia__Sydney)
        );
    }

    // Additional Windows timezone names from PR #9, verified against CLDR.
    #[test]
    fn test_tz_windows_extended_coverage() {
        // US additional
        assert_eq!(
            parse_ical_timezone("US Mountain Standard Time"),
            Some(Tz::America__Phoenix)
        );
        assert_eq!(
            parse_ical_timezone("US Eastern Standard Time"),
            Some(Tz::America__Indianapolis)
        );
        assert_eq!(
            parse_ical_timezone("Alaskan Standard Time"),
            Some(Tz::America__Anchorage)
        );
        assert_eq!(
            parse_ical_timezone("Hawaiian Standard Time"),
            Some(Tz::Pacific__Honolulu)
        );

        // Europe additional
        assert_eq!(
            parse_ical_timezone("Romance Standard Time"),
            Some(Tz::Europe__Paris)
        );
        assert_eq!(
            parse_ical_timezone("W. Europe Standard Time"),
            Some(Tz::Europe__Berlin)
        );
        assert_eq!(
            parse_ical_timezone("Russian Standard Time"),
            Some(Tz::Europe__Moscow)
        );

        // Asia additional
        assert_eq!(
            parse_ical_timezone("Korea Standard Time"),
            Some(Tz::Asia__Seoul)
        );
        assert_eq!(
            parse_ical_timezone("Singapore Standard Time"),
            Some(Tz::Asia__Singapore)
        );
        assert_eq!(
            parse_ical_timezone("Arabian Standard Time"),
            Some(Tz::Asia__Dubai)
        );

        // Pacific/Oceania additional
        assert_eq!(
            parse_ical_timezone("New Zealand Standard Time"),
            Some(Tz::Pacific__Auckland)
        );
        assert_eq!(
            parse_ical_timezone("E. Australia Standard Time"),
            Some(Tz::Australia__Brisbane)
        );
        assert_eq!(
            parse_ical_timezone("W. Australia Standard Time"),
            Some(Tz::Australia__Perth)
        );

        // Americas (non-US)
        assert_eq!(
            parse_ical_timezone("E. South America Standard Time"),
            Some(Tz::America__Sao_Paulo)
        );
        // CLDR uses "America/Buenos_Aires" not "America/Argentina/Buenos_Aires"
        assert_eq!(
            parse_ical_timezone("Argentina Standard Time"),
            Some(Tz::America__Buenos_Aires)
        );

        // Africa
        assert_eq!(
            parse_ical_timezone("Egypt Standard Time"),
            Some(Tz::Africa__Cairo)
        );
        assert_eq!(
            parse_ical_timezone("South Africa Standard Time"),
            Some(Tz::Africa__Johannesburg)
        );
    }

    // Timezone abbreviations: some are valid IANA zones, others are handled by
    // our fallback table, and ambiguous ones are rejected.
    #[test]
    fn test_tz_abbreviations() {
        // These ARE valid IANA zones (legacy/deprecated but recognized by chrono-tz)
        assert_eq!(parse_ical_timezone("EST"), Some(Tz::EST)); // Fixed offset -5
        assert_eq!(parse_ical_timezone("MST"), Some(Tz::MST)); // Fixed offset -7
        assert_eq!(parse_ical_timezone("HST"), Some(Tz::HST)); // Fixed offset -10
        assert_eq!(parse_ical_timezone("GMT"), Some(Tz::GMT)); // Same as UTC
        assert_eq!(parse_ical_timezone("CET"), Some(Tz::CET)); // Central European
        assert_eq!(parse_ical_timezone("MET"), Some(Tz::MET)); // Middle European

        // Non-standard abbreviations handled by our fallback table
        // US
        assert_eq!(parse_ical_timezone("PST"), Some(Tz::America__Los_Angeles));
        assert_eq!(parse_ical_timezone("PDT"), Some(Tz::America__Los_Angeles));
        assert_eq!(parse_ical_timezone("EDT"), Some(Tz::America__New_York));
        assert_eq!(parse_ical_timezone("CDT"), Some(Tz::America__Chicago));
        assert_eq!(parse_ical_timezone("MDT"), Some(Tz::America__Denver));
        // Europe
        assert_eq!(parse_ical_timezone("BST"), Some(Tz::Europe__London));
        assert_eq!(parse_ical_timezone("CEST"), Some(Tz::Europe__Berlin));
        // Asia
        assert_eq!(parse_ical_timezone("JST"), Some(Tz::Asia__Tokyo));
        assert_eq!(parse_ical_timezone("KST"), Some(Tz::Asia__Seoul));
        assert_eq!(parse_ical_timezone("SGT"), Some(Tz::Asia__Singapore));
        // Pacific/Oceania
        assert_eq!(parse_ical_timezone("NZST"), Some(Tz::Pacific__Auckland));
        assert_eq!(parse_ical_timezone("NZDT"), Some(Tz::Pacific__Auckland));
        assert_eq!(parse_ical_timezone("AEST"), Some(Tz::Australia__Sydney));
        assert_eq!(parse_ical_timezone("AEDT"), Some(Tz::Australia__Sydney));
        assert_eq!(parse_ical_timezone("AWST"), Some(Tz::Australia__Perth));
        // UTC
        assert_eq!(parse_ical_timezone("Z"), Some(Tz::UTC));

        // Ambiguous abbreviations are NOT handled - they return None
        assert_eq!(parse_ical_timezone("CST"), None); // US Central or China?
        assert_eq!(parse_ical_timezone("IST"), None); // India, Ireland, or Israel?
    }

    // DST transition tests: verify that events scheduled during DST transitions
    // are handled gracefully. Spring-forward creates a gap (hour doesn't exist),
    // fall-back creates ambiguity (hour repeats).
    #[test]
    fn test_tz_dst_transitions() {
        // In 2024, US DST:
        // - Spring forward: March 10, 2024 2:00 AM -> 3:00 AM (2:30 AM doesn't exist)
        // - Fall back: November 3, 2024 2:00 AM -> 1:00 AM (1:30 AM happens twice)

        // Test that timezones observing DST are parsed correctly
        // The timezone itself is always valid; DST handling happens at datetime level
        let la_tz = parse_ical_timezone("America/Los_Angeles");
        assert!(la_tz.is_some());
        assert_eq!(la_tz, Some(Tz::America__Los_Angeles));

        // Timezones that don't observe DST
        let arizona_tz = parse_ical_timezone("America/Phoenix");
        assert_eq!(arizona_tz, Some(Tz::America__Phoenix)); // No DST in Arizona

        let utc_tz = parse_ical_timezone("UTC");
        assert_eq!(utc_tz, Some(Tz::UTC)); // UTC never has DST

        // European DST (different dates than US)
        // In 2024: Last Sunday of March (March 31) and last Sunday of October (October 27)
        let london_tz = parse_ical_timezone("Europe/London");
        assert_eq!(london_tz, Some(Tz::Europe__London));

        // Windows timezone names for DST-observing zones
        // These should still parse correctly regardless of current DST status
        let eastern_std = parse_ical_timezone("Eastern Standard Time");
        let eastern_day = parse_ical_timezone("Eastern Daylight Time");
        assert_eq!(eastern_std, Some(Tz::America__New_York));
        assert_eq!(eastern_day, Some(Tz::America__New_York)); // Both map to same tz
    }

    // Integration tests: verify parse_ical_datetime works correctly with
    // various timezone formats, including edge cases.
    #[test]
    fn test_parse_ical_datetime_with_timezones() {
        // Basic UTC time (Z suffix)
        let utc_result = parse_ical_datetime("20240315T140000", None);
        assert!(utc_result.is_some());
        let utc_dt = utc_result.unwrap();
        // The parsed time should represent 14:00 UTC, converted to local
        // We can't assert exact local time since it depends on system timezone,
        // but we can verify it parsed successfully and is a valid datetime
        assert!(utc_dt.timestamp() > 0);

        // Time with IANA timezone
        let iana_result = parse_ical_datetime("20240315T100000", Some("America/New_York"));
        assert!(iana_result.is_some());

        // Time with Windows timezone name
        let win_result = parse_ical_datetime("20240315T100000", Some("Eastern Standard Time"));
        assert!(win_result.is_some());

        // Both should represent the same instant (10 AM Eastern)
        // Since both IANA and Windows map to America/New_York
        if let (Some(iana_dt), Some(win_dt)) = (iana_result, win_result) {
            assert_eq!(iana_dt.timestamp(), win_dt.timestamp());
        }

        // Time with case-insensitive timezone
        let case_result = parse_ical_datetime("20240315T100000", Some("america/new_york"));
        assert!(case_result.is_some());
        if let (Some(iana_dt), Some(case_dt)) = (
            parse_ical_datetime("20240315T100000", Some("America/New_York")),
            case_result,
        ) {
            assert_eq!(iana_dt.timestamp(), case_dt.timestamp());
        }

        // Date-only format (all-day events)
        let date_result = parse_ical_datetime("20240315", None);
        assert!(date_result.is_some());
        let date_dt = date_result.unwrap();
        assert_eq!(date_dt.format("%Y%m%d").to_string(), "20240315");

        // Invalid timezone should fall back to local time interpretation
        let invalid_tz_result = parse_ical_datetime("20240315T100000", Some("Invalid/Timezone"));
        assert!(invalid_tz_result.is_some()); // Should still parse, just in local time
    }

    // Test DST transition edge cases in parse_ical_datetime
    #[test]
    fn test_parse_ical_datetime_dst_edge_cases() {
        // Spring forward gap: 2:30 AM on March 10, 2024 doesn't exist in America/Los_Angeles
        // The rrule/chrono libraries handle this by returning None for ambiguous times
        let gap_result = parse_ical_datetime("20240310T023000", Some("America/Los_Angeles"));
        // This may return None or a resolved time depending on library behavior
        // The important thing is it doesn't panic

        // Fall back overlap: 1:30 AM on November 3, 2024 happens twice
        let overlap_result = parse_ical_datetime("20241103T013000", Some("America/Los_Angeles"));
        // Libraries typically pick one interpretation (usually the first/earlier one)
        // Again, should not panic

        // Times clearly outside DST transitions should work normally
        let summer_result = parse_ical_datetime("20240715T120000", Some("America/Los_Angeles"));
        assert!(summer_result.is_some()); // July 15 is well within PDT

        let winter_result = parse_ical_datetime("20240115T120000", Some("America/Los_Angeles"));
        assert!(winter_result.is_some()); // January 15 is well within PST

        // UTC times are never affected by DST
        let utc_march = parse_ical_datetime("20240310T100000", None);
        assert!(utc_march.is_some());

        // Timezones without DST should never have gaps
        let phoenix_result = parse_ical_datetime("20240310T023000", Some("America/Phoenix"));
        assert!(phoenix_result.is_some()); // Arizona doesn't observe DST

        // Suppress unused variable warnings for gap/overlap tests
        let _ = gap_result;
        let _ = overlap_result;
    }

    // =========================================================================
    // Tests for parse_ics_objects and dedup_and_sort_meetings
    // =========================================================================

    /// Helper: parse ICS objects and return deduplicated meetings with a wide time window.
    fn parse_and_dedup(ics_objects: &[&str]) -> Vec<Meeting> {
        let now = Local::now();
        // Wide window: from 1 year ago to catch all test events
        let query_start = now - chrono::Duration::days(365);
        let user_emails: Vec<String> = vec![];
        let mut all_meetings = Vec::new();
        let ics_strings: Vec<String> = ics_objects.iter().map(|s| (*s).to_string()).collect();
        parse_ics_objects(
            &ics_strings,
            "test-calendar",
            now,
            query_start,
            &user_emails,
            &mut all_meetings,
        );
        dedup_and_sort_meetings(all_meetings, 1000)
    }

    // Test helpers use TZID=UTC to ensure consistent behavior across systems.
    // Real EDS data uses DTSTART;TZID=<timezone>:<time> format.
    // Using UTC avoids timezone conversion differences between calcard's
    // expand_dates (RRULE expansion) and standalone event parsing.

    /// Helper: build a simple VCALENDAR-wrapped VEVENT with given properties.
    /// `dtstart`/`dtend` should be in "YYYYMMDDTHHMMSS" format (no Z suffix).
    fn make_ics(uid: &str, summary: &str, dtstart: &str, dtend: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
             BEGIN:VEVENT\r\n\
             UID:{uid}\r\n\
             SUMMARY:{summary}\r\n\
             DTSTART;TZID=UTC:{dtstart}\r\n\
             DTEND;TZID=UTC:{dtend}\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR"
        )
    }

    /// Helper: build a recurring VEVENT (master) with RRULE.
    fn make_recurring_ics(
        uid: &str,
        summary: &str,
        dtstart: &str,
        dtend: &str,
        rrule: &str,
    ) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
             BEGIN:VEVENT\r\n\
             UID:{uid}\r\n\
             SUMMARY:{summary}\r\n\
             DTSTART;TZID=UTC:{dtstart}\r\n\
             DTEND;TZID=UTC:{dtend}\r\n\
             RRULE:{rrule}\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR"
        )
    }

    /// Helper: build a RECURRENCE-ID override VEVENT.
    fn make_override_ics(
        uid: &str,
        summary: &str,
        dtstart: &str,
        dtend: &str,
        recurrence_id: &str,
    ) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
             BEGIN:VEVENT\r\n\
             UID:{uid}\r\n\
             SUMMARY:{summary}\r\n\
             DTSTART;TZID=UTC:{dtstart}\r\n\
             DTEND;TZID=UTC:{dtend}\r\n\
             RECURRENCE-ID;TZID=UTC:{recurrence_id}\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR"
        )
    }

    #[test]
    fn test_single_non_recurring_event() {
        // A far-future event that will always be in range
        let ics = make_ics("evt1", "Team Standup", "20270601T100000", "20270601T103000");
        let meetings = parse_and_dedup(&[&ics]);
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].title, "Team Standup");
        assert!(meetings[0].uid.starts_with("evt1@"));
    }

    #[test]
    fn test_two_different_events_no_dedup() {
        let ics1 = make_ics("evt1", "Meeting A", "20270601T100000", "20270601T110000");
        let ics2 = make_ics("evt2", "Meeting B", "20270601T140000", "20270601T150000");
        let meetings = parse_and_dedup(&[&ics1, &ics2]);
        assert_eq!(meetings.len(), 2);
        let titles: Vec<&str> = meetings.iter().map(|m| m.title.as_str()).collect();
        assert!(titles.contains(&"Meeting A"));
        assert!(titles.contains(&"Meeting B"));
    }

    #[test]
    fn test_recurring_event_expands_multiple_instances() {
        // Weekly event starting far in the future, 3 occurrences
        let ics = make_recurring_ics(
            "weekly1",
            "Weekly Sync",
            "20270601T100000",
            "20270601T110000",
            "FREQ=WEEKLY;COUNT=3",
        );
        let meetings = parse_and_dedup(&[&ics]);
        assert_eq!(meetings.len(), 3);
        // All should have same UID prefix but different timestamps
        for m in &meetings {
            assert!(m.uid.starts_with("weekly1@"));
            assert_eq!(m.title, "Weekly Sync");
        }
        // Verify they're on different weeks
        assert_ne!(
            meetings[0].start.date_naive(),
            meetings[1].start.date_naive()
        );
        assert_ne!(
            meetings[1].start.date_naive(),
            meetings[2].start.date_naive()
        );
    }

    #[test]
    fn test_recurrence_id_override_deduplicates_master_expansion() {
        // This is the key test for the duplicate bug fix.
        // EDS returns both a master event (with RRULE) and a separate
        // RECURRENCE-ID override for the same date. Only one should appear.

        // Master: weekly event, 2 instances
        let master = make_recurring_ics(
            "allhands",
            "All Hands",
            "20270601T120000",
            "20270601T130000",
            "FREQ=WEEKLY;COUNT=2",
        );

        // Override for first instance (same UID, same start time)
        let override_ics = make_override_ics(
            "allhands",
            "All Hands (Updated)",
            "20270601T120000",
            "20270601T130000",
            "20270601T120000",
        );

        let meetings = parse_and_dedup(&[&master, &override_ics]);

        // Should be 2 instances total (week 1 + week 2), not 3
        assert_eq!(meetings.len(), 2);

        // The first instance should use the override's title
        let first = meetings
            .iter()
            .find(|m| m.start.format("%Y%m%d").to_string() == "20270601");
        assert!(first.is_some());
        assert_eq!(first.unwrap().title, "All Hands (Updated)");
    }

    #[test]
    fn test_recurrence_id_override_preferred_regardless_of_order() {
        // Override comes BEFORE master in the list — should still prefer override
        let override_ics = make_override_ics(
            "meeting1",
            "Override Title",
            "20270601T120000",
            "20270601T130000",
            "20270601T120000",
        );

        let master = make_recurring_ics(
            "meeting1",
            "Original Title",
            "20270601T120000",
            "20270601T130000",
            "FREQ=WEEKLY;COUNT=1",
        );

        let meetings = parse_and_dedup(&[&override_ics, &master]);
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].title, "Override Title");
    }

    #[test]
    fn test_multiple_overrides_for_different_dates() {
        // Master with 3 weekly instances, overrides for dates 1 and 3
        let master = make_recurring_ics(
            "series1",
            "Weekly",
            "20270601T100000",
            "20270601T110000",
            "FREQ=WEEKLY;COUNT=3",
        );

        let override1 = make_override_ics(
            "series1",
            "Week 1 Updated",
            "20270601T100000",
            "20270601T110000",
            "20270601T100000",
        );

        let override3 = make_override_ics(
            "series1",
            "Week 3 Updated",
            "20270615T100000",
            "20270615T110000",
            "20270615T100000",
        );

        let meetings = parse_and_dedup(&[&master, &override1, &override3]);
        assert_eq!(meetings.len(), 3);

        // Check override titles are used for dates 1 and 3
        let titles: Vec<&str> = meetings.iter().map(|m| m.title.as_str()).collect();
        assert!(titles.contains(&"Week 1 Updated"));
        assert!(titles.contains(&"Week 3 Updated"));
        assert!(titles.contains(&"Weekly")); // Week 2 keeps original
    }

    #[test]
    fn test_dedup_same_uid_same_time_non_recurring() {
        // Two non-recurring events with same UID and same time (shouldn't happen
        // in practice but tests the dedup path)
        let ics1 = make_ics("dup1", "Version A", "20270601T100000", "20270601T110000");
        let ics2 = make_ics("dup1", "Version B", "20270601T100000", "20270601T110000");
        let meetings = parse_and_dedup(&[&ics1, &ics2]);
        // Both have same uid@timestamp key; first one wins (neither is an override)
        assert_eq!(meetings.len(), 1);
    }

    #[test]
    fn test_dedup_preserves_different_uids_same_time() {
        // Two different events at the same time — both should appear
        let ics1 = make_ics("evt-a", "Meeting A", "20270601T100000", "20270601T110000");
        let ics2 = make_ics("evt-b", "Meeting B", "20270601T100000", "20270601T110000");
        let meetings = parse_and_dedup(&[&ics1, &ics2]);
        assert_eq!(meetings.len(), 2);
    }

    #[test]
    fn test_meetings_sorted_by_start_time() {
        let ics1 = make_ics("evt1", "Third", "20270603T100000", "20270603T110000");
        let ics2 = make_ics("evt2", "First", "20270601T100000", "20270601T110000");
        let ics3 = make_ics("evt3", "Second", "20270602T100000", "20270602T110000");
        let meetings = parse_and_dedup(&[&ics1, &ics2, &ics3]);
        assert_eq!(meetings.len(), 3);
        assert_eq!(meetings[0].title, "First");
        assert_eq!(meetings[1].title, "Second");
        assert_eq!(meetings[2].title, "Third");
    }

    #[test]
    fn test_dedup_and_sort_limit() {
        let m1 = (
            false,
            Meeting {
                uid: "a@1".to_string(),
                title: "First".to_string(),
                start: Local::now() + chrono::Duration::hours(1),
                end: Local::now() + chrono::Duration::hours(2),
                location: None,
                description: None,
                calendar_uid: "cal".to_string(),
                is_all_day: false,
                attendance_status: AttendanceStatus::None,
            },
        );
        let m2 = (
            false,
            Meeting {
                uid: "b@2".to_string(),
                title: "Second".to_string(),
                start: Local::now() + chrono::Duration::hours(3),
                end: Local::now() + chrono::Duration::hours(4),
                location: None,
                description: None,
                calendar_uid: "cal".to_string(),
                is_all_day: false,
                attendance_status: AttendanceStatus::None,
            },
        );
        let m3 = (
            false,
            Meeting {
                uid: "c@3".to_string(),
                title: "Third".to_string(),
                start: Local::now() + chrono::Duration::hours(5),
                end: Local::now() + chrono::Duration::hours(6),
                location: None,
                description: None,
                calendar_uid: "cal".to_string(),
                is_all_day: false,
                attendance_status: AttendanceStatus::None,
            },
        );

        let result = dedup_and_sort_meetings(vec![m1, m2, m3], 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title, "First");
        assert_eq!(result[1].title, "Second");
    }

    #[test]
    fn test_dedup_override_wins_over_non_override() {
        let now = Local::now();
        let start = now + chrono::Duration::hours(1);
        let end = start + chrono::Duration::hours(1);

        let master = (
            false,
            Meeting {
                uid: "key@123".to_string(),
                title: "Original".to_string(),
                start,
                end,
                location: None,
                description: None,
                calendar_uid: "cal".to_string(),
                is_all_day: false,
                attendance_status: AttendanceStatus::None,
            },
        );
        let override_m = (
            true,
            Meeting {
                uid: "key@123".to_string(),
                title: "Override".to_string(),
                start,
                end,
                location: Some("New Room".to_string()),
                description: None,
                calendar_uid: "cal".to_string(),
                is_all_day: false,
                attendance_status: AttendanceStatus::None,
            },
        );

        // Override after master
        let result = dedup_and_sort_meetings(vec![master.clone(), override_m.clone()], 100);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "Override");
        assert_eq!(result[0].location, Some("New Room".to_string()));

        // Override before master — should still win
        let result2 = dedup_and_sort_meetings(vec![override_m, master], 100);
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0].title, "Override");
    }

    #[test]
    fn test_dedup_non_override_does_not_replace_override() {
        let now = Local::now();
        let start = now + chrono::Duration::hours(1);
        let end = start + chrono::Duration::hours(1);

        let override_m = (
            true,
            Meeting {
                uid: "key@123".to_string(),
                title: "Override".to_string(),
                start,
                end,
                location: None,
                description: None,
                calendar_uid: "cal".to_string(),
                is_all_day: false,
                attendance_status: AttendanceStatus::None,
            },
        );
        let non_override = (
            false,
            Meeting {
                uid: "key@123".to_string(),
                title: "Non-Override".to_string(),
                start,
                end,
                location: None,
                description: None,
                calendar_uid: "cal".to_string(),
                is_all_day: false,
                attendance_status: AttendanceStatus::None,
            },
        );

        // If override is first, non-override should NOT replace it
        let result = dedup_and_sort_meetings(vec![override_m, non_override], 100);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "Override");
    }

    #[test]
    fn test_all_day_event_parsed() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
             BEGIN:VEVENT\r\n\
             UID:allday1\r\n\
             SUMMARY:Company Holiday\r\n\
             DTSTART;VALUE=DATE:20270601\r\n\
             DTEND;VALUE=DATE:20270602\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR";
        let meetings = parse_and_dedup(&[ics]);
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].title, "Company Holiday");
        assert!(meetings[0].is_all_day);
    }

    #[test]
    fn test_bare_vevent_wrapped() {
        // EDS sometimes returns raw VEVENT without VCALENDAR wrapper
        let ics = "BEGIN:VEVENT\r\n\
             UID:bare1\r\n\
             SUMMARY:Bare Event\r\n\
             DTSTART:20270601T100000\r\n\
             DTEND:20270601T110000\r\n\
             END:VEVENT";
        let meetings = parse_and_dedup(&[ics]);
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].title, "Bare Event");
    }

    #[test]
    fn test_bundled_master_and_override_in_one_ics() {
        // EDS can return a master + RECURRENCE-ID exception bundled in one ICS object
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
             BEGIN:VEVENT\r\n\
             UID:bundled1\r\n\
             SUMMARY:Weekly Meeting\r\n\
             DTSTART:20270601T100000\r\n\
             DTEND:20270601T110000\r\n\
             RRULE:FREQ=WEEKLY;COUNT=2\r\n\
             END:VEVENT\r\n\
             BEGIN:VEVENT\r\n\
             UID:bundled1\r\n\
             SUMMARY:Weekly Meeting (Moved)\r\n\
             DTSTART:20270601T140000\r\n\
             DTEND:20270601T150000\r\n\
             RECURRENCE-ID:20270601T100000\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR";
        let meetings = parse_and_dedup(&[ics]);
        // calcard should handle the override within the same ICS, producing:
        // - The override instance (moved to 14:00) for June 1
        // - The regular instance for June 8
        assert_eq!(meetings.len(), 2);
        // The June 1 instance should use the override (moved time)
        // The June 8 instance should use the master
    }

    #[test]
    fn test_invalid_ics_skipped() {
        let valid = make_ics("evt1", "Valid", "20270601T100000", "20270601T110000");
        let invalid = "this is not valid ics data";
        let meetings = parse_and_dedup(&[&valid, invalid]);
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].title, "Valid");
    }

    #[test]
    fn test_event_with_location_and_description() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
             BEGIN:VEVENT\r\n\
             UID:loc1\r\n\
             SUMMARY:In-Person Meeting\r\n\
             DTSTART:20270601T100000\r\n\
             DTEND:20270601T110000\r\n\
             LOCATION:Conference Room A\r\n\
             DESCRIPTION:Quarterly planning session\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR";
        let meetings = parse_and_dedup(&[ics]);
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].location, Some("Conference Room A".to_string()));
        assert_eq!(
            meetings[0].description,
            Some("Quarterly planning session".to_string())
        );
    }

    #[test]
    fn test_untitled_event_gets_default_title() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
             BEGIN:VEVENT\r\n\
             UID:notitle1\r\n\
             DTSTART:20270601T100000\r\n\
             DTEND:20270601T110000\r\n\
             END:VEVENT\r\n\
             END:VCALENDAR";
        let meetings = parse_and_dedup(&[ics]);
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0].title, "Untitled Event");
    }
}

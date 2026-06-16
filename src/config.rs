// SPDX-License-Identifier: GPL-3.0-only

use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};
use serde::{Deserialize, Serialize};

/// How to display the meeting time in the panel
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum DisplayFormat {
    /// Show day and time (e.g., "Fri 9:30: All Hands")
    #[default]
    DayAndTime,
    /// Show relative time (e.g., "In 2d 3h: All Hands")
    Relative,
    /// Legacy: treated as `DayAndTime`
    TitleOnly,
    /// Legacy: treated as `DayAndTime`
    TimeOnly,
}

/// When to show the Join button
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum JoinButtonVisibility {
    /// Never show join button
    Hide,
    /// Always show join button (when URL available)
    Show,
    /// Show join button only when meeting is same day
    ShowIfSameDay,
    /// Show join button only when meeting is within 30 minutes
    ShowIf30m,
    /// Show join button only when meeting is within 15 minutes
    #[default]
    ShowIf15m,
    /// Show join button only when meeting is within 5 minutes
    ShowIf5m,
}

/// Which events to show based on attendance status
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum EventStatusFilter {
    /// Show all events regardless of attendance status
    #[default]
    All,
    /// Show only events the user accepted
    Accepted,
    /// Show events the user accepted or tentatively accepted
    AcceptedOrTentative,
}

/// Whether to show meetings that have already started
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum InProgressMeeting {
    /// Only show future meetings
    Off,
    /// Show meetings that started within 5 minutes (default)
    #[default]
    Within5m,
    /// Show meetings that started within 10 minutes
    Within10m,
    /// Show meetings that started within 15 minutes
    Within15m,
    /// Show meetings that started within 30 minutes
    Within30m,
}

/// Filter events by how far in the future they are
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum TimeUntilFilter {
    /// Show all events (default)
    #[default]
    All,
    /// Show events within 3 hours
    Within3Hours,
    /// Show events within 6 hours
    Within6Hours,
    /// Show events on the same day
    SameDay,
    /// Show events within 1 day (24 hours)
    Within1Day,
    /// Show events within 2 days (48 hours)
    Within2Days,
}

/// Action to take when opening the calendar app
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum CalendarAppAction {
    /// Open the system default calendar app (default)
    #[default]
    SystemDefault,
    /// Run a custom command
    RunCommand,
    /// Open a URL
    OpenUrl,
}

#[derive(Debug, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 1]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    /// Whether to automatically refresh calendars from remote servers.
    pub auto_refresh_enabled: bool,
    /// Interval in minutes for automatic refresh (5, 10, 15, 30).
    pub auto_refresh_interval_minutes: u8,
    /// Calendar UIDs that are enabled for display.
    /// Empty list means all calendars are enabled.
    pub enabled_calendar_uids: Vec<String>,
    /// How to display the meeting time in the panel.
    pub display_format: DisplayFormat,
    /// Number of upcoming events to show in the popup (0-10).
    pub upcoming_events_count: u8,
    /// When to show the Join button in the popup.
    pub popup_join_button: JoinButtonVisibility,
    /// When to show the Join button in the panel.
    pub panel_join_button: JoinButtonVisibility,
    /// Whether to show the physical location in the popup.
    pub popup_show_location: bool,
    /// Whether to show the physical location in the panel.
    pub panel_show_location: bool,
    /// Show the app icon to the left of the panel text.
    pub panel_show_icon: bool,
    /// Show calendar color indicator in panel.
    pub panel_calendar_indicator: bool,
    /// Show calendar color indicator in popup.
    pub popup_calendar_indicator: bool,
    /// Regex patterns to detect meeting URLs in location/description.
    pub meeting_url_patterns: Vec<String>,
    /// Whether to show all-day events.
    pub show_all_day_events: bool,
    /// Filter events by attendance status.
    pub event_status_filter: EventStatusFilter,
    /// Additional email addresses to identify the user in ATTENDEE fields.
    /// Used in addition to the `CalEmailAddress` from each calendar.
    pub additional_emails: Vec<String>,
    /// Whether to show meetings that have already started.
    pub show_in_progress: InProgressMeeting,
    /// Filter events by how far in the future they are.
    pub time_until_filter: TimeUntilFilter,
    /// Whether to show the "Open Calendar" menu item.
    pub show_calendar_button: bool,
    /// Action to take when opening the calendar.
    pub calendar_app_action: CalendarAppAction,
    /// Custom command to run when opening the calendar.
    pub calendar_app_command: String,
    /// URL to open when opening the calendar.
    pub calendar_app_url: String,
    /// Whether to hide the applet from the panel when there are no meetings.
    pub hide_when_no_meetings: bool,
    /// Whether to show the "Scheduling Helper" entry in the menu (off by default).
    pub scheduling_helper_enabled: bool,
    /// Whether the Scheduling Helper opens as a resizable window instead of a
    /// fixed-size popup (which floats as a dialog under tiling WMs).
    pub scheduling_helper_resizable: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_refresh_enabled: false,
            auto_refresh_interval_minutes: 10,
            enabled_calendar_uids: Vec::new(),
            display_format: DisplayFormat::default(),
            upcoming_events_count: 3,
            popup_join_button: JoinButtonVisibility::ShowIfSameDay,
            panel_join_button: JoinButtonVisibility::ShowIf15m,
            popup_show_location: false,
            panel_show_location: false,
            panel_show_icon: false,
            panel_calendar_indicator: false,
            popup_calendar_indicator: true,
            meeting_url_patterns: vec![
                // Google Meet
                r"https://meet\.google\.com/[a-z-]+".to_string(),
                // Zoom
                r"https://[a-z0-9]+\.zoom\.us/j/[0-9]+".to_string(),
                // Microsoft Teams
                r"https://teams\.microsoft\.com/l/meetup-join/[^\s]+".to_string(),
                r"https://teams\.live\.com/meet/[^\s]+".to_string(),
                // Webex
                r"https://[a-z0-9]+\.webex\.com/[^\s]+/j\.php\?MTID=[^\s]+".to_string(),
                r"https://[a-z0-9]+\.webex\.com/meet/[^\s]+".to_string(),
            ],
            show_all_day_events: true,
            event_status_filter: EventStatusFilter::default(),
            additional_emails: Vec::new(),
            show_in_progress: InProgressMeeting::default(),
            time_until_filter: TimeUntilFilter::default(),
            show_calendar_button: true,
            calendar_app_action: CalendarAppAction::default(),
            calendar_app_command: "gnome-calendar".to_string(),
            calendar_app_url: "https://calendar.google.com".to_string(),
            hide_when_no_meetings: false,
            scheduling_helper_enabled: false,
            scheduling_helper_resizable: false,
        }
    }
}

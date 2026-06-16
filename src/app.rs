// SPDX-License-Identifier: GPL-3.0-only

use crate::calendar::{
    CalendarDiscovery, CalendarInfo, Meeting, extract_meeting_url, get_physical_location,
};
use crate::config::{Config, DisplayFormat, InProgressMeeting, JoinButtonVisibility};
use crate::fl;
use crate::formatting::{
    format_backend_name, format_last_updated, format_panel_time, format_relative_time, format_time,
    parse_hex_color,
};
use crate::widgets::{
    calendar_color_dot, display_format_options, email_input_id, secondary_text_style,
    settings_nav_row, settings_nav_row_with_icon, settings_page_header, spacing,
};
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::{Length, Limits, Subscription, clipboard, window::Id};
use cosmic::prelude::*;
use cosmic::widget;
use futures_util::SinkExt;

/// The application model stores app-specific state used to describe its interface and
/// drive its logic.
#[derive(Default)]
pub struct AppModel {
    /// Application state which is managed by the COSMIC runtime.
    core: cosmic::Core,
    /// The popup id.
    popup: Option<Id>,
    /// Configuration data that persists between application runs.
    config: Config,
    /// Upcoming meetings to display.
    upcoming_meetings: Vec<Meeting>,
    /// Available calendars from Evolution Data Server.
    available_calendars: Vec<CalendarInfo>,
    /// Config context for saving changes.
    config_context: Option<cosmic_config::Config>,
    /// Current page in popup navigation
    current_page: PopupPage,
    /// Whether a calendar refresh is in progress.
    is_refreshing: bool,
    /// Whether the initial meeting fetch has completed.
    has_loaded_meetings: bool,
    /// Whether Evolution Data Server is available on the session bus.
    /// `false` means EDS appears to be missing (not installed or not running),
    /// which calls for different guidance than "no calendars configured".
    /// Defaults to `true` so we don't flash a "not installed" message before
    /// the first calendar discovery completes.
    eds_available: bool,
    /// Online accounts that need re-authentication.
    accounts_needing_attention: Vec<crate::calendar::AccountNeedingAttention>,
    /// Action selected in the keyboard-shortcut page's dropdown (UI-only).
    shortcut_action: ShortcutAction,
}

/// Navigation state for popup pages
#[derive(Debug, Default, Clone, PartialEq)]
pub enum PopupPage {
    #[default]
    Main,
    Settings,
    Calendars,
    RefreshSettings,
    CalendarAppSettings,
    EventsToShowSettings,
    EmailSettings,
    PanelDisplaySettings,
    PopupDisplaySettings,
    PanelJoinButtonSettings,
    PopupJoinButtonSettings,
    SchedulingHelperSettings,
    KeyboardShortcut,
    About,
}

/// Actions that can be bound to a global keyboard shortcut, each mapping to a
/// CLI flag the user binds in COSMIC's keyboard settings.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    /// Open the applet's panel popup (signals the running applet over D-Bus).
    #[default]
    OpenMenu,
    /// Join/open the next meeting's URL.
    NextMeeting,
    /// Open the Scheduling Helper window.
    SchedulingHelper,
}

impl ShortcutAction {
    /// The CLI flag this action maps to.
    fn flag(self) -> &'static str {
        match self {
            ShortcutAction::OpenMenu => "--open-menu",
            ShortcutAction::NextMeeting => "--join-next",
            ShortcutAction::SchedulingHelper => "--scheduling-helper",
        }
    }

    /// The localized dropdown label.
    fn label(self) -> String {
        match self {
            ShortcutAction::OpenMenu => fl!("shortcut-action-open-menu"),
            ShortcutAction::NextMeeting => fl!("shortcut-action-next-meeting"),
            ShortcutAction::SchedulingHelper => fl!("shortcut-action-scheduling-helper"),
        }
    }
}

impl AppModel {
    /// Actions offered in the keyboard-shortcut dropdown. The Scheduling Helper
    /// only appears when its feature is enabled.
    fn available_shortcut_actions(&self) -> Vec<ShortcutAction> {
        let mut actions = vec![ShortcutAction::OpenMenu, ShortcutAction::NextMeeting];
        if self.config.scheduling_helper_enabled {
            actions.push(ShortcutAction::SchedulingHelper);
        }
        actions
    }
}

impl AppModel {
    /// Get meetings filtered by current settings (all-day events, attendance status, in-progress, time until)
    fn filtered_meetings(&self) -> Vec<&Meeting> {
        use crate::calendar::AttendanceStatus;
        use crate::config::{EventStatusFilter, TimeUntilFilter};
        use chrono::Local;

        let now = Local::now();

        self.upcoming_meetings
            .iter()
            .filter(|m| {
                // Filter out all-day events if disabled
                if !self.config.show_all_day_events && m.is_all_day {
                    return false;
                }

                // Filter in-progress meetings based on config
                if m.start <= now {
                    // This is an in-progress meeting (already started)
                    let minutes_since_start = now.signed_duration_since(m.start).num_minutes();
                    let include = match self.config.show_in_progress {
                        InProgressMeeting::Off => false,
                        InProgressMeeting::Within5m => minutes_since_start <= 5,
                        InProgressMeeting::Within10m => minutes_since_start <= 10,
                        InProgressMeeting::Within15m => minutes_since_start <= 15,
                        InProgressMeeting::Within30m => minutes_since_start <= 30,
                    };
                    if !include {
                        return false;
                    }
                }

                // Filter by time until (how far in the future)
                if m.start > now {
                    let include = match self.config.time_until_filter {
                        TimeUntilFilter::All => true,
                        TimeUntilFilter::Within3Hours => {
                            m.start.signed_duration_since(now).num_hours() < 3
                        }
                        TimeUntilFilter::Within6Hours => {
                            m.start.signed_duration_since(now).num_hours() < 6
                        }
                        TimeUntilFilter::SameDay => m.start.date_naive() == now.date_naive(),
                        TimeUntilFilter::Within1Day => {
                            m.start.signed_duration_since(now).num_hours() < 24
                        }
                        TimeUntilFilter::Within2Days => {
                            m.start.signed_duration_since(now).num_hours() < 48
                        }
                    };
                    if !include {
                        return false;
                    }
                }

                // Filter by attendance status
                match self.config.event_status_filter {
                    EventStatusFilter::All => true,
                    EventStatusFilter::Accepted => {
                        matches!(
                            m.attendance_status,
                            AttendanceStatus::Accepted | AttendanceStatus::None
                        )
                    }
                    EventStatusFilter::AcceptedOrTentative => {
                        matches!(
                            m.attendance_status,
                            AttendanceStatus::Accepted
                                | AttendanceStatus::Tentative
                                | AttendanceStatus::None
                        )
                    }
                }
            })
            .collect()
    }

    /// Save config to disk, logging any errors (e.g. Flatpak sandbox write failures).
    fn save_config(&self) {
        if let Some(ref ctx) = self.config_context
            && let Err(e) = self.config.write_entry(ctx)
        {
            eprintln!("warning: failed to save config: {e}");
        }
    }

    /// Get UIDs of enabled calendars that are valid meeting sources.
    /// Filters out non-meeting calendars (contacts, weather, birthdays).
    fn enabled_meeting_source_uids(&self) -> Vec<String> {
        // If we don't have the calendars list yet, fall back to config
        // (non-meeting calendars won't have VEVENT data anyway)
        if self.available_calendars.is_empty() {
            return self.config.enabled_calendar_uids.clone();
        }

        // If enabled_calendar_uids is empty, all meeting-source calendars are enabled
        if self.config.enabled_calendar_uids.is_empty() {
            self.available_calendars
                .iter()
                .filter(|c| c.is_meeting_source())
                .map(|c| c.uid.clone())
                .collect()
        } else {
            // Filter the enabled list to only include meeting sources
            self.config
                .enabled_calendar_uids
                .iter()
                .filter(|uid| {
                    self.available_calendars
                        .iter()
                        .find(|c| &c.uid == *uid)
                        .is_some_and(super::calendar::CalendarInfo::is_meeting_source)
                })
                .cloned()
                .collect()
        }
    }

    /// Main popup page showing meeting info and settings nav
    #[allow(clippy::too_many_lines)]
    fn view_main_page(&self) -> Element<'_, Message> {
        let space = spacing();

        let mut content = widget::column::with_capacity(8)
            .padding([space.space_xxs, space.space_none])
            .width(Length::Fill);

        let filtered = self.filtered_meetings();
        if let Some(meeting) = filtered.first() {
            use chrono::Local;
            let now = Local::now();
            let minutes_until = meeting.start.signed_duration_since(now).num_minutes();
            let is_same_day = meeting.start.date_naive() == now.date_naive();
            let time_str = format_time(&meeting.start, true);

            // Check for meeting URL based on popup join button visibility settings
            let show_join = match self.config.popup_join_button {
                JoinButtonVisibility::Hide => false,
                JoinButtonVisibility::Show => true,
                JoinButtonVisibility::ShowIfSameDay => is_same_day,
                JoinButtonVisibility::ShowIf30m => minutes_until <= 30,
                JoinButtonVisibility::ShowIf15m => minutes_until <= 15,
                JoinButtonVisibility::ShowIf5m => minutes_until <= 5,
            };
            let meeting_url = if show_join {
                extract_meeting_url(meeting, &self.config.meeting_url_patterns)
            } else {
                None
            };

            // Check for physical location based on popup location visibility settings
            let physical_location = if self.config.popup_show_location {
                get_physical_location(meeting, &self.config.meeting_url_patterns)
            } else {
                None
            };

            // Next meeting content block with optional Join button
            let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);

            // Build meeting info column with title, time, and optional location
            let mut meeting_column = widget::column::with_capacity(3)
                .push(widget::text::title4(&meeting.title))
                .push(widget::text::body(time_str).class(secondary_text))
                .spacing(space.space_xxxs)
                .width(Length::Fill);

            if let Some(location) = physical_location {
                meeting_column =
                    meeting_column.push(widget::text::body(location).class(secondary_text));
            }

            // Wrap column in row with optional calendar indicator dot (centered vertically)
            let meeting_content: cosmic::Element<'_, Message> =
                if self.config.popup_calendar_indicator {
                    let mut row = widget::row::with_capacity(2)
                        .spacing(space.space_s)
                        .align_y(cosmic::iced::Alignment::Center)
                        .width(Length::Fill);
                    if let Some(dot) = calendar_color_dot::<Message>(
                        &meeting.calendar_uid,
                        &self.available_calendars,
                        10.0,
                        Some(widget::tooltip::Position::Top),
                    ) {
                        row = row.push(dot);
                    }
                    row.push(meeting_column).into()
                } else {
                    meeting_column.into()
                };

            let meeting_info = widget::container(meeting_content)
                .padding([space.space_xxs, space.space_xs])
                .width(Length::Fill);

            if let Some(url) = meeting_url {
                // Row with meeting info and Join button (with horizontal padding)
                content = content.push(
                    widget::row::with_capacity(2)
                        .push(meeting_info)
                        .push(
                            widget::button::suggested(fl!("join")).on_press(Message::OpenUrl(url)),
                        )
                        .align_y(cosmic::iced::Alignment::Center)
                        .spacing(space.space_xs)
                        .width(Length::Fill)
                        .apply(widget::container)
                        .padding([0, space.space_s]),
                );
            } else {
                // Wrap in container with horizontal padding
                content = content.push(
                    meeting_info
                        .apply(widget::container)
                        .padding([0, space.space_s]),
                );
            }

            // Show notice if in vertical panel (after next meeting, before upcoming)
            if !self.core.applet.is_horizontal() {
                content = content.push(
                    cosmic::applet::padded_control(widget::divider::horizontal::default())
                        .padding([space.space_xxs, space.space_s]),
                );
                content = content.push(cosmic::applet::padded_control(
                    widget::row::with_capacity(2)
                        .spacing(space.space_xs)
                        .align_y(cosmic::iced::Alignment::Start)
                        .push(
                            widget::container(
                                widget::icon::from_name("dialog-warning-symbolic")
                                    .size(space.space_s),
                            )
                            .class(cosmic::theme::Container::custom(
                                |theme| cosmic::iced::widget::container::Style {
                                    icon_color: Some(theme.cosmic().palette.bright_orange.into()),
                                    ..Default::default()
                                },
                            )),
                        )
                        .push(widget::text::body(fl!("vertical-panel-notice"))),
                ));
            }

            // Upcoming events section
            let upcoming_count = self.config.upcoming_events_count as usize;
            if upcoming_count > 0 && filtered.len() > 1 {
                // Divider before "Upcoming" section
                content = content.push(
                    cosmic::applet::padded_control(widget::divider::horizontal::default())
                        .padding([space.space_xxs, space.space_s]),
                );

                // "Upcoming" section heading
                content = content.push(cosmic::applet::padded_control(widget::text::heading(fl!(
                    "upcoming"
                ))));

                let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);
                for meeting in filtered.iter().skip(1).take(upcoming_count) {
                    let time_str = format_time(&meeting.start, false);

                    // Build row with optional calendar indicator
                    let mut row = widget::row::with_capacity(4)
                        .spacing(space.space_xs)
                        .align_y(cosmic::iced::Alignment::Center)
                        .width(Length::Fill);

                    if self.config.popup_calendar_indicator
                        && let Some(dot) = calendar_color_dot::<Message>(
                            &meeting.calendar_uid,
                            &self.available_calendars,
                            8.0,
                            None,
                        )
                    {
                        row = row.push(dot);
                    }

                    // Title takes available space, time shrinks to fit
                    row = row
                        .push(
                            widget::container(widget::text::body(&meeting.title))
                                .width(Length::Fill),
                        )
                        .push(widget::text::body(time_str).class(secondary_text));

                    content = content.push(cosmic::applet::padded_control(row));
                }
            }
        } else if !self.has_loaded_meetings {
            // Still loading initial data
            content = content.push(cosmic::applet::padded_control(widget::text::body(fl!(
                "loading-meetings"
            ))));
        } else if self.available_calendars.is_empty() {
            // No calendars configured - show prominent centered message.
            // Distinguish "EDS not installed" from "EDS present, no calendars".
            let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);
            let (title, description) = if self.eds_available {
                (fl!("no-calendars"), fl!("no-calendars-description"))
            } else {
                (
                    fl!("no-calendar-service"),
                    fl!("no-calendar-service-description"),
                )
            };

            // Keep the icon and heading tightly grouped, then leave a larger
            // gap before the centered body text.
            let heading = widget::column::with_capacity(2)
                .spacing(space.space_xxs)
                .align_x(cosmic::iced::Alignment::Center)
                .push(widget::icon::from_name("dialog-warning-symbolic").size(space.space_l))
                .push(widget::text::title4(title));

            let no_cal_content = widget::column::with_capacity(2)
                .spacing(space.space_m)
                .align_x(cosmic::iced::Alignment::Center)
                .width(Length::Fill)
                .push(heading)
                .push(
                    widget::text::body(description)
                        .class(secondary_text)
                        .width(Length::Fill)
                        .align_x(cosmic::iced::alignment::Horizontal::Center)
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word),
                );

            content = content.push(
                widget::container(no_cal_content)
                    .padding([space.space_l, space.space_l])
                    .width(Length::Fill)
                    .align_x(cosmic::iced::alignment::Horizontal::Center),
            );
        } else {
            // Calendars exist but no upcoming meetings
            content = content.push(cosmic::applet::padded_control(widget::text::body(fl!(
                "no-meetings"
            ))));
        }

        // Warning banner for accounts needing re-authentication
        if !self.accounts_needing_attention.is_empty() {
            content = content.push(
                cosmic::applet::padded_control(widget::divider::horizontal::default())
                    .padding([space.space_xxs, space.space_s]),
            );
            for account in &self.accounts_needing_attention {
                content = content.push(
                    cosmic::applet::menu_button(
                        widget::row::with_capacity(3)
                            .spacing(space.space_xs)
                            .align_y(cosmic::iced::Alignment::Center)
                            .push(
                                widget::container(
                                    widget::icon::from_name("dialog-warning-symbolic")
                                        .size(space.space_m),
                                )
                                .class(
                                    cosmic::theme::Container::custom(|theme| {
                                        cosmic::iced::widget::container::Style {
                                            icon_color: Some(
                                                theme.cosmic().palette.bright_orange.into(),
                                            ),
                                            ..Default::default()
                                        }
                                    }),
                                ),
                            )
                            .push(
                                widget::text::body(fl!(
                                    "account-needs-attention",
                                    identity = account.identity.clone()
                                ))
                                .wrapping(cosmic::iced::widget::text::Wrapping::Word),
                            )
                            .width(Length::Fill),
                    )
                    .on_press(Message::OpenOnlineAccounts),
                );
            }
        }

        // Divider before bottom actions
        content = content.push(
            cosmic::applet::padded_control(widget::divider::horizontal::default())
                .padding([space.space_xxs, space.space_s]),
        );

        // Bottom actions section (Open calendar + Settings)
        if self.config.show_calendar_button {
            content = content.push(
                cosmic::applet::menu_button(
                    widget::row::with_capacity(3)
                        .push(
                            widget::icon::from_name("office-calendar-symbolic").size(space.space_m),
                        )
                        .push(widget::text::body(fl!("open-calendar")))
                        .push(widget::space::horizontal())
                        .spacing(space.space_xs)
                        .align_y(cosmic::iced::Alignment::Center)
                        .width(Length::Fill),
                )
                .on_press(Message::OpenCalendar),
            );
        }

        // Optional Scheduling Helper entry (off by default).
        if self.config.scheduling_helper_enabled {
            content = content.push(
                cosmic::applet::menu_button(
                    widget::row::with_capacity(3)
                        .push(widget::icon::from_name("mail-send-symbolic").size(space.space_m))
                        .push(widget::text::body(fl!("scheduling-helper")))
                        .push(widget::space::horizontal())
                        .spacing(space.space_xs)
                        .align_y(cosmic::iced::Alignment::Center)
                        .width(Length::Fill),
                )
                .on_press(Message::OpenSchedulingHelper),
            );
        }

        content = content.push(
            cosmic::applet::menu_button(
                widget::row::with_capacity(3)
                    .push(
                        widget::icon::from_name("preferences-system-symbolic").size(space.space_m),
                    )
                    .push(widget::text::body(fl!("settings")))
                    .push(widget::space::horizontal())
                    .spacing(space.space_xs)
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            )
            .on_press(Message::Navigate(PopupPage::Settings)),
        );

        content.into()
    }

    /// Settings page with back button
    #[allow(clippy::too_many_lines)]
    fn view_settings_page(&self) -> Element<'_, Message> {
        use crate::config::EventStatusFilter;

        let space = spacing();
        let mut content = widget::column::with_capacity(8)
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(settings_page_header(
            fl!("back"),
            fl!("settings"),
            Message::Navigate(PopupPage::Main),
        ));

        // Compute summaries for calendar settings
        let calendars_summary = if self.available_calendars.is_empty() {
            fl!("calendars-none")
        } else {
            // Total includes all calendars (even force-disabled ones like birthdays)
            let total = self.available_calendars.len();
            // Enabled count is meeting sources that are enabled
            let meeting_sources: Vec<_> = self
                .available_calendars
                .iter()
                .filter(|c| c.is_meeting_source())
                .collect();
            let enabled = if self.config.enabled_calendar_uids.is_empty() {
                meeting_sources.len() // All meeting sources enabled when list is empty
            } else {
                meeting_sources
                    .iter()
                    .filter(|c| self.config.enabled_calendar_uids.contains(&c.uid))
                    .count()
            };
            fl!("calendars-enabled", enabled = enabled, total = total)
        };

        let filter_summary = {
            let all_day = !self.config.show_all_day_events;
            let status = match self.config.event_status_filter {
                EventStatusFilter::All => None,
                EventStatusFilter::Accepted => Some(fl!("filter-summary-accepted")),
                EventStatusFilter::AcceptedOrTentative => Some(fl!("filter-summary-tentative")),
            };
            match (all_day, status) {
                (false, None) => fl!("filter-summary-all"),
                (true, None) => fl!("filter-summary-no-all-day"),
                (false, Some(s)) => s,
                (true, Some(s)) => fl!(
                    "filter-summary-combo",
                    allday = fl!("filter-summary-no-all-day"),
                    status = s
                ),
            }
        };

        let refresh_summary = if self.config.auto_refresh_enabled {
            fl!(
                "refresh-summary-on",
                interval = self.config.auto_refresh_interval_minutes
            )
        } else {
            fl!("refresh-summary-off")
        };

        // Calendars & Filtering section
        let calendars_section = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(settings_nav_row_with_icon(
                "x-office-calendar-symbolic",
                fl!("calendars-section"),
                calendars_summary,
                Message::Navigate(PopupPage::Calendars),
            ))
            .add(settings_nav_row_with_icon(
                "view-filter-symbolic",
                fl!("filter-events-section"),
                filter_summary,
                Message::Navigate(PopupPage::EventsToShowSettings),
            ))
            .add(settings_nav_row_with_icon(
                "emblem-synchronizing-symbolic",
                fl!("refresh-section"),
                refresh_summary,
                Message::Navigate(PopupPage::RefreshSettings),
            ));

        content = content.push(calendars_section);
        content = content.push(widget::space::vertical().height(space.space_xs));

        // Display sections
        let display_section = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(settings_nav_row_with_icon(
                "preferences-panel-symbolic",
                fl!("panel-display"),
                String::new(),
                Message::Navigate(PopupPage::PanelDisplaySettings),
            ))
            .add(settings_nav_row_with_icon(
                "view-list-symbolic",
                fl!("dropdown-display"),
                String::new(),
                Message::Navigate(PopupPage::PopupDisplaySettings),
            ));

        content = content.push(display_section);
        content = content.push(widget::space::vertical().height(space.space_xs));

        // ===== FEATURES SECTION =====
        let scheduling_summary = if self.config.scheduling_helper_enabled {
            fl!("on")
        } else {
            fl!("off")
        };
        let features_section = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(settings_nav_row_with_icon(
                "mail-send-symbolic",
                fl!("scheduling-section"),
                scheduling_summary,
                Message::Navigate(PopupPage::SchedulingHelperSettings),
            ));

        content = content.push(features_section);
        content = content.push(widget::space::vertical().height(space.space_xs));

        // ===== KEYBOARD SHORTCUT & CALENDAR APP SECTION =====
        let shortcut_section = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(settings_nav_row_with_icon(
                "input-keyboard-symbolic",
                fl!("keyboard-shortcut"),
                String::new(),
                Message::Navigate(PopupPage::KeyboardShortcut),
            ))
            .add(settings_nav_row_with_icon(
                "office-calendar-symbolic",
                fl!("calendar-app-section"),
                String::new(),
                Message::Navigate(PopupPage::CalendarAppSettings),
            ));

        content = content.push(shortcut_section);
        content = content.push(widget::space::vertical().height(space.space_xs));

        // ===== ABOUT SECTION =====
        let about_section = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(settings_nav_row_with_icon(
                "help-about-symbolic",
                fl!("about"),
                String::new(),
                Message::Navigate(PopupPage::About),
            ));

        content = content.push(about_section);

        content.into()
    }

    /// Calendars selection page
    #[allow(clippy::too_many_lines)]
    fn view_calendars_page(&self) -> Element<'_, Message> {
        let space = spacing();
        let mut content = widget::column::with_capacity(2 + self.available_calendars.len())
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(
            widget::column::with_capacity(2)
                .push(
                    widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                        .extra_small()
                        .padding(space.space_none)
                        .label(fl!("settings"))
                        .spacing(space.space_xxxs)
                        .class(cosmic::theme::Button::Link)
                        .on_press(Message::Navigate(PopupPage::Settings)),
                )
                .push(widget::text::title4(fl!("calendars-section")))
                .spacing(space.space_xxs),
        );

        // Warning for accounts needing re-authentication
        for account in &self.accounts_needing_attention {
            content = content.push(
                cosmic::applet::menu_button(
                    widget::row::with_capacity(2)
                        .spacing(space.space_xs)
                        .align_y(cosmic::iced::Alignment::Center)
                        .push(
                            widget::container(
                                widget::icon::from_name("dialog-warning-symbolic")
                                    .size(space.space_m),
                            )
                            .class(cosmic::theme::Container::custom(
                                |theme| cosmic::iced::widget::container::Style {
                                    icon_color: Some(theme.cosmic().palette.bright_orange.into()),
                                    ..Default::default()
                                },
                            )),
                        )
                        .push(
                            widget::text::body(fl!(
                                "account-needs-attention",
                                identity = account.identity.clone()
                            ))
                            .wrapping(cosmic::iced::widget::text::Wrapping::Word),
                        )
                        .width(Length::Fill),
                )
                .on_press(Message::OpenOnlineAccounts),
            );
        }

        // Calendar toggles in a single list_column
        let mut calendars_list =
            widget::list_column().list_item_padding([space.space_xxs, space.space_xs]);

        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);

        for calendar in &self.available_calendars {
            let is_meeting_source = calendar.is_meeting_source();
            let is_enabled = is_meeting_source
                && (self.config.enabled_calendar_uids.is_empty()
                    || self.config.enabled_calendar_uids.contains(&calendar.uid));

            let uid = calendar.uid.clone();

            // Build row with optional color indicator
            let mut row = widget::row::with_capacity(4)
                .spacing(space.space_xs)
                .align_y(cosmic::iced::Alignment::Center);

            // Add color circle if color is available
            if let Some(color) = &calendar.color
                && let Some(parsed_color) = parse_hex_color(color)
            {
                row = row.push(
                    widget::container(widget::Space::new())
                        .width(Length::Fixed(12.0))
                        .height(Length::Fixed(12.0))
                        .class(cosmic::theme::Container::custom(move |_theme| {
                            cosmic::iced::widget::container::Style {
                                background: Some(cosmic::iced::Background::Color(parsed_color)),
                                border: cosmic::iced::Border {
                                    radius: 6.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            }
                        })),
                );
            }

            // Calendar name and metadata in a column
            let mut name_col = widget::column::with_capacity(2).spacing(space.space_xxxs);

            name_col = name_col.push(widget::text::body(&calendar.display_name));

            // Build secondary line with backend type and/or last updated time
            let backend_str = calendar.backend.as_ref().map(|b| format_backend_name(b));
            let updated_str = calendar
                .last_synced
                .as_ref()
                .map(|s| format_last_updated(s));

            let secondary_line = match (backend_str, updated_str) {
                (Some(backend), Some(updated)) => Some(format!("{backend} · {updated}")),
                (Some(backend), None) => Some(backend.to_string()),
                (None, Some(updated)) => Some(updated),
                (None, None) => None,
            };

            if let Some(line) = secondary_line {
                name_col = name_col.push(widget::text::caption(line).class(secondary_text));
            }

            // Create toggler - disabled for non-meeting sources (contacts, weather, birthdays)
            let toggler = if is_meeting_source {
                widget::toggler(is_enabled).on_toggle(move |_| Message::ToggleCalendar(uid.clone()))
            } else {
                // Non-meeting sources: show disabled toggle (no on_toggle = non-interactive)
                widget::toggler(false)
            };

            row = row
                .push(name_col)
                .push(widget::space::horizontal())
                .push(toggler);

            calendars_list = calendars_list.add(row);
        }

        if self.available_calendars.is_empty() {
            // No calendars - show explanation tailored to whether EDS is present
            let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);
            let description = if self.eds_available {
                fl!("no-calendars-description")
            } else {
                fl!("no-calendar-service-description")
            };
            content = content.push(widget::text::body(description).class(secondary_text));
        } else {
            content = content.push(calendars_list);

            // Show setup tip if no remote calendars and <=3 calendars total
            // Use DEBUG_NO_REMOTE_CALENDARS=1 to force showing this tip
            let has_remote = self.available_calendars.iter().any(|c| {
                c.backend.as_ref().is_some_and(|b| {
                    !matches!(
                        b.to_lowercase().as_str(),
                        "local" | "weather" | "contacts" | "birthdays"
                    )
                })
            });
            let force_no_remote = std::env::var("DEBUG_NO_REMOTE_CALENDARS").is_ok();
            if force_no_remote || (!has_remote && self.available_calendars.len() <= 3) {
                content = content.push(
                    widget::row::with_capacity(2)
                        .spacing(space.space_xs)
                        .align_y(cosmic::iced::Alignment::Start)
                        .push(
                            widget::container(
                                widget::icon::from_name("dialog-information-symbolic")
                                    .size(space.space_s),
                            )
                            .class(cosmic::theme::Container::custom(
                                |theme| cosmic::iced::widget::container::Style {
                                    icon_color: Some(theme.cosmic().palette.neutral_6.into()),
                                    ..Default::default()
                                },
                            )),
                        )
                        .push(
                            widget::text::caption(fl!("calendars-setup-tip")).class(secondary_text),
                        ),
                );
            }
        }

        content.into()
    }

    /// Panel join button settings page
    fn view_panel_join_button_settings_page(&self) -> Element<'_, Message> {
        let space = spacing();
        let mut content = widget::column::with_capacity(8 + self.config.meeting_url_patterns.len())
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(settings_page_header(
            fl!("panel-display"),
            fl!("join-button-section"),
            Message::Navigate(PopupPage::PanelDisplaySettings),
        ));

        // Join button visibility dropdown options
        let join_options = vec![
            fl!("join-hide"),
            fl!("join-show"),
            fl!("join-show-same-day"),
            fl!("join-show-30m"),
            fl!("join-show-15m"),
            fl!("join-show-5m"),
        ];

        let join_idx = match self.config.panel_join_button {
            JoinButtonVisibility::Hide => Some(0),
            JoinButtonVisibility::Show => Some(1),
            JoinButtonVisibility::ShowIfSameDay => Some(2),
            JoinButtonVisibility::ShowIf30m => Some(3),
            JoinButtonVisibility::ShowIf15m => Some(4),
            JoinButtonVisibility::ShowIf5m => Some(5),
        };

        let settings_list = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("join-button-visibility")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(
                        join_options,
                        join_idx,
                        Message::SetPanelJoinButton,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );

        content = content.push(settings_list);

        // URL Patterns section
        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);
        content = content.push(widget::space::vertical().height(space.space_s));
        content = content.push(widget::text::body(fl!("url-patterns")));

        // Pattern list
        let mut patterns_list =
            widget::list_column().list_item_padding([space.space_xxs, space.space_xs]);

        for (idx, pattern) in self.config.meeting_url_patterns.iter().enumerate() {
            patterns_list = patterns_list.add(
                widget::row::with_capacity(2)
                    .push(
                        widget::text_input("https://example.com/meeting/.*", pattern)
                            .on_input(move |s| Message::UpdatePattern(idx, s))
                            .width(Length::Fill),
                    )
                    .push(
                        widget::button::icon(widget::icon::from_name("edit-delete-symbolic"))
                            .extra_small()
                            .on_press(Message::RemovePattern(idx)),
                    )
                    .spacing(space.space_xs)
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );
        }

        content = content.push(patterns_list);

        // Add pattern button
        content = content
            .push(widget::button::standard(fl!("add-pattern")).on_press(Message::AddPattern));

        // URL patterns description
        content = content.push(widget::space::vertical().height(space.space_xxs));
        content = content
            .push(widget::text::caption(fl!("url-patterns-description")).class(secondary_text));
        content = content.push(widget::space::vertical().height(space.space_m));

        content.into()
    }

    /// Popup join button settings page
    fn view_popup_join_button_settings_page(&self) -> Element<'_, Message> {
        let space = spacing();
        let mut content = widget::column::with_capacity(8 + self.config.meeting_url_patterns.len())
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(settings_page_header(
            fl!("dropdown-display"),
            fl!("join-button-section"),
            Message::Navigate(PopupPage::PopupDisplaySettings),
        ));

        // Join button visibility dropdown options
        let join_options = vec![
            fl!("join-hide"),
            fl!("join-show"),
            fl!("join-show-same-day"),
            fl!("join-show-30m"),
            fl!("join-show-15m"),
            fl!("join-show-5m"),
        ];

        let join_idx = match self.config.popup_join_button {
            JoinButtonVisibility::Hide => Some(0),
            JoinButtonVisibility::Show => Some(1),
            JoinButtonVisibility::ShowIfSameDay => Some(2),
            JoinButtonVisibility::ShowIf30m => Some(3),
            JoinButtonVisibility::ShowIf15m => Some(4),
            JoinButtonVisibility::ShowIf5m => Some(5),
        };

        let settings_list = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("join-button-visibility")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(
                        join_options,
                        join_idx,
                        Message::SetPopupJoinButton,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );

        content = content.push(settings_list);

        // URL Patterns section
        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);
        content = content.push(widget::space::vertical().height(space.space_s));
        content = content.push(widget::text::body(fl!("url-patterns")));

        // Pattern list
        let mut patterns_list =
            widget::list_column().list_item_padding([space.space_xxs, space.space_xs]);

        for (idx, pattern) in self.config.meeting_url_patterns.iter().enumerate() {
            patterns_list = patterns_list.add(
                widget::row::with_capacity(2)
                    .push(
                        widget::text_input("https://example.com/meeting/.*", pattern)
                            .on_input(move |s| Message::UpdatePattern(idx, s))
                            .width(Length::Fill),
                    )
                    .push(
                        widget::button::icon(widget::icon::from_name("edit-delete-symbolic"))
                            .extra_small()
                            .on_press(Message::RemovePattern(idx)),
                    )
                    .spacing(space.space_xs)
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );
        }

        content = content.push(patterns_list);

        // Add pattern button
        content = content
            .push(widget::button::standard(fl!("add-pattern")).on_press(Message::AddPattern));

        // URL patterns description
        content = content.push(widget::space::vertical().height(space.space_xxs));
        content = content
            .push(widget::text::caption(fl!("url-patterns-description")).class(secondary_text));
        content = content.push(widget::space::vertical().height(space.space_m));

        content.into()
    }

    /// Panel display settings page
    fn view_panel_display_settings_page(&self) -> Element<'_, Message> {
        let space = spacing();
        let mut content = widget::column::with_capacity(8)
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(settings_page_header(
            fl!("settings"),
            fl!("panel-display"),
            Message::Navigate(PopupPage::Settings),
        ));

        // Display format dropdown
        let format_idx = match self.config.display_format {
            DisplayFormat::Relative => Some(1),
            _ => Some(0),
        };

        // Formatting section
        content = content.push(widget::text::heading(fl!("formatting-section")));

        let formatting_list = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            // Show app icon toggle
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("panel-show-icon")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.panel_show_icon)
                            .on_toggle(Message::SetPanelShowIcon),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            )
            // Display format dropdown
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("display-format-section")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(
                        display_format_options(),
                        format_idx,
                        Message::SelectDisplayFormat,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            )
            // Join button navigation
            .add(settings_nav_row(
                fl!("join-button-section"),
                join_button_visibility_summary(self.config.panel_join_button),
                Message::Navigate(PopupPage::PanelJoinButtonSettings),
            ))
            // Calendar indicator toggle
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("calendar-indicator-section")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.panel_calendar_indicator)
                            .on_toggle(Message::SetPanelCalendarIndicator),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            )
            // Physical location toggle
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("location-section")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.panel_show_location)
                            .on_toggle(Message::SetPanelShowLocation),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            )
            // Hide when no meetings toggle
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("hide-when-no-meetings")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.hide_when_no_meetings)
                            .on_toggle(Message::SetHideWhenNoMeetings),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );

        content = content.push(formatting_list);
        content = content.push(widget::space::vertical().height(space.space_m));

        content.into()
    }

    /// Popup display settings page
    fn view_popup_display_settings_page(&self) -> Element<'_, Message> {
        let space = spacing();
        let mut content = widget::column::with_capacity(8)
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(settings_page_header(
            fl!("settings"),
            fl!("dropdown-display"),
            Message::Navigate(PopupPage::Settings),
        ));

        // Show additional meetings count at top
        let additional_list = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("upcoming-events-section")))
                    .push(widget::space::horizontal())
                    .push(widget::spin_button(
                        self.config.upcoming_events_count.to_string(),
                        fl!("upcoming-events-section"),
                        i32::from(self.config.upcoming_events_count),
                        1,
                        0,
                        10,
                        Message::SetUpcomingEventsCount,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );

        content = content.push(additional_list);
        content = content.push(widget::space::vertical().height(space.space_xs));

        // Formatting section
        content = content.push(widget::text::heading(fl!("formatting-section")));

        let formatting_list = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            // Join button navigation
            .add(settings_nav_row(
                fl!("join-button-section"),
                join_button_visibility_summary(self.config.popup_join_button),
                Message::Navigate(PopupPage::PopupJoinButtonSettings),
            ))
            // Calendar indicator toggle
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("calendar-indicator-section")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.popup_calendar_indicator)
                            .on_toggle(Message::SetPopupCalendarIndicator),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            )
            // Physical location toggle
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("location-section")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.popup_show_location)
                            .on_toggle(Message::SetPopupShowLocation),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );

        content = content.push(formatting_list);
        content = content.push(widget::space::vertical().height(space.space_m));

        content.into()
    }

    /// Filter events settings page
    #[allow(clippy::too_many_lines)]
    fn view_events_to_show_settings_page(&self) -> Element<'_, Message> {
        use crate::config::{EventStatusFilter, TimeUntilFilter};

        let space = spacing();
        let mut content = widget::column::with_capacity(6)
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(
            widget::column::with_capacity(2)
                .push(
                    widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                        .extra_small()
                        .padding(space.space_none)
                        .label(fl!("settings"))
                        .spacing(space.space_xxxs)
                        .class(cosmic::theme::Button::Link)
                        .on_press(Message::Navigate(PopupPage::Settings)),
                )
                .push(widget::text::title4(fl!("filter-events-section")))
                .spacing(space.space_xxs),
        );

        // Status filter dropdown options
        let status_options = vec![
            fl!("status-filter-all"),
            fl!("status-filter-accepted"),
            fl!("status-filter-accepted-tentative"),
        ];
        let status_idx = match self.config.event_status_filter {
            EventStatusFilter::All => Some(0),
            EventStatusFilter::Accepted => Some(1),
            EventStatusFilter::AcceptedOrTentative => Some(2),
        };

        // In-progress meeting dropdown options
        let in_progress_options = vec![
            fl!("in-progress-off"),
            fl!("in-progress-5m"),
            fl!("in-progress-10m"),
            fl!("in-progress-15m"),
            fl!("in-progress-30m"),
        ];
        let in_progress_idx = match self.config.show_in_progress {
            InProgressMeeting::Off => Some(0),
            InProgressMeeting::Within5m => Some(1),
            InProgressMeeting::Within10m => Some(2),
            InProgressMeeting::Within15m => Some(3),
            InProgressMeeting::Within30m => Some(4),
        };

        // Time until filter dropdown options
        let time_until_options = vec![
            fl!("time-until-all"),
            fl!("time-until-3h"),
            fl!("time-until-6h"),
            fl!("time-until-same-day"),
            fl!("time-until-1d"),
            fl!("time-until-2d"),
        ];
        let time_until_idx = match self.config.time_until_filter {
            TimeUntilFilter::All => Some(0),
            TimeUntilFilter::Within3Hours => Some(1),
            TimeUntilFilter::Within6Hours => Some(2),
            TimeUntilFilter::SameDay => Some(3),
            TimeUntilFilter::Within1Day => Some(4),
            TimeUntilFilter::Within2Days => Some(5),
        };

        // Email summary for navigation link
        let email_count = self.config.additional_emails.len();
        let email_summary = fl!("additional-emails-summary", count = email_count);

        // Filter settings group
        let mut filter_settings = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            // Show all-day events toggle
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("show-all-day-events")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.show_all_day_events)
                            .on_toggle(Message::SetShowAllDayEvents),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            )
            // In-progress meeting dropdown
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("in-progress-section")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(
                        in_progress_options,
                        in_progress_idx,
                        Message::SetInProgressMeeting,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            )
            // Time until filter dropdown
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("time-until-section")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(
                        time_until_options,
                        time_until_idx,
                        Message::SetTimeUntilFilter,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            )
            // Status filter dropdown
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("status-filter-section")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(
                        status_options,
                        status_idx,
                        Message::SetEventStatusFilter,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );

        // Show email settings link only when filtering by status
        if self.config.event_status_filter != EventStatusFilter::All {
            filter_settings = filter_settings.add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("additional-emails-section")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::button::custom(
                            widget::row::with_capacity(2)
                                .push(widget::text::body(email_summary))
                                .push(
                                    widget::icon::from_name("go-next-symbolic").size(space.space_m),
                                )
                                .spacing(space.space_xxs)
                                .align_y(cosmic::iced::Alignment::Center),
                        )
                        .class(cosmic::theme::Button::Link)
                        .on_press(Message::Navigate(PopupPage::EmailSettings)),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );
        }

        content = content.push(filter_settings);

        // Description at bottom with secondary color
        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);
        content = content.push(widget::space::vertical().height(space.space_s));
        content = content
            .push(widget::text::caption(fl!("filter-events-description")).class(secondary_text));
        content = content.push(widget::space::vertical().height(space.space_m));

        content.into()
    }

    /// Email settings page for additional email addresses
    fn view_email_settings_page(&self) -> Element<'_, Message> {
        let space = spacing();
        let mut content = widget::column::with_capacity(6 + self.config.additional_emails.len())
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(
            widget::column::with_capacity(2)
                .push(
                    widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                        .extra_small()
                        .padding(space.space_none)
                        .label(fl!("filter-events-section"))
                        .spacing(space.space_xxxs)
                        .class(cosmic::theme::Button::Link)
                        .on_press(Message::Navigate(PopupPage::EventsToShowSettings)),
                )
                .push(widget::text::title4(fl!("additional-emails-section")))
                .spacing(space.space_xxs),
        );

        // Email list as a grouped list
        let mut emails_list =
            widget::list_column().list_item_padding([space.space_xxs, space.space_xs]);

        for (idx, email) in self.config.additional_emails.iter().enumerate() {
            emails_list = emails_list.add(
                widget::row::with_capacity(2)
                    .push(
                        widget::text_input("email@example.com", email)
                            .on_input(move |s| Message::UpdateEmail(idx, s))
                            .width(Length::Fill)
                            .id(email_input_id(idx)),
                    )
                    .push(
                        widget::button::icon(widget::icon::from_name("edit-delete-symbolic"))
                            .extra_small()
                            .on_press(Message::RemoveEmail(idx)),
                    )
                    .spacing(space.space_xs)
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );
        }

        content = content.push(emails_list);

        // Add email button
        content =
            content.push(widget::button::standard(fl!("add-email")).on_press(Message::AddEmail));

        // Description at bottom with secondary color
        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);
        content = content.push(widget::space::vertical().height(space.space_s));
        content = content.push(
            widget::text::caption(fl!("additional-emails-description")).class(secondary_text),
        );
        content = content.push(widget::space::vertical().height(space.space_m));

        content.into()
    }

    /// Refresh settings page
    fn view_refresh_settings_page(&self) -> Element<'_, Message> {
        let space = spacing();
        let mut content = widget::column::with_capacity(6)
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(
            widget::column::with_capacity(2)
                .push(
                    widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                        .extra_small()
                        .padding(space.space_none)
                        .label(fl!("settings"))
                        .spacing(space.space_xxxs)
                        .class(cosmic::theme::Button::Link)
                        .on_press(Message::Navigate(PopupPage::Settings)),
                )
                .push(widget::text::title4(fl!("refresh-section")))
                .spacing(space.space_xxs),
        );

        // Interval dropdown options
        let interval_options = vec![
            fl!("refresh-interval-5m"),
            fl!("refresh-interval-10m"),
            fl!("refresh-interval-15m"),
            fl!("refresh-interval-30m"),
        ];

        // Current interval index
        let interval_idx = match self.config.auto_refresh_interval_minutes {
            5 => Some(0),
            15 => Some(2),
            30 => Some(3),
            _ => Some(1), // 10 or any other value defaults to index 1
        };

        // Refresh settings group
        let mut refresh_settings = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            // Auto-refresh toggle
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("auto-refresh")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.auto_refresh_enabled)
                            .on_toggle(Message::SetAutoRefresh),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );

        // Only show interval dropdown when auto-refresh is enabled
        if self.config.auto_refresh_enabled {
            refresh_settings = refresh_settings.add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("refresh-interval")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(
                        interval_options,
                        interval_idx,
                        Message::SetAutoRefreshInterval,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );
        }

        content = content.push(refresh_settings);

        // Force sync manually button (centered, standard size)
        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);
        content = content.push(widget::space::vertical().height(space.space_xs));

        let sync_button = widget::button::standard(if self.is_refreshing {
            fl!("refreshing")
        } else {
            fl!("refresh-now")
        })
        .leading_icon(
            widget::icon::from_name("view-refresh-symbolic")
                .size(16)
                .handle(),
        );
        let sync_button = if self.is_refreshing {
            sync_button
        } else {
            sync_button.on_press(Message::RefreshCalendars)
        };

        content = content.push(
            widget::container(sync_button)
                .width(Length::Fill)
                .align_x(cosmic::iced::alignment::Horizontal::Center),
        );

        // Description at bottom with secondary color
        content = content.push(widget::space::vertical().height(space.space_s));
        content =
            content.push(widget::text::caption(fl!("refresh-description")).class(secondary_text));
        content = content.push(widget::space::vertical().height(space.space_m));

        content.into()
    }

    /// Calendar app settings page
    fn view_calendar_app_settings_page(&self) -> Element<'_, Message> {
        use crate::config::CalendarAppAction;

        let space = spacing();
        let mut content = widget::column::with_capacity(8)
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(
            widget::column::with_capacity(2)
                .push(
                    widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                        .extra_small()
                        .padding(space.space_none)
                        .label(fl!("settings"))
                        .spacing(space.space_xxxs)
                        .class(cosmic::theme::Button::Link)
                        .on_press(Message::Navigate(PopupPage::Settings)),
                )
                .push(widget::text::title4(fl!("calendar-app-section")))
                .spacing(space.space_xxs),
        );

        // Action dropdown options
        let action_options = vec![
            fl!("calendar-app-system"),
            fl!("calendar-app-command"),
            fl!("calendar-app-url"),
        ];
        let action_idx = match self.config.calendar_app_action {
            CalendarAppAction::SystemDefault => Some(0),
            CalendarAppAction::RunCommand => Some(1),
            CalendarAppAction::OpenUrl => Some(2),
        };

        // Settings group
        let mut settings_list = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            // Show calendar button toggle
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("calendar-app-show-button")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.show_calendar_button)
                            .on_toggle(Message::SetShowCalendarButton),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );

        // Only show action options when button is enabled
        if self.config.show_calendar_button {
            settings_list = settings_list.add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("calendar-app-action")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(
                        action_options,
                        action_idx,
                        Message::SetCalendarAppAction,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );

            // Show command input when RunCommand is selected
            if self.config.calendar_app_action == CalendarAppAction::RunCommand {
                settings_list = settings_list.add(
                    widget::row::with_capacity(2)
                        .push(widget::text::body(fl!("calendar-app-command-label")))
                        .push(widget::space::horizontal())
                        .align_y(cosmic::iced::Alignment::Center)
                        .width(Length::Fill),
                );
                settings_list = settings_list.add(
                    widget::text_input(
                        fl!("calendar-app-command-placeholder"),
                        &self.config.calendar_app_command,
                    )
                    .on_input(Message::SetCalendarAppCommand)
                    .width(Length::Fill),
                );
            }

            // Show URL input when OpenUrl is selected
            if self.config.calendar_app_action == CalendarAppAction::OpenUrl {
                settings_list = settings_list.add(
                    widget::row::with_capacity(2)
                        .push(widget::text::body(fl!("calendar-app-url-label")))
                        .push(widget::space::horizontal())
                        .align_y(cosmic::iced::Alignment::Center)
                        .width(Length::Fill),
                );
                settings_list = settings_list.add(
                    widget::text_input(
                        fl!("calendar-app-url-placeholder"),
                        &self.config.calendar_app_url,
                    )
                    .on_input(Message::SetCalendarAppUrl)
                    .width(Length::Fill),
                );
            }
        }

        content = content.push(settings_list);
        content = content.push(widget::space::vertical().height(space.space_m));

        content.into()
    }

    /// Scheduling Helper settings page
    fn view_scheduling_helper_settings_page(&self) -> Element<'_, Message> {
        let space = spacing();
        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);
        let mut content = widget::column::with_capacity(6)
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        content = content.push(settings_page_header(
            fl!("settings"),
            fl!("scheduling-section"),
            Message::Navigate(PopupPage::Settings),
        ));

        // Group 1: the on/off switch.
        let enable_group = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("scheduling-enable")))
                    .push(widget::space::horizontal())
                    .push(
                        widget::toggler(self.config.scheduling_helper_enabled)
                            .on_toggle(Message::SetSchedulingHelperEnabled),
                    )
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );
        content = content.push(enable_group);
        content = content.push(
            widget::text::caption(fl!("scheduling-helper-description")).class(secondary_text),
        );
        content = content.push(widget::space::vertical().height(space.space_xs));

        // Group 2: window mode.
        let window_options = vec![
            fl!("scheduling-window-fixed"),
            fl!("scheduling-window-resizable"),
        ];
        let window_idx = Some(usize::from(self.config.scheduling_helper_resizable));
        let window_group = widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(
                widget::row::with_capacity(3)
                    .push(widget::text::body(fl!("scheduling-window-mode")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(
                        window_options,
                        window_idx,
                        Message::SetSchedulingHelperWindowMode,
                    ))
                    .align_y(cosmic::iced::Alignment::Center)
                    .width(Length::Fill),
            );
        content = content.push(window_group);
        content = content.push(
            widget::text::caption(fl!("scheduling-window-description")).class(secondary_text),
        );

        content.into()
    }

    /// Keyboard shortcut setup page
    fn view_keyboard_shortcut_page(&self) -> Element<'_, Message> {
        let space = spacing();

        let mut content = widget::column::with_capacity(8)
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .width(Length::Fill);

        // Back button header
        content = content.push(settings_page_header(
            fl!("settings"),
            fl!("keyboard-shortcut"),
            Message::Navigate(PopupPage::Settings),
        ));

        // Description
        content = content.push(widget::text::body(fl!("keyboard-shortcut-description")));

        content = content.push(widget::space::vertical().height(space.space_xxs));

        // Instructions (second paragraph)
        content = content.push(widget::text::body(fl!("keyboard-shortcut-instructions")));

        content = content.push(widget::space::vertical().height(space.space_xs));

        // Action picker: which command the shortcut should run. A full-width
        // dropdown under an "Action" label.
        let actions = self.available_shortcut_actions();
        let selected = actions
            .iter()
            .position(|a| *a == self.shortcut_action)
            .unwrap_or(0);
        let action = actions.get(selected).copied().unwrap_or_default();
        let action_options: Vec<String> = actions.iter().map(|a| a.label()).collect();
        content = content.push(widget::text::body(fl!("shortcut-action-label")));
        content = content.push(
            widget::dropdown(action_options, Some(selected), Message::SetShortcutAction)
                .width(Length::Fill),
        );

        content = content.push(widget::space::vertical().height(space.space_xs));

        // Command in a styled container (read-only text input for selectability).
        // The binary differs in Flatpak; the flag depends on the selected action.
        let command = if std::env::var("FLATPAK_ID").is_ok() {
            format!(
                "flatpak run com.dangrover.next-meeting-app {}",
                action.flag()
            )
        } else {
            format!("cosmic-ext-applet-next-meeting {}", action.flag())
        };
        content = content.push(
            widget::container(
                widget::row::with_capacity(2)
                    .spacing(space.space_s)
                    .align_y(cosmic::iced::Alignment::Center)
                    .push(
                        widget::text(command.clone())
                            .font(cosmic::iced::Font::MONOSPACE)
                            .width(Length::Fill),
                    )
                    .push(
                        widget::button::text(fl!("keyboard-shortcut-copy"))
                            .class(cosmic::theme::Button::Standard)
                            .on_press(Message::CopyToClipboard(command)),
                    ),
            )
            .padding(space.space_s)
            .class(cosmic::theme::Container::List),
        );

        content = content.push(widget::space::vertical().height(space.space_s));

        // Open Settings button
        content = content.push(
            widget::container(
                widget::button::standard(fl!("keyboard-shortcut-open-settings"))
                    .on_press(Message::OpenCosmicSettings),
            )
            .width(Length::Fill)
            .align_x(cosmic::iced::alignment::Horizontal::Center),
        );

        content = content.push(widget::space::vertical().height(space.space_m));

        content.into()
    }

    /// About page with app info
    #[allow(clippy::unused_self)]
    fn view_about_page(&self) -> Element<'_, Message> {
        let space = spacing();
        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);

        let mut content = widget::column::with_capacity(6)
            .padding(space.space_xs)
            .spacing(space.space_xs)
            .align_x(cosmic::iced::Alignment::Center)
            .width(Length::Fill);

        // Back button header (left-aligned)
        content = content.push(
            widget::column::with_capacity(1)
                .push(
                    widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                        .extra_small()
                        .padding(space.space_none)
                        .label(fl!("settings"))
                        .spacing(space.space_xxxs)
                        .class(cosmic::theme::Button::Link)
                        .on_press(Message::Navigate(PopupPage::Settings)),
                )
                .width(Length::Fill),
        );

        // Vertical space before icon
        content = content.push(widget::space::vertical().height(space.space_m));

        // App icon (centered, large)
        content = content.push(widget::icon::from_name("com.dangrover.next-meeting-app").size(64));

        // App name
        content = content.push(widget::text::title3(fl!("app-title")));

        // Version with commit hash
        let version = env!("CARGO_PKG_VERSION");
        let git_hash = env!("GIT_HASH");
        let version_str = format!("{version} ({git_hash})");
        content = content
            .push(widget::text::body(fl!("version", version = version_str)).class(secondary_text));

        // Author
        let author = env!("CARGO_PKG_AUTHORS");
        content =
            content.push(widget::text::body(fl!("author", author = author)).class(secondary_text));

        // Website and Report bug buttons
        content = content.push(widget::space::vertical().height(space.space_s));
        content = content.push(
            widget::row::with_capacity(2)
                .spacing(space.space_s)
                .push(
                    widget::button::text(fl!("website"))
                        .class(cosmic::theme::Button::Link)
                        .on_press(Message::OpenUrl(
                            "https://github.com/dangrover/cosmic-ext-applet-next-meeting"
                                .to_string(),
                        )),
                )
                .push(
                    widget::button::text(fl!("report-bug"))
                        .class(cosmic::theme::Button::Link)
                        .on_press(Message::OpenUrl(
                            "https://github.com/dangrover/cosmic-ext-applet-next-meeting/issues"
                                .to_string(),
                        )),
                ),
        );

        // Vertical space at bottom
        content = content.push(widget::space::vertical().height(space.space_l));

        content.into()
    }
}

/// Open a URL in the default browser.
/// Returns true if the command was spawned successfully.
pub fn open_url(url: &str) -> bool {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .is_ok()
}

/// Open the system default calendar application.
fn open_system_calendar() {
    // Query the default calendar application
    let desktop_file = std::process::Command::new("xdg-mime")
        .args(["query", "default", "text/calendar"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if desktop_file.is_empty() {
        return;
    }

    // Try gtk-launch first (works on GTK-based systems)
    let gtk_result = std::process::Command::new("gtk-launch")
        .arg(&desktop_file)
        .spawn();

    // Fall back to gio launch with full path (for non-GTK systems like COSMIC)
    if gtk_result.is_err() {
        let xdg_dirs = xdg::BaseDirectories::new();
        if let Some(path) = xdg_dirs.find_data_file(format!("applications/{desktop_file}")) {
            let path_str = path.to_string_lossy();
            let _ = std::process::Command::new("gio")
                .args(["launch", path_str.as_ref()])
                .spawn();
        }
    }
}

/// Messages emitted by the application and its widgets.
#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    UpdateConfig(Config),
    MeetingsUpdated(Vec<Meeting>),
    CalendarsLoaded(CalendarDiscovery),
    ToggleCalendar(String),
    SelectDisplayFormat(usize),
    SetUpcomingEventsCount(i32),
    Navigate(PopupPage),
    OpenCalendar,
    OpenUrl(String),
    CopyToClipboard(String),
    SetPopupJoinButton(usize),
    SetPanelJoinButton(usize),
    SetPopupShowLocation(bool),
    SetPanelShowLocation(bool),
    SetPanelShowIcon(bool),
    SetPanelCalendarIndicator(bool),
    SetPopupCalendarIndicator(bool),
    UpdatePattern(usize, String),
    AddPattern,
    RemovePattern(usize),
    SetShowAllDayEvents(bool),
    SetInProgressMeeting(usize),
    SetEventStatusFilter(usize),
    SetTimeUntilFilter(usize),
    UpdateEmail(usize, String),
    AddEmail,
    RemoveEmail(usize),
    RefreshCalendars,
    RefreshCompleted,
    SetAutoRefresh(bool),
    SetAutoRefreshInterval(usize),
    SetShowCalendarButton(bool),
    SetCalendarAppAction(usize),
    SetCalendarAppCommand(String),
    SetCalendarAppUrl(String),
    CalendarChanged,
    /// EDS sources were added or removed (InterfacesAdded/Removed signal)
    SourcesChanged,
    /// GOA accounts status was checked
    AccountsChecked(Vec<crate::calendar::AccountNeedingAttention>),
    /// User clicked to open GNOME Online Accounts settings
    OpenOnlineAccounts,
    /// System resumed from sleep or session was unlocked
    SystemResumed,
    SetHideWhenNoMeetings(bool),
    OpenCosmicSettings,
    /// Toggle the Scheduling Helper menu entry on/off (settings).
    SetSchedulingHelperEnabled(bool),
    /// Choose the Scheduling Helper window mode (0 = fixed popup, 1 = resizable).
    SetSchedulingHelperWindowMode(usize),
    /// Choose which action the keyboard-shortcut page is configuring.
    SetShortcutAction(usize),
    /// Launch the Scheduling Helper as a separate window process.
    OpenSchedulingHelper,
    Noop,
}

/// Create a COSMIC application from the app model
impl cosmic::Application for AppModel {
    /// The async executor that will be used to run your application's commands.
    type Executor = cosmic::executor::Default;

    /// Data that your application receives to its init method.
    type Flags = ();

    /// Messages which the application and its widgets will emit.
    type Message = Message;

    /// Unique identifier in RDNN (reverse domain name notation) format.
    const APP_ID: &'static str = "com.dangrover.next-meeting-app";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    /// Initializes the application with any given flags and startup commands.
    fn init(
        core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        // Load configuration
        let config_context = cosmic_config::Config::new(Self::APP_ID, Config::VERSION).ok();
        let config = config_context
            .as_ref()
            .map(|ctx| Config::get_entry(ctx).unwrap_or_else(|(_e, c)| c))
            .unwrap_or_default();

        let enabled_uids = config.enabled_calendar_uids.clone();
        let upcoming_count = config.upcoming_events_count as usize;
        let additional_emails = config.additional_emails.clone();

        // Construct the app model with the runtime's core.
        let app = AppModel {
            core,
            config,
            config_context,
            // Assume EDS is present until discovery proves otherwise.
            eds_available: true,
            ..Default::default()
        };

        // Fetch initial calendar list and meeting data
        let calendars_task = Task::perform(
            async { crate::calendar::get_available_calendars().await },
            |calendars| Message::CalendarsLoaded(calendars).into(),
        );

        let meetings_task = Task::perform(
            async move {
                crate::calendar::get_upcoming_meetings(
                    &enabled_uids,
                    upcoming_count + 1,
                    &additional_emails,
                )
                .await
            },
            |meetings| Message::MeetingsUpdated(meetings).into(),
        );

        let accounts_task = Task::perform(
            async { crate::calendar::check_accounts_needing_attention().await },
            |accounts| Message::AccountsChecked(accounts).into(),
        );

        (
            app,
            Task::batch([calendars_task, meetings_task, accounts_task]),
        )
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    /// Describes the interface based on the current state of the application model.
    ///
    /// The applet's button in the panel will be drawn using the main view method.
    /// This view should emit messages to toggle the applet's popup window, which will
    /// be drawn using the `view_window` method.
    #[allow(clippy::too_many_lines)]
    fn view(&self) -> Element<'_, Self::Message> {
        use chrono::Local;
        let space = spacing();

        // In vertical panels, just show an icon since text won't fit well
        if !self.core.applet.is_horizontal() {
            let icon = widget::icon::from_name("com.dangrover.next-meeting-app-symbolic")
                .size(space.space_m);
            let button = widget::button::custom(icon)
                .class(cosmic::theme::Button::AppletIcon)
                .on_press(Message::TogglePopup);
            return self.core.applet.autosize_window(button).into();
        }

        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);

        // Build panel content based on whether we have meetings
        let filtered = self.filtered_meetings();

        // Hide the applet entirely when there are no meetings and the setting is enabled
        if self.config.hide_when_no_meetings
            && self.has_loaded_meetings
            && filtered.is_empty()
            && !self.available_calendars.is_empty()
        {
            return widget::container(widget::Space::new())
                .width(Length::Shrink)
                .into();
        }

        let (panel_content, show_panel_join) = if let Some(meeting) = filtered.first() {
            // Truncate title if needed
            let title = if meeting.title.len() > 30 {
                format!("{}...", &meeting.title[..27])
            } else {
                meeting.title.clone()
            };

            let now = Local::now();
            let minutes_until = meeting.start.signed_duration_since(now).num_minutes();

            // Check if we should show location based on visibility settings
            let is_same_day = meeting.start.date_naive() == now.date_naive();

            let physical_location = if self.config.panel_show_location {
                get_physical_location(meeting, &self.config.meeting_url_patterns)
            } else {
                None
            };

            // Get time string based on display format (smart formatting for date mode)
            // For in-progress meetings (already started), show "started" instead
            // For meetings starting right now (within a minute), show "now"
            let is_in_progress = meeting.start <= now;
            let is_starting_now = minutes_until <= 0 && minutes_until > -1;
            let time_str = if is_in_progress && !is_starting_now {
                fl!("panel-started")
            } else if is_starting_now || minutes_until == 0 {
                fl!("time-now")
            } else {
                match self.config.display_format {
                    DisplayFormat::Relative => {
                        let duration = meeting.start.signed_duration_since(now);
                        format_relative_time(duration)
                    }
                    _ => {
                        // Smart date formatting: just time if today, day+time if different day
                        format_panel_time(&meeting.start, &now)
                    }
                }
            };

            // Build the parenthetical string: "(time in Location)" or just "(time)"
            let info_str = match physical_location {
                Some(loc) => format!(
                    "  {}",
                    fl!(
                        "panel-time-location",
                        time = time_str.clone(),
                        location = loc
                    )
                ),
                None => format!("  {}", fl!("panel-time", time = time_str)),
            };

            // Create styled text with optional calendar indicator: "[dot] Title (time in Location)"
            let mut content = widget::row::with_capacity(3)
                .spacing(space.space_xxs)
                .align_y(cosmic::iced::Alignment::Center);

            // Add calendar indicator dot if enabled
            if self.config.panel_calendar_indicator
                && let Some(dot) = calendar_color_dot::<Message>(
                    &meeting.calendar_uid,
                    &self.available_calendars,
                    8.0,
                    Some(widget::tooltip::Position::Bottom),
                )
            {
                content = content.push(dot);
            }

            content = content
                .push(self.core.applet.text(title).font(cosmic::iced::font::Font {
                    weight: cosmic::iced::font::Weight::Bold,
                    ..cosmic::iced::font::Font::DEFAULT
                }))
                .push(self.core.applet.text(info_str).class(secondary_text));
            let join_url = match self.config.panel_join_button {
                JoinButtonVisibility::Show => {
                    extract_meeting_url(meeting, &self.config.meeting_url_patterns)
                }
                JoinButtonVisibility::ShowIfSameDay if is_same_day => {
                    extract_meeting_url(meeting, &self.config.meeting_url_patterns)
                }
                JoinButtonVisibility::ShowIf30m if minutes_until <= 30 => {
                    extract_meeting_url(meeting, &self.config.meeting_url_patterns)
                }
                JoinButtonVisibility::ShowIf15m if minutes_until <= 15 => {
                    extract_meeting_url(meeting, &self.config.meeting_url_patterns)
                }
                JoinButtonVisibility::ShowIf5m if minutes_until <= 5 => {
                    extract_meeting_url(meeting, &self.config.meeting_url_patterns)
                }
                JoinButtonVisibility::Hide
                | JoinButtonVisibility::ShowIfSameDay
                | JoinButtonVisibility::ShowIf30m
                | JoinButtonVisibility::ShowIf15m
                | JoinButtonVisibility::ShowIf5m => None,
            };

            (content, join_url)
        } else if !self.has_loaded_meetings {
            // Still loading initial data
            let content =
                widget::row::with_capacity(1).push(self.core.applet.text(fl!("loading-meetings")));
            (content, None)
        } else if self.available_calendars.is_empty() {
            // No calendars configured - show warning. Use a shorter label when
            // EDS itself is missing so the panel stays compact.
            let label = if self.eds_available {
                fl!("no-calendars")
            } else {
                fl!("no-calendar-service")
            };
            let content = widget::row::with_capacity(2)
                .spacing(space.space_xxs)
                .align_y(cosmic::iced::Alignment::Center)
                .push(widget::icon::from_name("dialog-warning-symbolic").size(space.space_s))
                .push(self.core.applet.text(label));
            (content, None)
        } else {
            let content =
                widget::row::with_capacity(1).push(self.core.applet.text(fl!("no-meetings-panel")));
            (content, None)
        };

        // Main panel button with meeting text, optionally preceded by the app icon
        let inner_content = if self.config.panel_show_icon {
            widget::row::with_capacity(2)
                .spacing(space.space_xxs)
                .align_y(cosmic::iced::Alignment::Center)
                .push(
                    widget::icon::from_name("com.dangrover.next-meeting-app-symbolic")
                        .size(space.space_m),
                )
                .push(panel_content)
                .into()
        } else {
            Element::from(panel_content)
        };
        let constrained_content =
            widget::container(inner_content).padding([space.space_none, space.space_xs]);
        let main_button = widget::button::custom(constrained_content)
            .class(cosmic::theme::Button::AppletIcon)
            .on_press(Message::TogglePopup);

        let mut row = widget::row::with_capacity(2)
            .push(main_button)
            .align_y(cosmic::iced::Alignment::Center)
            .spacing(space.space_xxs);

        // Add join button next to panel button if we should show it
        if let Some(url) = show_panel_join {
            // Use space_xxs (8) + space_xxxs (4) = 12px for compact panel text
            let font_size = space.space_xxs + space.space_xxxs;
            row = row.push(
                widget::button::custom(
                    widget::text(fl!("join"))
                        .size(font_size)
                        .font(cosmic::iced::font::Font {
                            weight: cosmic::iced::font::Weight::Bold,
                            ..cosmic::iced::font::Font::DEFAULT
                        })
                        .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
                            font_size.into(),
                        )),
                )
                .padding([space.space_xxxs, space.space_xxs])
                .class(cosmic::theme::Button::Suggested)
                .on_press(Message::OpenUrl(url)),
            );
        }

        self.core.applet.autosize_window(row).into()
    }

    /// The applet's popup window will be drawn using this view method. If there are
    /// multiple poups, you may match the id parameter to determine which popup to
    /// create a view for.
    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        let content: Element<'_, Self::Message> = match self.current_page {
            PopupPage::Main => self.view_main_page(),
            PopupPage::Settings => self.view_settings_page(),
            PopupPage::Calendars => self.view_calendars_page(),
            PopupPage::RefreshSettings => self.view_refresh_settings_page(),
            PopupPage::CalendarAppSettings => self.view_calendar_app_settings_page(),
            PopupPage::EventsToShowSettings => self.view_events_to_show_settings_page(),
            PopupPage::EmailSettings => self.view_email_settings_page(),
            PopupPage::PanelDisplaySettings => self.view_panel_display_settings_page(),
            PopupPage::PopupDisplaySettings => self.view_popup_display_settings_page(),
            PopupPage::PanelJoinButtonSettings => self.view_panel_join_button_settings_page(),
            PopupPage::PopupJoinButtonSettings => self.view_popup_join_button_settings_page(),
            PopupPage::SchedulingHelperSettings => self.view_scheduling_helper_settings_page(),
            PopupPage::KeyboardShortcut => self.view_keyboard_shortcut_page(),
            PopupPage::About => self.view_about_page(),
        };

        // Popup size limits
        let limits = Limits::NONE
            .max_width(420.0)
            .min_width(420.0)
            .min_height(200.0)
            .max_height(800.0);

        self.core
            .applet
            .popup_container(content)
            .limits(limits)
            .into()
    }

    /// Register subscriptions for this application.
    ///
    /// Subscriptions are long-lived async tasks running in the background which
    /// emit messages to the application through a channel. They may be conditionally
    /// activated by selectively appending to the subscription batch, and will
    /// continue to execute for the duration that they remain in the batch.
    #[allow(clippy::too_many_lines)]
    fn subscription(&self) -> Subscription<Self::Message> {
        use std::hash::{Hash, Hasher};

        let enabled_uids = self.config.enabled_calendar_uids.clone();
        let upcoming_count = self.config.upcoming_events_count as usize;
        let additional_emails = self.config.additional_emails.clone();
        let auto_refresh_enabled = self.config.auto_refresh_enabled;
        let auto_refresh_interval = self.config.auto_refresh_interval_minutes;

        // Create a unique subscription ID based on config values that affect filtering.
        // When these change, the subscription will be recreated with the new values.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        enabled_uids.hash(&mut hasher);
        upcoming_count.hash(&mut hasher);
        additional_emails.hash(&mut hasher);
        let config_hash = hasher.finish();

        // Create a separate hash for auto-refresh subscription
        let mut refresh_hasher = std::collections::hash_map::DefaultHasher::new();
        auto_refresh_enabled.hash(&mut refresh_hasher);
        auto_refresh_interval.hash(&mut refresh_hasher);
        enabled_uids.hash(&mut refresh_hasher);
        let refresh_hash = refresh_hasher.finish();

        let mut subscriptions = vec![
            // Periodically read cached calendar and meeting data (every 60 seconds)
            Subscription::run_with(
                (
                    config_hash,
                    enabled_uids.clone(),
                    upcoming_count,
                    additional_emails.clone(),
                ),
                |(_, enabled_uids, upcoming_count, additional_emails)| {
                    let enabled_uids = enabled_uids.clone();
                    let additional_emails = additional_emails.clone();
                    let upcoming_count = *upcoming_count;
                    cosmic::iced::stream::channel(
                        6,
                        move |mut channel: cosmic::iced::futures::channel::mpsc::Sender<
                            Message,
                        >| async move {
                            let mut interval =
                                tokio::time::interval(std::time::Duration::from_mins(1));
                            loop {
                                interval.tick().await;
                                // Read cached calendars and meetings
                                let calendars = crate::calendar::get_available_calendars().await;
                                let _ = channel.send(Message::CalendarsLoaded(calendars)).await;
                                let meetings = crate::calendar::get_upcoming_meetings(
                                    &enabled_uids,
                                    upcoming_count + 1,
                                    &additional_emails,
                                )
                                .await;
                                let _ = channel.send(Message::MeetingsUpdated(meetings)).await;
                                // Check for GOA accounts needing re-authentication
                                let accounts =
                                    crate::calendar::check_accounts_needing_attention().await;
                                let _ = channel.send(Message::AccountsChecked(accounts)).await;
                            }
                        },
                    )
                },
            ),
            // Watch for application configuration changes.
            self.core()
                .watch_config::<Config>(Self::APP_ID)
                .map(|update| Message::UpdateConfig(update.config)),
        ];

        // Add auto-refresh subscription if enabled
        if auto_refresh_enabled {
            let refresh_uids = self.config.enabled_calendar_uids.clone();
            subscriptions.push(Subscription::run_with(
                (refresh_hash, refresh_uids, auto_refresh_interval),
                |(_, refresh_uids, auto_refresh_interval)| {
                    let refresh_uids = refresh_uids.clone();
                    let auto_refresh_interval = *auto_refresh_interval;
                    cosmic::iced::stream::channel(
                        2,
                        move |mut channel: cosmic::iced::futures::channel::mpsc::Sender<
                            Message,
                        >| async move {
                            let interval_secs = u64::from(auto_refresh_interval) * 60;
                            let mut interval = tokio::time::interval(
                                std::time::Duration::from_secs(interval_secs),
                            );
                            // Skip the first immediate tick
                            interval.tick().await;
                            loop {
                                interval.tick().await;
                                // Trigger a refresh from remote servers
                                crate::calendar::refresh_calendars(&refresh_uids).await;
                                // Signal that refresh started (the 60-second subscription will pick up new data)
                                let _ = channel.send(Message::RefreshCalendars).await;
                            }
                        },
                    )
                },
            ));
        }

        // Watch for D-Bus PropertiesChanged signals from EDS calendars
        // This detects when calendars are updated after a sync (by us or external apps)
        let watch_uids = self.config.enabled_calendar_uids.clone();
        subscriptions.push(Subscription::run_with(
            ("calendar-changes", config_hash, watch_uids),
            |(_, _, watch_uids)| {
                let watch_uids = watch_uids.clone();
                cosmic::iced::stream::channel(
                    4,
                    move |mut channel: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
                    let (sender, mut receiver) = tokio::sync::mpsc::channel::<()>(4);

                    // Spawn the watcher in a separate task
                    let watch_task =
                        tokio::spawn(crate::calendar::watch_calendar_changes(watch_uids, sender));

                    // Forward messages from the watcher to the iced channel
                    while receiver.recv().await.is_some() {
                        let _ = channel.send(Message::CalendarChanged).await;
                    }

                    // Clean up if the watcher exits
                    watch_task.abort();
                })
            },
        ));

        // Watch for new/removed calendar sources via ObjectManager signals
        // Detects when the user adds or removes a calendar account
        subscriptions.push(Subscription::run_with("source-changes", |_| {
            cosmic::iced::stream::channel(
                2,
                move |mut channel: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
                let (sender, mut receiver) = tokio::sync::mpsc::channel::<()>(2);

                let watch_task = tokio::spawn(crate::calendar::watch_source_changes(sender));

                while receiver.recv().await.is_some() {
                    let _ = channel.send(Message::SourcesChanged).await;
                }

                watch_task.abort();
            })
        }));

        // Watch for system resume (from sleep) and session unlock events
        // Uses org.freedesktop.login1 on the system bus; fails gracefully on non-systemd systems
        subscriptions.push(Subscription::run_with("system-resume", |_| {
            cosmic::iced::stream::channel(
                2,
                move |mut channel: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
                let (sender, mut receiver) = tokio::sync::mpsc::channel::<()>(2);

                // Spawn the watcher in a separate task
                let watch_task = tokio::spawn(crate::calendar::watch_system_resume(sender));

                // Forward messages from the watcher to the iced channel
                while receiver.recv().await.is_some() {
                    let _ = channel.send(Message::SystemResumed).await;
                }

                // Clean up if the watcher exits
                watch_task.abort();
            })
        }));

        // Serve the D-Bus control interface so an external `--open-menu`
        // invocation (e.g. a keyboard shortcut) can open the panel popup.
        subscriptions.push(Subscription::run_with("menu-control", |_| {
            cosmic::iced::stream::channel(
                2,
                move |mut channel: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
                    let (sender, mut receiver) = tokio::sync::mpsc::channel::<()>(2);
                    // Hold the connection alive for as long as we forward calls.
                    if let Ok(_conn) = crate::ipc::serve(sender).await {
                        while receiver.recv().await.is_some() {
                            let _ = channel.send(Message::TogglePopup).await;
                        }
                    }
                },
            )
        }));

        Subscription::batch(subscriptions)
    }

    /// Handles messages emitted by the application and its widgets.
    ///
    /// Tasks may be returned for asynchronous execution of code in the background
    /// on the application's async runtime. The application will not exit until all
    /// tasks are finished.
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::UpdateConfig(config) => {
                self.config = config;
            }
            Message::MeetingsUpdated(meetings) => {
                self.upcoming_meetings = meetings;
                self.has_loaded_meetings = true;
            }
            Message::CalendarsLoaded(discovery) => {
                let CalendarDiscovery {
                    eds_available,
                    calendars,
                } = discovery;
                self.eds_available = eds_available;
                // Auto-enable newly discovered meeting-source calendars
                // (only when the user has explicitly selected calendars; if the
                // list is empty, all calendars are already implicitly enabled)
                if !self.config.enabled_calendar_uids.is_empty() {
                    let mut changed = false;
                    for cal in &calendars {
                        if cal.is_meeting_source()
                            && !self
                                .available_calendars
                                .iter()
                                .any(|prev| prev.uid == cal.uid)
                            && !self.config.enabled_calendar_uids.contains(&cal.uid)
                        {
                            self.config.enabled_calendar_uids.push(cal.uid.clone());
                            changed = true;
                        }
                    }
                    if changed {
                        self.save_config();
                    }
                }
                self.available_calendars = calendars;
            }
            Message::ToggleCalendar(uid) => {
                // If the list is empty (all enabled), populate it with all meeting-source calendars
                if self.config.enabled_calendar_uids.is_empty() {
                    self.config.enabled_calendar_uids = self
                        .available_calendars
                        .iter()
                        .filter(|c| c.is_meeting_source())
                        .map(|c| c.uid.clone())
                        .collect();
                }

                // Toggle the calendar
                if self.config.enabled_calendar_uids.contains(&uid) {
                    self.config.enabled_calendar_uids.retain(|u| u != &uid);
                } else {
                    self.config.enabled_calendar_uids.push(uid);
                }

                // Save config
                self.save_config();

                // Refresh meetings with new filter
                let enabled_uids = self.enabled_meeting_source_uids();
                let upcoming_count = self.config.upcoming_events_count as usize;
                let additional_emails = self.config.additional_emails.clone();
                return Task::perform(
                    async move {
                        crate::calendar::get_upcoming_meetings(
                            &enabled_uids,
                            upcoming_count + 1,
                            &additional_emails,
                        )
                        .await
                    },
                    |meetings| Message::MeetingsUpdated(meetings).into(),
                );
            }
            Message::SelectDisplayFormat(idx) => {
                self.config.display_format = match idx {
                    1 => DisplayFormat::Relative,
                    _ => DisplayFormat::DayAndTime, // 0 or any other value
                };
                self.save_config();
            }
            Message::SetUpcomingEventsCount(count) => {
                #[allow(clippy::cast_sign_loss)] // clamp(0, 10) ensures value is non-negative
                {
                    self.config.upcoming_events_count = count.clamp(0, 10) as u8;
                }
                self.save_config();
                // Refresh meetings with new count
                let enabled_uids = self.config.enabled_calendar_uids.clone();
                let upcoming_count = self.config.upcoming_events_count as usize;
                let additional_emails = self.config.additional_emails.clone();
                return Task::perform(
                    async move {
                        crate::calendar::get_upcoming_meetings(
                            &enabled_uids,
                            upcoming_count + 1,
                            &additional_emails,
                        )
                        .await
                    },
                    |meetings| Message::MeetingsUpdated(meetings).into(),
                );
            }
            Message::Navigate(page) => {
                // Save config when leaving display settings pages (for slider values)
                if matches!(
                    self.current_page,
                    PopupPage::PanelDisplaySettings | PopupPage::PopupDisplaySettings
                ) {
                    self.save_config();
                }
                self.current_page = page;
            }
            Message::OpenCalendar => {
                use crate::config::CalendarAppAction;
                match self.config.calendar_app_action {
                    CalendarAppAction::SystemDefault => {
                        open_system_calendar();
                    }
                    CalendarAppAction::RunCommand => {
                        if !self.config.calendar_app_command.is_empty() {
                            // Parse command and arguments
                            let parts: Vec<&str> = self
                                .config
                                .calendar_app_command
                                .split_whitespace()
                                .collect();
                            if let Some((cmd, args)) = parts.split_first() {
                                let _ = std::process::Command::new(cmd).args(args).spawn();
                            }
                        }
                    }
                    CalendarAppAction::OpenUrl => {
                        if !self.config.calendar_app_url.is_empty() {
                            open_url(&self.config.calendar_app_url);
                        }
                    }
                }
            }
            Message::OpenUrl(url) => {
                // Open the meeting URL using freedesktop portal (preferred) or xdg-open fallback
                open_url(&url);
            }
            Message::CopyToClipboard(text) => {
                return clipboard::write(text);
            }
            Message::SetPopupJoinButton(idx) => {
                self.config.popup_join_button = match idx {
                    0 => JoinButtonVisibility::Hide,
                    2 => JoinButtonVisibility::ShowIfSameDay,
                    3 => JoinButtonVisibility::ShowIf30m,
                    4 => JoinButtonVisibility::ShowIf15m,
                    5 => JoinButtonVisibility::ShowIf5m,
                    _ => JoinButtonVisibility::Show, // 1 or any other value
                };
                self.save_config();
            }
            Message::SetPanelJoinButton(idx) => {
                self.config.panel_join_button = match idx {
                    0 => JoinButtonVisibility::Hide,
                    2 => JoinButtonVisibility::ShowIfSameDay,
                    3 => JoinButtonVisibility::ShowIf30m,
                    4 => JoinButtonVisibility::ShowIf15m,
                    5 => JoinButtonVisibility::ShowIf5m,
                    _ => JoinButtonVisibility::Show, // 1 or any other value
                };
                self.save_config();
            }
            Message::SetPopupShowLocation(enabled) => {
                self.config.popup_show_location = enabled;
                self.save_config();
            }
            Message::SetPanelShowLocation(enabled) => {
                self.config.panel_show_location = enabled;
                self.save_config();
            }
            Message::SetPanelShowIcon(enabled) => {
                self.config.panel_show_icon = enabled;
                self.save_config();
            }
            Message::SetPanelCalendarIndicator(enabled) => {
                self.config.panel_calendar_indicator = enabled;
                self.save_config();
            }
            Message::SetHideWhenNoMeetings(enabled) => {
                self.config.hide_when_no_meetings = enabled;
                self.save_config();
            }
            Message::SetPopupCalendarIndicator(enabled) => {
                self.config.popup_calendar_indicator = enabled;
                self.save_config();
            }
            Message::UpdatePattern(idx, pattern) => {
                if idx < self.config.meeting_url_patterns.len() {
                    self.config.meeting_url_patterns[idx] = pattern;
                    self.save_config();
                }
            }
            Message::AddPattern => {
                self.config.meeting_url_patterns.push(String::new());
                self.save_config();
            }
            Message::RemovePattern(idx) => {
                if idx < self.config.meeting_url_patterns.len() {
                    self.config.meeting_url_patterns.remove(idx);
                    self.save_config();
                }
            }
            Message::SetShowAllDayEvents(enabled) => {
                self.config.show_all_day_events = enabled;
                self.save_config();
            }
            Message::SetInProgressMeeting(idx) => {
                self.config.show_in_progress = match idx {
                    1 => InProgressMeeting::Within5m,
                    2 => InProgressMeeting::Within10m,
                    3 => InProgressMeeting::Within15m,
                    4 => InProgressMeeting::Within30m,
                    _ => InProgressMeeting::Off, // 0 or any other value
                };
                self.save_config();
            }
            Message::SetEventStatusFilter(idx) => {
                use crate::config::EventStatusFilter;
                self.config.event_status_filter = match idx {
                    1 => EventStatusFilter::Accepted,
                    2 => EventStatusFilter::AcceptedOrTentative,
                    _ => EventStatusFilter::All, // 0 or any other value
                };
                self.save_config();
            }
            Message::SetTimeUntilFilter(idx) => {
                use crate::config::TimeUntilFilter;
                self.config.time_until_filter = match idx {
                    1 => TimeUntilFilter::Within3Hours,
                    2 => TimeUntilFilter::Within6Hours,
                    3 => TimeUntilFilter::SameDay,
                    4 => TimeUntilFilter::Within1Day,
                    5 => TimeUntilFilter::Within2Days,
                    _ => TimeUntilFilter::All, // 0 or any other value
                };
                self.save_config();
            }
            Message::UpdateEmail(idx, email) => {
                if let Some(e) = self.config.additional_emails.get_mut(idx) {
                    *e = email;
                }
                self.save_config();
            }
            Message::AddEmail => {
                let new_idx = self.config.additional_emails.len();
                self.config.additional_emails.push(String::new());
                self.save_config();
                // Focus the new email input field
                return cosmic::widget::text_input::focus(email_input_id(new_idx));
            }
            Message::RemoveEmail(idx) => {
                if idx < self.config.additional_emails.len() {
                    self.config.additional_emails.remove(idx);
                }
                self.save_config();
            }
            Message::RefreshCalendars => {
                if !self.is_refreshing {
                    self.is_refreshing = true;
                    let enabled_uids = self.enabled_meeting_source_uids();
                    let upcoming_count = self.config.upcoming_events_count as usize;
                    let additional_emails = self.config.additional_emails.clone();
                    return Task::perform(
                        async move {
                            // Ask EDS to re-discover calendars from accounts
                            crate::calendar::refresh_source_backends().await;
                            // Refresh existing calendars from remote servers
                            crate::calendar::refresh_calendars(&enabled_uids).await;
                            // Wait a moment for EDS to process
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            // Then fetch updated meetings
                            crate::calendar::get_upcoming_meetings(
                                &enabled_uids,
                                upcoming_count + 1,
                                &additional_emails,
                            )
                            .await
                        },
                        |meetings| Message::MeetingsUpdated(meetings).into(),
                    )
                    .chain(Task::done(Message::RefreshCompleted.into()));
                }
            }
            Message::RefreshCompleted => {
                self.is_refreshing = false;
            }
            Message::SetAutoRefresh(enabled) => {
                self.config.auto_refresh_enabled = enabled;
                self.save_config();
            }
            Message::SetAutoRefreshInterval(idx) => {
                self.config.auto_refresh_interval_minutes = match idx {
                    0 => 5,
                    2 => 15,
                    3 => 30,
                    _ => 10, // 1 or any other value
                };
                self.save_config();
            }
            Message::SetShowCalendarButton(enabled) => {
                self.config.show_calendar_button = enabled;
                self.save_config();
            }
            Message::SetCalendarAppAction(idx) => {
                use crate::config::CalendarAppAction;
                self.config.calendar_app_action = match idx {
                    1 => CalendarAppAction::RunCommand,
                    2 => CalendarAppAction::OpenUrl,
                    _ => CalendarAppAction::SystemDefault, // 0 or any other value
                };
                self.save_config();
            }
            Message::SetCalendarAppCommand(command) => {
                self.config.calendar_app_command = command;
                self.save_config();
            }
            Message::SetCalendarAppUrl(url) => {
                self.config.calendar_app_url = url;
                self.save_config();
            }
            Message::OpenCosmicSettings => {
                let _ = std::process::Command::new("cosmic-settings")
                    .arg("keyboard")
                    .spawn();
            }
            Message::SetSchedulingHelperEnabled(enabled) => {
                self.config.scheduling_helper_enabled = enabled;
                self.save_config();
            }
            Message::SetSchedulingHelperWindowMode(idx) => {
                self.config.scheduling_helper_resizable = idx == 1;
                self.save_config();
            }
            Message::SetShortcutAction(idx) => {
                if let Some(action) = self.available_shortcut_actions().get(idx) {
                    self.shortcut_action = *action;
                }
            }
            Message::OpenSchedulingHelper => {
                // Launch the helper as its own process (see scheduling_app.rs for
                // why a separate window process rather than an in-applet window).
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe)
                        .arg("--scheduling-helper")
                        .spawn();
                }
                // Close the popup now that we've handed off to the window.
                if let Some(p) = self.popup.take() {
                    self.current_page = PopupPage::Main;
                    return destroy_popup(p);
                }
            }
            Message::Noop => {}
            Message::AccountsChecked(accounts) => {
                self.accounts_needing_attention = accounts;
            }
            Message::OpenOnlineAccounts => {
                // gnome-control-center checks XDG_CURRENT_DESKTOP and refuses to
                // launch outside GNOME/Unity, so override it for this call.
                let _ = std::process::Command::new("gnome-control-center")
                    .arg("online-accounts")
                    .env("XDG_CURRENT_DESKTOP", "GNOME")
                    .spawn();
            }
            Message::SourcesChanged => {
                // EDS sources were added or removed — refresh calendar list and meetings.
                // The CalendarsLoaded handler will auto-enable any new calendars.
                let enabled_uids = self.enabled_meeting_source_uids();
                let upcoming_count = self.config.upcoming_events_count as usize;
                let additional_emails = self.config.additional_emails.clone();

                let calendars_task = Task::perform(
                    async { crate::calendar::get_available_calendars().await },
                    |calendars| Message::CalendarsLoaded(calendars).into(),
                );

                let meetings_task = Task::perform(
                    async move {
                        crate::calendar::get_upcoming_meetings(
                            &enabled_uids,
                            upcoming_count + 1,
                            &additional_emails,
                        )
                        .await
                    },
                    |meetings| Message::MeetingsUpdated(meetings).into(),
                );

                return Task::batch([calendars_task, meetings_task]);
            }
            Message::CalendarChanged => {
                // A calendar was updated via D-Bus signal (sync completed)
                // Refresh both calendars list (for updated sync timestamps) and meetings
                let enabled_uids = self.enabled_meeting_source_uids();
                let upcoming_count = self.config.upcoming_events_count as usize;
                let additional_emails = self.config.additional_emails.clone();

                let calendars_task = Task::perform(
                    async { crate::calendar::get_available_calendars().await },
                    |calendars| Message::CalendarsLoaded(calendars).into(),
                );

                let meetings_task = Task::perform(
                    async move {
                        crate::calendar::get_upcoming_meetings(
                            &enabled_uids,
                            upcoming_count + 1,
                            &additional_emails,
                        )
                        .await
                    },
                    |meetings| Message::MeetingsUpdated(meetings).into(),
                );

                return Task::batch([calendars_task, meetings_task]);
            }
            Message::SystemResumed => {
                // System woke from sleep or session was unlocked
                // Refresh immediately to show current data, and optionally trigger EDS sync
                let enabled_uids = self.enabled_meeting_source_uids();
                let upcoming_count = self.config.upcoming_events_count as usize;
                let additional_emails = self.config.additional_emails.clone();

                let mut tasks = vec![];

                // If auto-refresh is enabled, tell EDS to fetch fresh data from remote servers
                // The CalendarChanged handler will fire again when EDS finishes syncing
                if self.config.auto_refresh_enabled {
                    let refresh_uids = enabled_uids.clone();
                    tasks.push(Task::perform(
                        async move {
                            crate::calendar::refresh_calendars(&refresh_uids).await;
                        },
                        |()| Message::Noop.into(),
                    ));
                }

                // Also immediately refresh local data so we show what's cached
                tasks.push(Task::perform(
                    async { crate::calendar::get_available_calendars().await },
                    |calendars| Message::CalendarsLoaded(calendars).into(),
                ));

                tasks.push(Task::perform(
                    async move {
                        crate::calendar::get_upcoming_meetings(
                            &enabled_uids,
                            upcoming_count + 1,
                            &additional_emails,
                        )
                        .await
                    },
                    |meetings| Message::MeetingsUpdated(meetings).into(),
                ));

                return Task::batch(tasks);
            }
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let new_id = Id::unique();
                    self.popup.replace(new_id);
                    let mut popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    popup_settings.positioner.size_limits = Limits::NONE;
                    get_popup(popup_settings)
                };
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                    // Save any pending config changes (e.g., from sliders)
                    self.save_config();
                    // Reset to main page for next open
                    self.current_page = PopupPage::Main;
                }
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

/// Helper: Get summary text for join button visibility setting
fn join_button_visibility_summary(visibility: JoinButtonVisibility) -> String {
    match visibility {
        JoinButtonVisibility::Hide => fl!("join-hide"),
        JoinButtonVisibility::Show => fl!("join-show"),
        JoinButtonVisibility::ShowIfSameDay => fl!("join-show-same-day"),
        JoinButtonVisibility::ShowIf30m => fl!("join-show-30m"),
        JoinButtonVisibility::ShowIf15m => fl!("join-show-15m"),
        JoinButtonVisibility::ShowIf5m => fl!("join-show-5m"),
    }
}

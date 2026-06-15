// SPDX-License-Identifier: GPL-3.0-only
//
// Standalone Scheduling Helper window.
//
// Runs as a SEPARATE `cosmic::Application` process, launched by the panel applet
// via `--scheduling-helper`. A separate top-level app — rather than a window
// opened from inside the applet with `window::open` — is what behaves correctly
// under tiling window managers (the applet is a layer-shell surface; spawning an
// xdg-toplevel from it shows up as a stray background window). A fixed window
// size makes the COSMIC tiler float it as a dialog.
//
// The picker is a custom `canvas`-based week grid: calendar events render as
// faded colored blocks; the user drags vertically within a day column to mark
// availability (snapped to 15 minutes), and those spans drive the message text.

// Pixel math in the canvas does many small lossy int/float casts; that's expected.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Weekday};

use cosmic::iced::{Alignment, Color, Length, Limits, Point, Rectangle, Size, mouse};
use cosmic::prelude::*;
use cosmic::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use cosmic::widget::{self};

use crate::calendar::{CalendarEventBlock, CalendarInfo};
use crate::fl;
use crate::formatting::parse_hex_color;
use crate::scheduling::{self, Span};
use crate::widgets::{
    calendar_color_dot, secondary_text_style, settings_nav_row, settings_page_header, spacing,
};

const APP_ID: &str = "com.dangrover.next-meeting-app";
/// How many days of events to fetch up front (covers several weeks of paging).
const FETCH_DAYS: u32 = 28;
/// Highest week the user can page to (0 = current week).
const MAX_WEEK_OFFSET: u32 = 3;

// Canvas layout constants.
const GUTTER_W: f32 = 52.0;
const HEADER_H: f32 = 26.0;
/// Fixed pixel height per displayed hour. The grid is this tall per hour and
/// scrolls vertically when the visible hour range exceeds the pane.
const HOUR_H: f32 = 48.0;
const BLOCK_PAD: f32 = 1.5;
const DELETE_SIZE: f32 = 16.0;
const SNAP_MIN: i32 = 15;
/// Pixels from a block edge within which a drag resizes rather than creates.
const EDGE_GRAB: f32 = 5.0;
/// Height of each pane's header row, so the picker grid and the message box
/// (which sit below their headers) start at the same vertical position.
const HEADER_ROW_H: f32 = 32.0;

/// Launch the Scheduling Helper as its own floating window.
pub fn run() -> cosmic::iced::Result {
    use cosmic::cosmic_config::CosmicConfigEntry;

    let config = cosmic::cosmic_config::Config::new(APP_ID, crate::config::Config::VERSION)
        .ok()
        .map(|ctx| crate::config::Config::get_entry(&ctx).unwrap_or_else(|(_e, c)| c))
        .unwrap_or_default();

    // Window mode from config: a fixed-size popup floats as a dialog under tiling
    // WMs; a resizable window can be resized but may be tiled.
    let settings = if config.scheduling_helper_resizable {
        let limits = Limits::NONE.min_width(640.0).min_height(460.0);
        cosmic::app::Settings::default()
            .size(cosmic::iced::Size::new(880.0, 660.0))
            .size_limits(limits)
            .resizable(Some(1.0))
    } else {
        // Fixed-size floating dialog: `min == max` plus `resizable(None)` hints
        // the compositor to float it rather than tile it. The trade-off is that a
        // non-resizable window is clamped to this size, so maximizing it won't
        // reflow the content — the resizable mode is for that.
        let limits = Limits::NONE
            .min_width(980.0)
            .max_width(980.0)
            .min_height(660.0)
            .max_height(660.0);
        cosmic::app::Settings::default()
            .size(cosmic::iced::Size::new(980.0, 660.0))
            .size_limits(limits)
            .resizable(None)
    };

    cosmic::app::run::<SchedulingApp>(
        settings,
        (config.enabled_calendar_uids, config.additional_emails),
    )
}

pub struct SchedulingApp {
    core: cosmic::Core,
    enabled_uids: Vec<String>,
    additional_emails: Vec<String>,
    /// First hour of the day shown in the grid (0-23).
    day_start_hour: u32,
    /// Last hour of the day shown in the grid (1-24).
    day_end_hour: u32,
    /// Whether weekend columns are shown.
    include_weekends: bool,
    /// Which week is visible (0 = the rolling week starting today).
    week_offset: u32,
    /// All fetched calendar events.
    events: Vec<CalendarEventBlock>,
    /// Events filtered to the currently selected calendars (what the grid shows).
    visible_events: Vec<CalendarEventBlock>,
    /// Meeting-source calendars available from EDS (for the calendar picker).
    calendars: Vec<CalendarInfo>,
    /// Calendar UIDs currently checked in the picker (default: all enabled).
    selected_calendars: BTreeSet<String>,
    /// User-selected availability spans.
    availability: Vec<Span>,
    /// Whether events are still loading.
    loading: bool,
    /// Whether the options popover is open.
    show_options: bool,
    /// Which page the options popover is showing.
    options_page: OptionsPage,
}

/// Pages within the options popover (a small drill-down, like the menu).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OptionsPage {
    #[default]
    Main,
    Calendars,
}

#[derive(Debug, Clone)]
pub enum Msg {
    EventsLoaded(Vec<CalendarEventBlock>),
    CalendarsLoaded(Vec<CalendarInfo>),
    ToggleCalendar(String),
    SetDayStart(u32),
    SetDayEnd(u32),
    SetWeekends(bool),
    ToggleOptions,
    CloseOptions,
    OpenCalendarsPage,
    OptionsBack,
    PrevWeek,
    NextWeek,
    AddAvailability(DateTime<Local>, DateTime<Local>),
    ResizeAvailability(usize, DateTime<Local>, DateTime<Local>),
    RemoveAvailability(usize),
    Copy,
    Clear,
    AutoPick,
    Close,
}

impl SchedulingApp {
    /// The calendar days currently visible (today's rolling week, offset by
    /// `week_offset`), skipping weekends unless enabled.
    fn visible_days(&self) -> Vec<NaiveDate> {
        let base =
            Local::now().date_naive() + chrono::Duration::days(i64::from(self.week_offset) * 7);
        (0..7)
            .filter_map(|d| base.checked_add_days(chrono::Days::new(d)))
            .filter(|date| {
                self.include_weekends || !matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
            })
            .collect()
    }

    /// The composed availability message for the current selection.
    fn message_text(&self) -> String {
        // Prefer a friendly abbreviation (e.g. "PDT"); fall back to the numeric
        // offset only if the zone has no abbreviation available.
        let mut timezone = crate::locale::timezone_abbrev();
        if timezone.is_empty() {
            timezone = Local::now().format("%Z").to_string();
        }
        let intro = fl!("scheduling-message-intro", timezone = timezone);
        scheduling::compose_availability(&self.availability, &intro)
    }

    /// Fill availability with every free window (not covered by a calendar event)
    /// within the configured hours, across the visible days. Merged with anything
    /// already selected; past time today is skipped.
    fn auto_pick(&mut self) {
        let now = Local::now();
        let day_start = self.day_start_hour as i32 * 60;
        let day_end = self.day_end_hour as i32 * 60;
        if day_end <= day_start {
            return;
        }

        let mut added: Vec<Span> = Vec::new();
        for date in self.visible_days() {
            // Busy intervals from events on this day, clamped to the day window.
            let mut busy: Vec<(i32, i32)> = self
                .visible_events
                .iter()
                .filter_map(|e| {
                    if e.start.date_naive() != date {
                        return None;
                    }
                    let s = (e.start.hour() as i32 * 60 + e.start.minute() as i32)
                        .clamp(day_start, day_end);
                    let end_min = if e.end.date_naive() > date {
                        day_end
                    } else {
                        e.end.hour() as i32 * 60 + e.end.minute() as i32
                    }
                    .clamp(day_start, day_end);
                    (end_min > s).then_some((s, end_min))
                })
                .collect();
            busy.sort_unstable();

            // Merge overlapping busy intervals.
            let mut merged: Vec<(i32, i32)> = Vec::new();
            for (s, e) in busy {
                if let Some(last) = merged.last_mut()
                    && s <= last.1
                {
                    last.1 = last.1.max(e);
                } else {
                    merged.push((s, e));
                }
            }

            // Walk the gaps between busy intervals to emit free windows.
            let mut cursor = day_start;
            if date == now.date_naive() {
                let now_min = now.hour() as i32 * 60 + now.minute() as i32;
                cursor = cursor.max(now_min);
                let rem = cursor % SNAP_MIN; // round up to the next slot boundary
                if rem != 0 {
                    cursor += SNAP_MIN - rem;
                }
            }
            for (bs, be) in merged.iter().chain(std::iter::once(&(day_end, day_end))) {
                if *bs > cursor && bs - cursor >= SNAP_MIN {
                    added.push((local_dt(date, cursor), local_dt(date, *bs)));
                }
                cursor = cursor.max(*be);
            }
        }

        self.availability.extend(added);
        self.availability = scheduling::merge_spans(&self.availability);
    }

    /// Whether a calendar is enabled in the applet (empty config list = all
    /// meeting sources enabled). Disabled calendars show greyed-out and locked.
    fn is_calendar_enabled(&self, uid: &str) -> bool {
        self.enabled_uids.is_empty() || self.enabled_uids.iter().any(|u| u == uid)
    }

    /// Recompute the events shown in the grid from the selected-calendar set.
    fn recompute_visible_events(&mut self) {
        if self.calendars.is_empty() {
            // Calendar list not loaded yet — show everything fetched.
            self.visible_events = self.events.clone();
        } else {
            self.visible_events = self
                .events
                .iter()
                .filter(|e| self.selected_calendars.contains(&e.calendar_uid))
                .cloned()
                .collect();
        }
    }

    /// Build the task that fetches calendar events for the grid.
    fn events_task(&self) -> Task<cosmic::Action<Msg>> {
        let enabled = self.enabled_uids.clone();
        let emails = self.additional_emails.clone();
        Task::perform(
            async move { crate::calendar::get_event_blocks(&enabled, &emails, FETCH_DAYS).await },
            |events| Msg::EventsLoaded(events).into(),
        )
    }

    /// Main page of the options popover: hours, weekends, and a Calendars nav row.
    fn options_main_page(&self) -> Element<'_, Msg> {
        let space = spacing();
        let start_options: Vec<String> = (0..24).map(crate::locale::hour_axis_label).collect();
        let end_options: Vec<String> = (1..=24).map(crate::locale::hour_axis_label).collect();
        let start_idx = usize::try_from(self.day_start_hour).ok();
        let end_idx = usize::try_from(self.day_end_hour.saturating_sub(1)).ok();

        let total = self.calendars.len();
        let enabled_count = self
            .calendars
            .iter()
            .filter(|c| self.is_calendar_enabled(&c.uid))
            .count();
        let summary = if total > 0 && self.selected_calendars.len() == enabled_count {
            fl!("calendars-all")
        } else {
            fl!(
                "calendars-summary",
                selected = self.selected_calendars.len(),
                total = total
            )
        };

        widget::list_column()
            .list_item_padding([space.space_xxs, space.space_xs])
            .add(
                widget::row::with_capacity(5)
                    .spacing(space.space_xs)
                    .align_y(Alignment::Center)
                    .width(Length::Fill)
                    .push(widget::text::body(fl!("range-label")))
                    .push(widget::space::horizontal())
                    .push(widget::dropdown(start_options, start_idx, |i| {
                        Msg::SetDayStart(u32::try_from(i).unwrap_or(9))
                    }))
                    .push(widget::text::body("–"))
                    .push(widget::dropdown(end_options, end_idx, |i| {
                        Msg::SetDayEnd(u32::try_from(i).unwrap_or(16) + 1)
                    })),
            )
            .add(
                widget::row::with_capacity(3)
                    .spacing(space.space_s)
                    .align_y(Alignment::Center)
                    .width(Length::Fill)
                    .push(widget::text::body(fl!("weekends-label")))
                    .push(widget::space::horizontal())
                    .push(widget::toggler(self.include_weekends).on_toggle(Msg::SetWeekends)),
            )
            .add(settings_nav_row(
                fl!("calendars-label"),
                summary,
                Msg::OpenCalendarsPage,
            ))
            .into()
    }

    /// Calendars sub-page: a list of calendars with color pucks and togglers.
    /// Enabled calendars are checked by default and togglable; disabled ones are
    /// greyed out and locked.
    fn options_calendars_page(&self) -> Element<'_, Msg> {
        let space = spacing();
        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);

        let mut list = widget::list_column().list_item_padding([space.space_xxs, space.space_xs]);
        for cal in &self.calendars {
            let enabled = self.is_calendar_enabled(&cal.uid);
            let checked = self.selected_calendars.contains(&cal.uid);
            let puck: Element<'_, Msg> =
                calendar_color_dot::<Msg>(&cal.uid, &self.calendars, 12.0, None).unwrap_or_else(
                    || {
                        widget::container(widget::Space::new())
                            .width(Length::Fixed(12.0))
                            .height(Length::Fixed(12.0))
                            .into()
                    },
                );
            // Fill-width name pushes every toggler to the same right edge.
            let mut name = widget::text::body(cal.display_name.clone()).width(Length::Fill);
            if !enabled {
                name = name.class(secondary_text);
            }
            let mut tog = widget::toggler(enabled && checked);
            if enabled {
                let uid = cal.uid.clone();
                tog = tog.on_toggle(move |_| Msg::ToggleCalendar(uid.clone()));
            }
            list = list.add(
                widget::row::with_capacity(3)
                    .spacing(space.space_xs)
                    .align_y(Alignment::Center)
                    .width(Length::Fill)
                    .push(puck)
                    .push(name)
                    .push(tog),
            );
        }

        widget::column::with_capacity(2)
            .spacing(space.space_s)
            .push(settings_page_header(
                fl!("scheduling-options"),
                fl!("calendars-label"),
                Msg::OptionsBack,
            ))
            .push(widget::container(widget::scrollable(list).width(Length::Fill)).max_height(260.0))
            .into()
    }

    /// Build the task that fetches the available calendars (for the picker).
    fn calendars_task() -> Task<cosmic::Action<Msg>> {
        Task::perform(
            async { crate::calendar::get_available_calendars().await },
            |discovery| Msg::CalendarsLoaded(discovery.calendars).into(),
        )
    }
}

impl cosmic::Application for SchedulingApp {
    type Executor = cosmic::executor::Default;
    type Flags = (Vec<String>, Vec<String>);
    type Message = Msg;
    const APP_ID: &'static str = "com.dangrover.next-meeting-app.SchedulingHelper";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, flags: Self::Flags) -> (Self, Task<cosmic::Action<Msg>>) {
        let (enabled_uids, additional_emails) = flags;
        let mut app = SchedulingApp {
            core,
            enabled_uids,
            additional_emails,
            day_start_hour: 9,
            day_end_hour: 17,
            include_weekends: false,
            week_offset: 0,
            events: Vec::new(),
            visible_events: Vec::new(),
            calendars: Vec::new(),
            selected_calendars: BTreeSet::new(),
            availability: Vec::new(),
            loading: true,
            show_options: false,
            options_page: OptionsPage::Main,
        };

        app.set_header_title(fl!("scheduling-helper"));
        let title_task = app.core.main_window_id().map_or_else(Task::none, |id| {
            app.set_window_title(fl!("scheduling-helper"), id)
        });
        let events_task = app.events_task();

        (
            app,
            Task::batch([title_task, events_task, Self::calendars_task()]),
        )
    }

    fn on_close_requested(&self, _id: cosmic::iced::window::Id) -> Option<Msg> {
        Some(Msg::Close)
    }

    fn update(&mut self, message: Msg) -> Task<cosmic::Action<Msg>> {
        match message {
            Msg::EventsLoaded(events) => {
                self.events = events;
                self.loading = false;
                self.recompute_visible_events();
            }
            Msg::CalendarsLoaded(calendars) => {
                self.calendars = calendars
                    .into_iter()
                    .filter(CalendarInfo::is_meeting_source)
                    .collect();
                // Default: every enabled calendar is checked.
                self.selected_calendars = self
                    .calendars
                    .iter()
                    .filter(|c| self.is_calendar_enabled(&c.uid))
                    .map(|c| c.uid.clone())
                    .collect();
                self.recompute_visible_events();
            }
            Msg::ToggleCalendar(uid) => {
                // Only enabled calendars can be toggled.
                if self.is_calendar_enabled(&uid) {
                    if !self.selected_calendars.remove(&uid) {
                        self.selected_calendars.insert(uid);
                    }
                    self.recompute_visible_events();
                }
            }
            Msg::SetDayStart(hour) => {
                self.day_start_hour = hour;
                if self.day_end_hour <= hour {
                    self.day_end_hour = (hour + 1).min(24);
                }
            }
            Msg::SetDayEnd(hour) => {
                self.day_end_hour = hour;
                if self.day_start_hour >= hour {
                    self.day_start_hour = hour.saturating_sub(1);
                }
            }
            Msg::SetWeekends(enabled) => self.include_weekends = enabled,
            Msg::ToggleOptions => {
                self.show_options = !self.show_options;
                if self.show_options {
                    self.options_page = OptionsPage::Main;
                }
            }
            Msg::CloseOptions => self.show_options = false,
            Msg::OpenCalendarsPage => self.options_page = OptionsPage::Calendars,
            Msg::OptionsBack => self.options_page = OptionsPage::Main,
            Msg::PrevWeek => self.week_offset = self.week_offset.saturating_sub(1),
            Msg::NextWeek => self.week_offset = (self.week_offset + 1).min(MAX_WEEK_OFFSET),
            Msg::AddAvailability(start, end) => {
                // Merge with any overlapping/touching spans so availability never
                // contains overlaps.
                self.availability.push((start, end));
                self.availability = scheduling::merge_spans(&self.availability);
            }
            Msg::ResizeAvailability(i, start, end) => {
                if i < self.availability.len() {
                    self.availability[i] = (start, end);
                    self.availability = scheduling::merge_spans(&self.availability);
                }
            }
            Msg::RemoveAvailability(i) => {
                if i < self.availability.len() {
                    self.availability.remove(i);
                }
            }
            Msg::Copy => {
                let text = self.message_text();
                if !text.is_empty() {
                    return cosmic::iced::clipboard::write(text);
                }
            }
            Msg::Clear => self.availability.clear(),
            Msg::AutoPick => self.auto_pick(),
            Msg::Close => std::process::exit(0),
        }
        Task::none()
    }

    #[allow(clippy::too_many_lines)]
    fn view(&self) -> Element<'_, Msg> {
        let space = spacing();
        let secondary_text = cosmic::theme::Text::Custom(secondary_text_style);

        // ----- Picker pane (left): week nav (+ options gear) + canvas grid -----
        let days = self.visible_days();
        let week_label = match (days.first(), days.last()) {
            (Some(first), Some(last)) => format!(
                "{} – {}",
                crate::locale::short_date(*first),
                crate::locale::short_date(*last)
            ),
            _ => String::new(),
        };

        let mut prev =
            widget::button::icon(widget::icon::from_name("go-previous-symbolic")).extra_small();
        if self.week_offset > 0 {
            prev = prev.on_press(Msg::PrevWeek);
        }
        let mut next =
            widget::button::icon(widget::icon::from_name("go-next-symbolic")).extra_small();
        if self.week_offset < MAX_WEEK_OFFSET {
            next = next.on_press(Msg::NextWeek);
        }

        // Visible-hours + weekends settings collapse into a gear-triggered popover.
        let gear = widget::button::icon(widget::icon::from_name("preferences-system-symbolic"))
            .extra_small()
            .on_press(Msg::ToggleOptions);
        let mut options = widget::popover(gear).position(widget::popover::Position::Bottom);
        if self.show_options {
            let panel_inner = match self.options_page {
                OptionsPage::Main => self.options_main_page(),
                OptionsPage::Calendars => self.options_calendars_page(),
            };
            let panel = widget::container(panel_inner)
                .padding(space.space_s)
                .width(Length::Fixed(320.0))
                .class(cosmic::theme::Container::Dialog);
            options = options.popup(panel).on_close(Msg::CloseOptions);
        }

        let nav = widget::row::with_capacity(5)
            .spacing(space.space_xs)
            .height(Length::Fixed(HEADER_ROW_H))
            .align_y(Alignment::Center)
            .push(widget::text::heading(fl!("scheduling-pick-times")))
            .push(widget::space::horizontal())
            .push(prev)
            .push(
                widget::text::body(week_label)
                    .width(Length::Fixed(120.0))
                    .center(),
            )
            .push(next)
            .push(options);

        let grid_content: Element<'_, Msg> = if self.loading {
            widget::text::body(fl!("scheduling-loading"))
                .class(secondary_text)
                .into()
        } else {
            // The canvas fills the pane and scrolls its hour range internally (a
            // tall canvas inside a scrollable produces compositing artifacts), so
            // it keeps fixed day headers and crisp drag coordinates.
            canvas::Canvas::new(CalendarGrid {
                days,
                day_start_hour: self.day_start_hour,
                day_end_hour: self.day_end_hour,
                events: &self.visible_events,
                availability: &self.availability,
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
        };

        let picker_pane = widget::column::with_capacity(2)
            .spacing(space.space_s)
            .width(Length::FillPortion(3))
            .height(Length::Fill)
            .push(nav)
            .push(
                widget::container(grid_content)
                    .padding(space.space_xxs)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .class(cosmic::theme::Container::List),
            );

        // ----- Message pane (right) -----
        let message_text = self.message_text();
        let has_text = !message_text.is_empty();
        // `width(Fill)` is required for the text to wrap to the pane width rather
        // than lay out on one long line.
        let preview: Element<'_, Msg> = if has_text {
            widget::text::body(message_text)
                .width(Length::Fill)
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into()
        } else {
            widget::text::body(fl!("scheduling-empty-hint"))
                .class(secondary_text)
                .width(Length::Fill)
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into()
        };

        // Copy is the primary action, ~1.5x the width of a standard button, with
        // its icon + label centered. (A fixed-width `button::suggested` would
        // left-align them, so build the content and center it explicitly; the
        // Suggested class still colors the icon/text correctly.)
        let copy_inner = widget::container(
            widget::row::with_capacity(2)
                .spacing(space.space_xxs)
                .align_y(Alignment::Center)
                .push(widget::icon::from_name("edit-copy-symbolic").size(16))
                .push(widget::text::body(fl!("scheduling-copy"))),
        )
        .center(Length::Fill);
        // Match the text buttons: they use a fixed height of `space_l`.
        let mut copy_button = widget::button::custom(copy_inner)
            .class(cosmic::theme::Button::Suggested)
            .width(Length::Fixed(150.0))
            .height(Length::Fixed(f32::from(space.space_l)))
            .padding([0.0, f32::from(space.space_s)]);
        if has_text {
            copy_button = copy_button.on_press(Msg::Copy);
        }

        let message_pane = widget::column::with_capacity(2)
            .spacing(space.space_s)
            .width(Length::FillPortion(2))
            .height(Length::Fill)
            .push(
                widget::row::with_capacity(1)
                    .height(Length::Fixed(HEADER_ROW_H))
                    .align_y(Alignment::Center)
                    .push(widget::text::heading(fl!("your-message"))),
            )
            .push(
                widget::container(widget::scrollable(preview).width(Length::Fill))
                    .padding(space.space_s)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .class(cosmic::theme::Container::List),
            );

        let body = widget::row::with_capacity(2)
            .spacing(space.space_m)
            .height(Length::Fill)
            .push(picker_pane)
            .push(message_pane);

        // Bottom action row spanning both columns: actions on the left, Copy
        // right-aligned under the message box.
        let actions = widget::row::with_capacity(4)
            .spacing(space.space_xs)
            .align_y(Alignment::Center)
            .push(
                widget::button::standard(fl!("scheduling-auto-pick"))
                    .leading_icon(widget::icon::from_name("edit-select-all-symbolic"))
                    .on_press(Msg::AutoPick),
            )
            .push(
                widget::button::standard(fl!("scheduling-clear"))
                    .leading_icon(widget::icon::from_name("edit-clear-symbolic"))
                    .on_press(Msg::Clear),
            )
            .push(widget::space::horizontal())
            .push(copy_button);

        widget::container(
            widget::column::with_capacity(2)
                .spacing(space.space_s)
                .width(Length::Fill)
                .height(Length::Fill)
                .push(body)
                .push(actions),
        )
        .padding(space.space_m)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

/// Resolved pixel layout of the grid for given canvas bounds + scroll offset.
struct Layout {
    gx: f32,     // left edge of the day columns (after the hour gutter)
    gy: f32,     // top edge of the scrollable band (below the day headers)
    gw: f32,     // width of the day-columns area
    band_h: f32, // visible vertical band height (viewport for the hours)
    col_w: f32,
    cols: usize,
    start_min: i32, // first minute-of-day shown
    total_min: i32, // minutes spanned (day_end - day_start)
    content_h: f32, // full pixel height of all hours at HOUR_H each
    scroll: f32,    // clamped vertical scroll offset in pixels
}

impl Layout {
    fn bottom_min(&self) -> i32 {
        self.start_min + self.total_min
    }
    fn visible_bottom(&self) -> f32 {
        self.gy + self.band_h
    }
    fn max_scroll(&self) -> f32 {
        (self.content_h - self.band_h).max(0.0)
    }
}

/// The interactive week-grid canvas program.
struct CalendarGrid<'a> {
    days: Vec<NaiveDate>,
    day_start_hour: u32,
    day_end_hour: u32,
    events: &'a [CalendarEventBlock],
    availability: &'a [Span],
}

/// Which edge of an availability block a resize drag moves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Top,
    Bottom,
}

/// An in-progress pointer gesture on the grid.
#[derive(Clone, Copy)]
enum Active {
    /// Dragging out a new availability span in a day column. `min_bound`/
    /// `max_bound` confine the drag to the free gap between neighboring spans so
    /// it can never overlap one.
    Create {
        day: usize,
        anchor_min: i32,
        current_min: i32,
        min_bound: i32,
        max_bound: i32,
    },
    /// Resizing an existing span by one edge; `fixed_min` stays put.
    Resize {
        index: usize,
        day: usize,
        fixed_min: i32,
        moving_min: i32,
    },
}

#[derive(Default)]
struct GridState {
    active: Option<Active>,
    /// Index of the availability span currently hovered (shows its delete ×).
    hover: Option<usize>,
    /// Index of the calendar event currently hovered (shows its full title).
    hover_event: Option<usize>,
    /// Vertical scroll offset in pixels.
    scroll: f32,
}

impl CalendarGrid<'_> {
    fn layout(&self, bounds: Rectangle, scroll: f32) -> Option<Layout> {
        let cols = self.days.len();
        let gw = bounds.width - GUTTER_W;
        let band_h = bounds.height - HEADER_H;
        if cols == 0 || gw <= 0.0 || band_h <= 0.0 || self.day_end_hour <= self.day_start_hour {
            return None;
        }
        let hours = self.day_end_hour - self.day_start_hour;
        let content_h = hours as f32 * HOUR_H;
        Some(Layout {
            gx: GUTTER_W,
            gy: HEADER_H,
            gw,
            band_h,
            col_w: gw / cols as f32,
            cols,
            start_min: self.day_start_hour as i32 * 60,
            total_min: hours as i32 * 60,
            content_h,
            scroll: scroll.clamp(0.0, (content_h - band_h).max(0.0)),
        })
    }

    fn y_of(l: &Layout, abs_min: i32) -> f32 {
        l.gy + ((abs_min - l.start_min) as f32 / 60.0) * HOUR_H - l.scroll
    }

    fn col_x(l: &Layout, col: usize) -> f32 {
        l.gx + col as f32 * l.col_w
    }

    /// Snap a y pixel to an absolute minute-of-day on the 15-minute grid.
    fn snap_y(l: &Layout, y: f32) -> i32 {
        let raw = l.start_min as f32 + ((y - l.gy + l.scroll) / HOUR_H) * 60.0;
        ((raw / SNAP_MIN as f32).round() as i32 * SNAP_MIN).clamp(l.start_min, l.bottom_min())
    }

    /// Map a cursor point to a (column, snapped minute-of-day), if over the grid.
    fn locate(l: &Layout, p: Point) -> Option<(usize, i32)> {
        if p.x < l.gx || p.y < l.gy {
            return None;
        }
        let col = ((p.x - l.gx) / l.col_w).floor() as usize;
        if col >= l.cols {
            return None;
        }
        Some((col, Self::snap_y(l, p.y)))
    }

    /// Column index and clamped `[lo, hi]` minutes for a `[start, end)` block.
    fn span_minutes(
        &self,
        l: &Layout,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> Option<(usize, i32, i32)> {
        let day = start.date_naive();
        let col = self.days.iter().position(|d| *d == day)?;
        let lo =
            (start.hour() as i32 * 60 + start.minute() as i32).clamp(l.start_min, l.bottom_min());
        let hi_raw = if end.date_naive() > day {
            l.bottom_min()
        } else {
            end.hour() as i32 * 60 + end.minute() as i32
        };
        Some((col, lo, hi_raw.clamp(l.start_min, l.bottom_min())))
    }

    /// On-screen rectangle for a block, clipped to the visible band. `None` if it
    /// is entirely scrolled out of view.
    fn rect_in_band(l: &Layout, col: usize, lo: i32, hi: i32) -> Option<Rectangle> {
        if hi <= lo {
            return None;
        }
        let top = Self::y_of(l, lo).max(l.gy);
        let bot = Self::y_of(l, hi).min(l.visible_bottom());
        if bot <= top {
            return None;
        }
        Some(Rectangle {
            x: Self::col_x(l, col) + BLOCK_PAD,
            y: top,
            width: (l.col_w - 2.0 * BLOCK_PAD).max(1.0),
            height: (bot - top).max(2.0),
        })
    }

    fn block_rect(
        &self,
        l: &Layout,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> Option<Rectangle> {
        let (col, lo, hi) = self.span_minutes(l, start, end)?;
        Self::rect_in_band(l, col, lo, hi)
    }

    /// Index of the availability span whose visible block is under the cursor.
    fn avail_at(&self, l: &Layout, p: Point) -> Option<usize> {
        self.availability
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| self.block_rect(l, s.0, s.1).is_some_and(|r| r.contains(p)))
            .map(|(i, _)| i)
    }

    /// Index of an availability span whose delete (×) hotspot is under the cursor.
    fn delete_at(&self, l: &Layout, p: Point) -> Option<usize> {
        self.availability
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| {
                self.block_rect(l, s.0, s.1).is_some_and(|r| {
                    Rectangle {
                        x: r.x + r.width - DELETE_SIZE,
                        y: r.y,
                        width: DELETE_SIZE,
                        height: DELETE_SIZE,
                    }
                    .contains(p)
                })
            })
            .map(|(i, _)| i)
    }

    /// Index of the calendar event whose visible block is under the cursor.
    fn event_at(&self, l: &Layout, p: Point) -> Option<usize> {
        self.events
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| {
                self.block_rect(l, e.start, e.end)
                    .is_some_and(|r| r.contains(p))
            })
            .map(|(i, _)| i)
    }

    /// If the cursor is within the grab zone of a visible block's top or bottom
    /// edge, return that span index and which edge.
    fn edge_at(&self, l: &Layout, p: Point) -> Option<(usize, Edge)> {
        for (i, s) in self.availability.iter().enumerate().rev() {
            let Some((col, lo, hi)) = self.span_minutes(l, s.0, s.1) else {
                continue;
            };
            let x0 = Self::col_x(l, col) + BLOCK_PAD;
            let x1 = x0 + (l.col_w - 2.0 * BLOCK_PAD).max(1.0);
            if p.x < x0 || p.x > x1 {
                continue;
            }
            let y_top = Self::y_of(l, lo);
            let y_bot = Self::y_of(l, hi);
            if y_top >= l.gy && (p.y - y_top).abs() <= EDGE_GRAB {
                return Some((i, Edge::Top));
            }
            if y_bot <= l.visible_bottom() && (p.y - y_bot).abs() <= EDGE_GRAB {
                return Some((i, Edge::Bottom));
            }
        }
        None
    }

    /// The free vertical gap (in minutes) on `col`'s day that contains `min`,
    /// bounded by existing availability spans. Returns `None` if `min` falls
    /// inside an existing span, or the surrounding gap is too small for a slot —
    /// in either case a new span can't be started there without overlapping.
    fn free_gap(&self, l: &Layout, col: usize, min: i32) -> Option<(i32, i32)> {
        let mut lo = l.start_min;
        let mut hi = l.bottom_min();
        for s in self.availability {
            let Some((c, s_min, e_min)) = self.span_minutes(l, s.0, s.1) else {
                continue;
            };
            if c != col {
                continue;
            }
            if min > s_min && min < e_min {
                return None; // strictly inside an existing span
            }
            if e_min <= min {
                lo = lo.max(e_min);
            }
            if s_min >= min {
                hi = hi.min(s_min);
            }
        }
        (hi - lo >= SNAP_MIN).then_some((lo, hi))
    }
}

impl canvas::Program<Msg, cosmic::Theme> for CalendarGrid<'_> {
    type State = GridState;

    #[allow(clippy::too_many_lines)]
    fn update(
        &self,
        state: &mut GridState,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Msg>> {
        let l = self.layout(bounds, state.scroll)?;
        let pos = cursor.position_in(bounds);

        match event {
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                pos?; // only scroll when the cursor is over the canvas
                let step = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y * HOUR_H,
                    mouse::ScrollDelta::Pixels { y, .. } => *y,
                };
                let new = (state.scroll - step).clamp(0.0, l.max_scroll());
                if (new - state.scroll).abs() > f32::EPSILON {
                    state.scroll = new;
                    return Some(canvas::Action::request_redraw().and_capture());
                }
                None
            }
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = pos?;
                // Starting an interaction hides hover affordances (× / tooltip).
                state.hover = None;
                state.hover_event = None;
                if let Some(i) = self.delete_at(&l, p) {
                    return Some(canvas::Action::publish(Msg::RemoveAvailability(i)).and_capture());
                }
                if let Some((i, edge)) = self.edge_at(&l, p)
                    && let Some((col, lo, hi)) =
                        self.span_minutes(&l, self.availability[i].0, self.availability[i].1)
                {
                    let (fixed_min, moving_min) = match edge {
                        Edge::Top => (hi, lo),
                        Edge::Bottom => (lo, hi),
                    };
                    state.active = Some(Active::Resize {
                        index: i,
                        day: col,
                        fixed_min,
                        moving_min,
                    });
                    return Some(canvas::Action::request_redraw().and_capture());
                }
                let (col, min) = Self::locate(&l, p)?;
                // Only start a create-drag in a free gap, so it can't overlap an
                // existing span (or even begin inside one).
                let (min_bound, max_bound) = self.free_gap(&l, col, min)?;
                state.active = Some(Active::Create {
                    day: col,
                    anchor_min: min,
                    current_min: min,
                    min_bound,
                    max_bound,
                });
                Some(canvas::Action::request_redraw().and_capture())
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let Some(active) = state.active.as_mut() {
                    if let Some(p) = pos {
                        let snapped = Self::snap_y(&l, p.y);
                        match active {
                            Active::Create {
                                current_min,
                                min_bound,
                                max_bound,
                                ..
                            } => *current_min = snapped.clamp(*min_bound, *max_bound),
                            Active::Resize { moving_min, .. } => *moving_min = snapped,
                        }
                    }
                    // Capture so the gesture owns the event stream and each move
                    // reliably triggers a repaint (avoids resize/drag artifacts).
                    return Some(canvas::Action::request_redraw().and_capture());
                }
                // Hover: an availability block (delete ×) takes priority over an
                // event block (tooltip) when they overlap.
                let avail = pos.and_then(|p| self.avail_at(&l, p));
                let event = match (avail, pos) {
                    (None, Some(p)) => self.event_at(&l, p),
                    _ => None,
                };
                if avail != state.hover || event != state.hover_event {
                    state.hover = avail;
                    state.hover_event = event;
                    return Some(canvas::Action::request_redraw());
                }
                None
            }
            canvas::Event::Mouse(mouse::Event::CursorLeft) => {
                if state.hover.is_some() || state.hover_event.is_some() {
                    state.hover = None;
                    state.hover_event = None;
                    return Some(canvas::Action::request_redraw());
                }
                None
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                match state.active.take()? {
                    Active::Create {
                        day,
                        anchor_min,
                        current_min,
                        min_bound,
                        max_bound,
                    } => {
                        let lo = anchor_min.min(current_min);
                        // Keep the slot within the free gap even after the minimum-
                        // length bump, so it can't spill into a neighbor.
                        let hi = anchor_min
                            .max(current_min)
                            .max(lo + SNAP_MIN)
                            .min(max_bound);
                        let date = self.days.get(day).copied()?;
                        if hi - lo < SNAP_MIN || lo < min_bound {
                            return None;
                        }
                        Some(
                            canvas::Action::publish(Msg::AddAvailability(
                                local_dt(date, lo),
                                local_dt(date, hi),
                            ))
                            .and_capture(),
                        )
                    }
                    Active::Resize {
                        index,
                        day,
                        fixed_min,
                        moving_min,
                    } => {
                        let lo = fixed_min.min(moving_min);
                        let hi = fixed_min.max(moving_min).max(lo + SNAP_MIN);
                        let date = self.days.get(day).copied()?;
                        Some(
                            canvas::Action::publish(Msg::ResizeAvailability(
                                index,
                                local_dt(date, lo),
                                local_dt(date, hi),
                            ))
                            .and_capture(),
                        )
                    }
                }
            }
            _ => None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn draw(
        &self,
        state: &GridState,
        renderer: &cosmic::Renderer,
        theme: &cosmic::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let Some(l) = self.layout(bounds, state.scroll) else {
            return vec![frame.into_geometry()];
        };

        let cosmic = theme.cosmic();
        let text_color: Color = cosmic.on_bg_color().into();
        let mut muted = text_color;
        muted.a = 0.55;
        let line: Color = cosmic.bg_divider().into();
        let accent: Color = cosmic.accent_color().into();
        let neutral: Color = cosmic.bg_component_color().into();
        // Honor the user's theme corner radius for blocks.
        let radius = cosmic::iced::border::Radius::from(cosmic.corner_radii.radius_xs);
        // Subtle drop shadow derived from the theme's `shade` overlay. Canvas
        // fills can't blur, so this is an offset solid; it disappears entirely
        // when the active theme makes `shade` transparent.
        let shade: Color = cosmic.shade_color().into();
        let vis_bot = l.visible_bottom();

        // Paint an opaque background across the whole canvas every frame. Without
        // this, iced's per-primitive damage tracking can leave residue (e.g. the
        // drop shadow's dark pixels) where a block used to be when it shrinks
        // during a resize.
        frame.fill(
            &Path::rectangle(Point::new(0.0, 0.0), bounds.size()),
            neutral,
        );

        // Hour gridlines + gutter labels (only those inside the visible band).
        for hour in self.day_start_hour..=self.day_end_hour {
            let y = Self::y_of(&l, hour as i32 * 60);
            if y < l.gy - 0.5 || y > vis_bot + 0.5 {
                continue;
            }
            frame.stroke(
                &Path::line(Point::new(l.gx, y), Point::new(l.gx + l.gw, y)),
                Stroke::default().with_color(line).with_width(1.0),
            );
            frame.fill_text(Text {
                content: crate::locale::hour_axis_label(hour),
                position: Point::new(6.0, y - 6.0),
                color: muted,
                size: 11.0.into(),
                max_width: GUTTER_W - 10.0,
                ..Text::default()
            });
        }

        // Vertical day separators across the visible band.
        for i in 0..=l.cols {
            let x = Self::col_x(&l, i);
            frame.stroke(
                &Path::line(Point::new(x, l.gy), Point::new(x, vis_bot)),
                Stroke::default().with_color(line).with_width(1.0),
            );
        }

        // Calendar events as faded colored blocks.
        for ev in self.events {
            let Some((col, lo, hi)) = self.span_minutes(&l, ev.start, ev.end) else {
                continue;
            };
            let Some(rect) = Self::rect_in_band(&l, col, lo, hi) else {
                continue;
            };
            let base = ev
                .color
                .as_deref()
                .and_then(parse_hex_color)
                .unwrap_or(neutral);
            let path = Path::rounded_rectangle(
                Point::new(rect.x, rect.y),
                Size::new(rect.width, rect.height),
                radius,
            );
            draw_block_shadow(&mut frame, rect, radius, shade);
            let mut fill = base;
            fill.a = 0.24;
            frame.fill(&path, fill);
            let mut border = base;
            border.a = 0.75;
            frame.stroke(&path, Stroke::default().with_color(border).with_width(1.0));
            // Skip the label when the block's real top is scrolled out of view.
            // Wrap to as many lines as fit the block's height, ellipsizing only
            // when it overflows; the full title is in the hover tooltip.
            if rect.height > 13.0 && Self::y_of(&l, lo) >= l.gy - 0.5 {
                let mut tcol = text_color;
                tcol.a = 0.85;
                let inner_w = (rect.width - 10.0).max(4.0);
                frame.fill_text(Text {
                    content: fit_text(&ev.title, inner_w, (rect.height - 3.0).max(1.0), 10.0),
                    position: Point::new(rect.x + 6.0, rect.y + 1.0),
                    color: tcol,
                    size: 10.0.into(),
                    max_width: inner_w,
                    ..Text::default()
                });
            }
        }

        // Availability spans (accent blocks), previewing an in-progress resize.
        for (i, span) in self.availability.iter().enumerate() {
            let resizing = matches!(state.active, Some(Active::Resize { index, .. }) if index == i);
            let placed = if let Some(Active::Resize {
                index,
                day,
                fixed_min,
                moving_min,
            }) = state.active
                && index == i
            {
                let lo = fixed_min.min(moving_min);
                let hi = fixed_min.max(moving_min).max(lo + SNAP_MIN);
                Self::rect_in_band(&l, day, lo, hi).map(|r| (r, lo))
            } else {
                self.span_minutes(&l, span.0, span.1)
                    .and_then(|(c, lo, hi)| Self::rect_in_band(&l, c, lo, hi).map(|r| (r, lo)))
            };
            let Some((rect, lo)) = placed else {
                continue;
            };
            let path = Path::rounded_rectangle(
                Point::new(rect.x, rect.y),
                Size::new(rect.width, rect.height),
                radius,
            );
            draw_block_shadow(&mut frame, rect, radius, shade);
            let mut fill = accent;
            fill.a = 0.38;
            frame.fill(&path, fill);
            frame.stroke(&path, Stroke::default().with_color(accent).with_width(1.5));
            if rect.height > 12.0 && Self::y_of(&l, lo) >= l.gy - 0.5 {
                frame.fill_text(Text {
                    content: crate::locale::format_time(&span.0),
                    position: Point::new(rect.x + 5.0, rect.y + 1.0),
                    color: text_color,
                    size: 10.0.into(),
                    max_width: (rect.width - 8.0).max(4.0),
                    ..Text::default()
                });
            }
            if state.hover == Some(i) && !resizing {
                let dx = rect.x + rect.width - DELETE_SIZE;
                frame.fill(
                    &Path::rounded_rectangle(
                        Point::new(dx, rect.y),
                        Size::new(DELETE_SIZE, DELETE_SIZE),
                        radius,
                    ),
                    accent,
                );
                frame.fill_text(Text {
                    content: "×".to_string(),
                    position: Point::new(dx + 4.0, rect.y - 1.0),
                    color: Color::WHITE,
                    size: 14.0.into(),
                    ..Text::default()
                });
            }
        }

        // In-progress create preview.
        if let Some(Active::Create {
            day,
            anchor_min,
            current_min,
            max_bound,
            ..
        }) = state.active
        {
            let lo = anchor_min.min(current_min);
            let hi = anchor_min
                .max(current_min)
                .max(lo + SNAP_MIN)
                .min(max_bound);
            if let Some(rect) = Self::rect_in_band(&l, day, lo, hi) {
                let path = Path::rounded_rectangle(
                    Point::new(rect.x, rect.y),
                    Size::new(rect.width, rect.height),
                    radius,
                );
                let mut fill = accent;
                fill.a = 0.30;
                frame.fill(&path, fill);
                frame.stroke(&path, Stroke::default().with_color(accent).with_width(1.5));
            }
        }

        // Day headers, drawn last so they sit above the grid. The header strip is
        // above `gy` and all grid content is clipped to `>= gy`, so nothing
        // bleeds underneath as it scrolls.
        frame.stroke(
            &Path::line(Point::new(0.0, l.gy), Point::new(bounds.width, l.gy)),
            Stroke::default().with_color(line).with_width(1.0),
        );
        for (i, day) in self.days.iter().enumerate() {
            frame.fill_text(Text {
                content: crate::locale::day_header(*day),
                position: Point::new(Self::col_x(&l, i) + 5.0, 7.0),
                color: text_color,
                size: 12.0.into(),
                max_width: (l.col_w - 8.0).max(4.0),
                ..Text::default()
            });
        }

        // Scrollbar thumb when the hours overflow the visible band.
        if l.max_scroll() > 0.0 {
            let thumb_h = (l.band_h * (l.band_h / l.content_h)).max(24.0);
            let thumb_y = l.gy + (l.scroll / l.max_scroll()) * (l.band_h - thumb_h);
            let mut thumb = muted;
            thumb.a = 0.45;
            frame.fill(
                &Path::rounded_rectangle(
                    Point::new(bounds.width - 5.0, thumb_y),
                    Size::new(3.0, thumb_h),
                    cosmic::iced::border::Radius::from(1.5),
                ),
                thumb,
            );
        }

        // Hovered-event tooltip with the full (untruncated) title, drawn on top
        // and wrapped so long titles stay within the window.
        if let Some(ev) = state.hover_event.and_then(|i| self.events.get(i))
            && let Some((col, lo, hi)) = self.span_minutes(&l, ev.start, ev.end)
            && let Some(rect) = Self::rect_in_band(&l, col, lo, hi)
        {
            let size = 11.0_f32;
            let pad = 6.0_f32;
            let char_w = size * 0.55;
            let line_h = size * 1.35;
            let n = ev.title.chars().count();
            let tw = (n as f32 * char_w + pad * 2.0)
                .min(bounds.width - 8.0)
                .max(60.0);
            let inner = (tw - pad * 2.0).max(1.0);
            let cols = (inner / char_w).floor().max(1.0) as usize;
            let lines = n.div_ceil(cols).max(1);
            let th = lines as f32 * line_h + pad;

            let mut tx = rect.x;
            if tx + tw > bounds.width - 4.0 {
                tx = bounds.width - 4.0 - tw;
            }
            tx = tx.max(4.0);
            // Prefer above the block; flip below if there isn't room.
            let mut ty = rect.y - th - 3.0;
            if ty < l.gy + 1.0 {
                ty = (rect.y + rect.height + 3.0).min((vis_bot - th - 1.0).max(l.gy + 1.0));
            }

            let tip_rect = Rectangle {
                x: tx,
                y: ty,
                width: tw,
                height: th,
            };
            draw_block_shadow(&mut frame, tip_rect, radius, shade);
            let tip = Path::rounded_rectangle(Point::new(tx, ty), Size::new(tw, th), radius);
            let tip_bg: Color = cosmic.bg_color().into();
            frame.fill(&tip, tip_bg);
            frame.stroke(&tip, Stroke::default().with_color(line).with_width(1.0));
            frame.fill_text(Text {
                content: ev.title.clone(),
                position: Point::new(tx + pad, ty + pad * 0.5),
                color: text_color,
                size: size.into(),
                max_width: inner,
                ..Text::default()
            });
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &GridState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if let Some(active) = state.active {
            return match active {
                Active::Create { .. } => mouse::Interaction::Crosshair,
                Active::Resize { .. } => mouse::Interaction::ResizingVertically,
            };
        }
        if let Some(l) = self.layout(bounds, state.scroll)
            && let Some(p) = cursor.position_in(bounds)
        {
            if self.edge_at(&l, p).is_some() {
                return mouse::Interaction::ResizingVertically;
            }
            if self.avail_at(&l, p).is_some() {
                return mouse::Interaction::Pointer;
            }
            if p.x >= l.gx && p.y >= l.gy {
                return mouse::Interaction::Crosshair;
            }
        }
        mouse::Interaction::default()
    }
}

/// Fit `text` into a `width` × `height` box at `size` px: wrap to as many lines
/// as fit (the caller sets `max_width` so the canvas word-wraps), ellipsizing
/// only when the text would overflow the available lines. Estimates are
/// intentionally conservative; the exact title is always available on hover.
fn fit_text(text: &str, width: f32, height: f32, size: f32) -> String {
    let cols = (width / (size * 0.55)).floor().max(1.0) as usize;
    let lines = (height / (size * 1.3)).floor().max(1.0) as usize;
    let budget = cols.saturating_mul(lines).max(1);
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let mut s: String = text.chars().take(budget.saturating_sub(1).max(1)).collect();
    s.push('…');
    s
}

/// Draw a subtle theme-driven drop shadow behind a block. No-op when the theme's
/// `shade` is transparent.
fn draw_block_shadow(
    frame: &mut Frame,
    rect: Rectangle,
    radius: cosmic::iced::border::Radius,
    shade: Color,
) {
    if shade.a <= 0.0 {
        return;
    }
    frame.fill(
        &Path::rounded_rectangle(
            Point::new(rect.x + 0.5, rect.y + 1.5),
            Size::new(rect.width, rect.height),
            radius,
        ),
        shade,
    );
}

/// Build a `DateTime<Local>` from a date and an absolute minute-of-day, handling
/// the end-of-day (1440 → next midnight) case.
fn local_dt(date: NaiveDate, minute_of_day: i32) -> DateTime<Local> {
    let mins = minute_of_day.clamp(0, 1440);
    let (date, mins) = if mins == 1440 {
        (date.succ_opt().unwrap_or(date), 0)
    } else {
        (date, mins)
    };
    date.and_hms_opt((mins / 60) as u32, (mins % 60) as u32, 0)
        .and_then(|naive| naive.and_local_timezone(Local).earliest())
        .unwrap_or_else(Local::now)
}

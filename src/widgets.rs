// SPDX-License-Identifier: GPL-3.0-only
//
// Reusable widget helpers and styles for the applet UI.

use crate::calendar::CalendarInfo;
use crate::fl;
use crate::formatting::parse_hex_color;
use cosmic::cosmic_theme;
use cosmic::iced::Length;
use cosmic::iced::core::id;
use cosmic::prelude::*;
use cosmic::widget;

/// Get display format labels for the dropdown (must be called at runtime for localization)
pub fn display_format_options() -> Vec<String> {
    vec![
        fl!("display-format-day-time"),
        fl!("display-format-relative"),
    ]
}

/// Get theme spacing values
pub fn spacing() -> cosmic_theme::Spacing {
    cosmic::theme::spacing()
}

/// Generate a unique ID for an email input field
pub fn email_input_id(idx: usize) -> id::Id {
    id::Id::new(format!("email_input_{idx}"))
}

/// Secondary text style for dimmed/muted text appearance
pub fn secondary_text_style(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    cosmic::iced::widget::text::Style {
        color: Some(theme.cosmic().palette.neutral_6.into()),
        ..Default::default()
    }
}

/// Creates a settings page header with back button and title
pub fn settings_page_header<'a, M: Clone + 'static>(
    back_label: String,
    title: String,
    back_message: M,
) -> Element<'a, M> {
    let space = spacing();
    widget::column::with_capacity(2)
        .push(
            widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                .extra_small()
                .padding(space.space_none)
                .label(back_label)
                .spacing(space.space_xxxs)
                .class(cosmic::theme::Button::Link)
                .on_press(back_message),
        )
        .push(widget::text::title4(title))
        .spacing(space.space_xxs)
        .into()
}

/// Creates a navigation row with label, summary and chevron (no hover, normal colors)
pub fn settings_nav_row<'a, M: Clone + 'static>(
    label: String,
    summary: String,
    nav_message: M,
) -> Element<'a, M> {
    let space = spacing();
    widget::button::custom(
        widget::container(
            widget::row::with_capacity(4)
                .push(widget::text::body(label))
                .push(widget::space::horizontal())
                .push(widget::text::body(summary))
                .push(widget::icon::from_name("go-next-symbolic").size(16).icon())
                .spacing(space.space_s)
                .align_y(cosmic::iced::Alignment::Center)
                .width(Length::Fill),
        )
        .class(cosmic::theme::Container::List),
    )
    .padding(0)
    .class(cosmic::theme::Button::Transparent)
    .width(Length::Fill)
    .on_press(nav_message)
    .into()
}

/// Creates a navigation row with icon, label, summary, and chevron (normal colors)
pub fn settings_nav_row_with_icon<'a, M: Clone + 'static>(
    icon_name: &'static str,
    label: String,
    summary: String,
    nav_message: M,
) -> Element<'a, M> {
    let space = spacing();
    widget::button::custom(
        widget::container(
            widget::row::with_capacity(5)
                .push(
                    widget::icon::from_name(icon_name)
                        .size(space.space_s)
                        .symbolic(true),
                )
                .push(widget::text::body(label))
                .push(widget::space::horizontal())
                .push(widget::text::body(summary))
                .push(widget::icon::from_name("go-next-symbolic").size(16).icon())
                .spacing(space.space_s)
                .align_y(cosmic::iced::Alignment::Center)
                .width(Length::Fill),
        )
        .class(cosmic::theme::Container::List),
    )
    .padding(0)
    .class(cosmic::theme::Button::Transparent)
    .width(Length::Fill)
    .on_press(nav_message)
    .into()
}

/// Create a calendar color indicator dot widget with optional tooltip showing calendar name
pub fn calendar_color_dot<'a, M: 'a>(
    calendar_uid: &str,
    calendars: &[CalendarInfo],
    size: f32,
    tooltip_position: Option<widget::tooltip::Position>,
) -> Option<Element<'a, M>> {
    // Find the calendar by UID and get its color
    let calendar = calendars.iter().find(|c| c.uid == calendar_uid)?;
    let color_hex = calendar.color.as_ref()?;
    let color = parse_hex_color(color_hex)?;
    let calendar_name = calendar.display_name.clone();

    let dot = widget::container(widget::Space::new())
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .class(cosmic::theme::Container::custom(move |_theme| {
            cosmic::iced::widget::container::Style {
                background: Some(cosmic::iced::Background::Color(color)),
                border: cosmic::iced::Border {
                    radius: (size / 2.0).into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        }));

    Some(match tooltip_position {
        Some(pos) => widget::tooltip(dot, widget::text(calendar_name), pos).into(),
        None => dot.into(),
    })
}

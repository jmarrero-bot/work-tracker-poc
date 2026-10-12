//! The pieces the views are built from: chips, badges and rows.

use crate::{
    api::{Outcome, Zone},
    model::{self, Tone},
};
use adw::prelude::*;
use gtk::glib;

/// The action that opens a link in the browser.
pub const OPEN_URI: &str = "app.open-uri";
/// The icon of a row that opens a web page.
const EXTERNAL_LINK: &str = "adw-external-link-symbolic";

/// A small rounded label, in one of the style's colors: `accent`,
/// `success`, `warning`, `error` or `hub`, or none for a neutral one.
pub fn chip(text: &str, tone: Option<&str>, icon: Option<&str>) -> gtk::Widget {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Start)
        .css_classes(["chip"])
        .build();
    if let Some(tone) = tone {
        content.add_css_class(tone);
    }
    if let Some(icon) = icon {
        content.append(&gtk::Image::from_icon_name(icon));
    }
    content.append(&gtk::Label::builder().label(text).build());
    content.upcast()
}

/// The chip of an outcome other than open, with the reason it was chosen.
pub fn outcome_chip(outcome: Outcome, reason: &str) -> Option<gtk::Widget> {
    let tone = Tone::of(outcome);
    if tone == Tone::Open {
        return None;
    }
    let class = match tone {
        Tone::Abandoned => None,
        other => Some(other.css_class()),
    };
    let chip = chip(model::outcome_label(outcome), class, None);
    if !reason.is_empty() {
        chip.set_tooltip_text(Some(reason));
    }
    Some(chip)
}

/// Marks what came from the hub: read-only here.
pub fn hub_chip() -> gtk::Widget {
    let chip = chip(
        "From the hub, read-only",
        Some("hub"),
        Some("system-lock-screen-symbolic"),
    );
    chip.set_tooltip_text(Some(
        "Synced by the hub and pulled from it. Change it on the hub.",
    ));
    chip
}

/// "Stale", with how stale in its tooltip.
pub fn stale_chip(text: &str) -> gtk::Widget {
    let chip = chip(
        "Stale",
        Some("warning"),
        Some("document-open-recent-symbolic"),
    );
    chip.set_tooltip_text(Some(text));
    chip
}

pub fn zone_chip(zone: Zone) -> gtk::Widget {
    match zone {
        Zone::Internal => {
            let chip = chip("Internal", None, None);
            chip.set_tooltip_text(Some("Internal work never leaves this machine"));
            chip
        }
        Zone::External => chip("External", None, None),
    }
}

/// A warning sign, for a row whose thing needs attention.
pub fn warning_icon() -> gtk::Widget {
    gtk::Image::builder()
        .icon_name("dialog-warning-symbolic")
        .css_classes(["warning"])
        .build()
        .upcast()
}

/// A source's two-letter badge.
pub fn badge(provider: &str) -> gtk::Widget {
    let (name, short) = model::provider(provider);
    let badge = gtk::Label::builder()
        .label(short)
        .valign(gtk::Align::Center)
        .css_classes(["badge"])
        .tooltip_text(name)
        .build();
    badge.add_css_class(provider);
    badge.upcast()
}

/// The icon of an outcome, colored by its tone, with its label and reason
/// as the tooltip.
pub fn outcome_icon(outcome: Outcome, reason: &str) -> gtk::Widget {
    let tone = Tone::of(outcome);
    // Without an icon, for open work, it keeps its width, so titles line up.
    let icon = gtk::Image::builder()
        .valign(gtk::Align::Start)
        .margin_top(4)
        .css_classes(["outcome", tone.css_class()])
        .build();
    if let Some(name) = tone.icon_name() {
        icon.set_icon_name(Some(name));
    }
    let label = model::outcome_label(outcome);
    let tooltip = if reason.is_empty() {
        label.to_string()
    } else {
        format!("{label}: {reason}")
    };
    icon.set_tooltip_text(Some(&tooltip));
    icon.update_property(&[gtk::accessible::Property::Label(label)]);
    icon.upcast()
}

/// What an item row shows.
pub struct ItemRow<'a> {
    pub title: &'a str,
    pub provider: &'a str,
    /// The meta line: where it is from, its key, its source's status.
    pub meta: String,
    pub outcome: Outcome,
    pub outcome_reason: &'a str,
    pub url: &'a str,
    pub chips: Vec<gtk::Widget>,
    /// A line more about it, such as why the bot waits on the person.
    pub note: Option<&'a str>,
    /// Buttons before its link.
    pub actions: Vec<gtk::Widget>,
    /// Whether its list has a column for outcome icons: only if one of
    /// its rows has an icon, so a list of open work doesn't spend the width.
    pub gutter: bool,
}

/// A row for one item: its outcome, title, source and native status, and
/// chips, opening its source in the browser when activated.
pub fn item_row(row: ItemRow) -> gtk::ListBoxRow {
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .build();
    text.append(
        &gtk::Label::builder()
            .label(row.title)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .css_classes(["item-title"])
            .build(),
    );
    text.append(
        &gtk::Label::builder()
            .label(&row.meta)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    if let Some(note) = row.note.filter(|n| !n.is_empty()) {
        text.append(
            &gtk::Label::builder()
                .label(note)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .css_classes(["caption"])
                .build(),
        );
    }
    if !row.chips.is_empty() {
        let chips = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(false)
            .column_spacing(6)
            .row_spacing(6)
            .max_children_per_line(12)
            .halign(gtk::Align::Start)
            .can_focus(false)
            .margin_top(2)
            .build();
        for chip in &row.chips {
            // A long chip shortens rather than widen the row past a phone's
            // width.
            if let Some(label) = chip.last_child().and_downcast::<gtk::Label>() {
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            }
            chips.append(chip);
        }
        let mut child = chips.first_child();
        while let Some(c) = child {
            c.set_can_focus(false);
            child = c.next_sibling();
        }
        text.append(&chips);
    }
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    if row.gutter || Tone::of(row.outcome) != Tone::Open {
        content.append(&outcome_icon(row.outcome, row.outcome_reason));
    }
    let badge = badge(row.provider);
    badge.set_valign(gtk::Align::Start);
    content.append(&badge);
    content.append(&text);
    for action in &row.actions {
        action.set_valign(gtk::Align::Center);
        content.append(action);
    }
    let link = gtk::Image::builder()
        .icon_name(EXTERNAL_LINK)
        .valign(gtk::Align::Center)
        .css_classes(["dim-label"])
        .build();
    content.append(&link);
    let list_row = gtk::ListBoxRow::builder()
        .child(&content)
        .activatable(true)
        .build();
    list_row.set_action_name(Some(OPEN_URI));
    list_row.set_action_target_value(Some(&row.url.to_variant()));
    list_row
}

/// A boxed list, as in a preferences group, for item rows.
pub fn boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

/// A group of rows under a heading.
pub fn group(title: &str, description: Option<&str>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(glib::markup_escape_text(title))
        .build();
    if let Some(description) = description {
        group.set_description(Some(&glib::markup_escape_text(description)));
    }
    group
}

/// A button that opens `url` in the browser.
pub fn link_button(icon: &str, tooltip: &str, url: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    button.set_action_name(Some(OPEN_URI));
    button.set_action_target_value(Some(&url.to_variant()));
    button
}

/// An action row that doesn't take markup: titles come from sources.
pub fn action_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .use_markup(false)
        .subtitle_lines(0)
        .build()
}

/// An action row that opens `url` when activated, with `suffixes` before
/// the icon that says so.
pub fn link_row(
    title: &str,
    subtitle: &str,
    url: &str,
    suffixes: &[gtk::Widget],
) -> adw::ActionRow {
    let row = action_row(title, subtitle);
    row.set_activatable(true);
    row.set_action_name(Some(OPEN_URI));
    row.set_action_target_value(Some(&url.to_variant()));
    for suffix in suffixes {
        row.add_suffix(suffix);
    }
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name(EXTERNAL_LINK)
            .css_classes(["dim-label"])
            .build(),
    );
    row.set_tooltip_text(Some(url));
    row
}

/// What a view shows: groups of rows, or a status page when there is
/// nothing to list.
pub enum Content {
    Page(Vec<gtk::Widget>),
    Empty(gtk::Widget),
}

/// A page of groups, shown in a scrolling column clamped to a readable
/// width, which stays as the view is rebuilt, and so does its scroll
/// position.
pub fn page(children: &[gtk::Widget]) -> Content {
    Content::Page(children.to_vec())
}

/// A page with nothing to show, and why. Its title is plain text, and its
/// description markup.
pub fn empty(icon: &str, title: &str, description: &str) -> Content {
    Content::Empty(
        adw::StatusPage::builder()
            .icon_name(icon)
            .title(title)
            .description(glib::markup_escape_text(description))
            .vexpand(true)
            .build()
            .upcast(),
    )
}

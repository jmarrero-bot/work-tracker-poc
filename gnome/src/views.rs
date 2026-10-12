//! The views, each built from the board the service last answered, as of a
//! moment. A view is rebuilt only when what it shows changed; the times it
//! tells ("3 min ago") are updated in place, so keyboard focus and a
//! screen reader's place stay. Buttons only activate the app's actions,
//! which call the service; nothing here changes anything itself.

use crate::{
    api::{Board, BotView, Connection, HubStatus, Item, MineRow, Mode},
    model,
    widgets::{self, Content, ItemRow},
};
use adw::prelude::*;
use gtk::glib;
use std::{cell::RefCell, collections::HashSet, rc::Rc};

/// Copies a comment's start and opens where it goes: `(text, url)`.
pub const COMMENT: &str = "app.comment";
/// Checks a connection's credential again, by its ID.
pub const VERIFY: &str = "app.verify";
/// Syncs one connection now, by its ID.
pub const SYNC_ONE: &str = "app.sync-connection";

/// Which efforts are expanded. They start expanded, so it keeps the IDs
/// of those the person collapsed.
#[derive(Debug, Clone, Default)]
pub struct Expanded(Rc<RefCell<HashSet<String>>>);

impl Expanded {
    fn contains(&self, id: &str) -> bool {
        !self.0.borrow().contains(id)
    }
    fn set(&self, id: &str, expanded: bool) {
        let mut collapsed = self.0.borrow_mut();
        if expanded {
            collapsed.remove(id);
        } else {
            collapsed.insert(id.to_string());
        }
    }
}

/// A text that tells time, worked out again from the latest board.
type Timed = Box<dyn Fn(&Board, i64)>;

/// The texts of a view that tell time, which [`Ticker::tick`] updates in
/// place from the latest board, without rebuilding the view.
#[derive(Clone, Default)]
pub struct Ticker(Rc<RefCell<Vec<Timed>>>);

impl Ticker {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }
    pub fn tick(&self, board: &Board, now: i64) {
        for update in self.0.borrow().iter() {
            update(board, now);
        }
    }
    /// Keep `row`'s subtitle as `text` says, for the board and the time.
    fn subtitle(
        &self,
        row: &adw::ActionRow,
        board: &Board,
        now: i64,
        text: impl Fn(&Board, i64) -> Option<String> + 'static,
    ) {
        if let Some(t) = text(board, now) {
            row.set_subtitle(&t);
        }
        let row = row.downgrade();
        self.0.borrow_mut().push(Box::new(move |board, now| {
            if let (Some(row), Some(t)) = (row.upgrade(), text(board, now)) {
                row.set_subtitle(&t);
            }
        }));
    }
    /// Keep `widget`'s tooltip as `text` says.
    fn tooltip(
        &self,
        widget: &gtk::Widget,
        board: &Board,
        now: i64,
        text: impl Fn(&Board, i64) -> Option<String> + 'static,
    ) {
        widget.set_tooltip_text(text(board, now).as_deref());
        let widget = widget.downgrade();
        self.0.borrow_mut().push(Box::new(move |board, now| {
            if let (Some(widget), Some(t)) = (widget.upgrade(), text(board, now)) {
                widget.set_tooltip_text(Some(&t));
            }
        }));
    }
}

/// What building a view takes besides the board.
pub struct Ctx<'a> {
    pub now: i64,
    pub ticker: &'a Ticker,
    pub expanded: &'a Expanded,
}

/// The app's views, in the sidebar's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum View {
    Mine,
    Next,
    Inbox,
    Efforts,
    Connections,
    Hub,
    Bot,
}

impl View {
    pub const ALL: [View; 7] = [
        View::Mine,
        View::Next,
        View::Inbox,
        View::Efforts,
        View::Connections,
        View::Hub,
        View::Bot,
    ];
    /// Its name in actions, such as `app.show-view::mine`.
    pub fn id(self) -> &'static str {
        match self {
            View::Mine => "mine",
            View::Next => "next",
            View::Inbox => "inbox",
            View::Efforts => "efforts",
            View::Connections => "connections",
            View::Hub => "hub",
            View::Bot => "bot",
        }
    }
    pub fn of(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.id() == id)
    }
    pub fn title(self) -> &'static str {
        match self {
            View::Mine => "Mine",
            View::Next => "Next",
            View::Inbox => "Inbox",
            View::Efforts => "Efforts",
            View::Connections => "Connections",
            View::Hub => "Hub",
            View::Bot => "Bot",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            View::Mine => "avatar-default-symbolic",
            View::Next => "task-due-symbolic",
            View::Inbox => "view-list-bullet-symbolic",
            View::Efforts => "checkbox-checked-symbolic",
            View::Connections => "network-transmit-receive-symbolic",
            View::Hub => "network-server-symbolic",
            View::Bot => "system-run-symbolic",
        }
    }
    /// What the sidebar shows next to it: a count, and whether it needs
    /// attention.
    pub fn badge(self, board: &Board) -> Option<(String, bool)> {
        let s = &board.snapshot;
        let count = |n: usize, attention: bool| (n > 0).then(|| (n.to_string(), attention));
        match self {
            View::Mine => count(board.mine.mine.len(), false),
            View::Next => count(s.items.iter().filter(|i| model::is_next(i)).count(), false),
            View::Inbox => count(
                s.items
                    .iter()
                    .filter(|i| !model::settled(i.outcome))
                    .count(),
                false,
            ),
            View::Efforts => None,
            View::Connections => count(
                s.connections
                    .iter()
                    .filter(|c| !c.from_hub && (c.schedule.needs_user || c.sync.error.is_some()))
                    .count(),
                true,
            ),
            View::Hub => s
                .hub
                .as_ref()
                .filter(|h| h.schedule.stale || h.schedule.needs_user)
                .map(|_| ("!".into(), true)),
            View::Bot => count(model::bot_waiting(s).len(), true),
        }
    }
    /// What it shows of `shape`, a board without its times
    /// ([`model::structural`]): when this changes, it is rebuilt.
    pub fn shape(self, shape: &Board) -> String {
        let s = &shape.snapshot;
        match self {
            View::Mine => format!("{:?}{:?}", shape.mine, s.connections),
            View::Next | View::Inbox => format!("{:?}{:?}{:?}", s.items, s.efforts, s.connections),
            View::Efforts => format!("{:?}{:?}{:?}", s.efforts, s.items, s.connections),
            View::Connections => format!("{:?}", s.connections),
            View::Hub => format!("{:?}{:?}{:?}", s.mode, s.hub, s.connections),
            View::Bot => format!("{:?}{:?}{:?}", shape.bots, s.items, s.connections),
        }
    }
    /// Builds it from `board`.
    pub fn build(self, board: &Board, ctx: &Ctx) -> Content {
        match self {
            View::Mine => mine(board, ctx),
            View::Next => next(board, ctx),
            View::Inbox => inbox(board, ctx),
            View::Efforts => efforts(board, ctx),
            View::Connections => connections(board, ctx),
            View::Hub => hub(board, ctx),
            View::Bot => bot(board, ctx),
        }
    }
}

fn find_item<'a>(board: &'a Board, id: &str) -> Option<&'a Item> {
    board.snapshot.items.iter().find(|i| i.id == id)
}

fn find_row<'a>(board: &'a Board, id: &str) -> Option<&'a MineRow> {
    let m = &board.mine;
    m.mine.iter().chain(m.watching.iter()).find(|r| r.id == id)
}

fn find_connection<'a>(board: &'a Board, id: &str) -> Option<&'a Connection> {
    board.snapshot.connections.iter().find(|c| c.id == id)
}

fn find_bot<'a>(board: &'a Board, id: &str) -> Option<&'a BotView> {
    board.bots.iter().find(|v| v.connection_id == id)
}

/// The chip of what is stale, whose tooltip says since when, kept current
/// from `freshness`: whether it is stale, and when it last synced.
fn stale_chip(
    board: &Board,
    ctx: &Ctx,
    freshness: impl Fn(&Board) -> Option<(bool, Option<String>)> + 'static,
) -> Option<gtk::Widget> {
    let (stale, synced_at) = freshness(board)?;
    let text = model::stale_text(stale, synced_at.as_deref(), ctx.now)?;
    let chip = widgets::stale_chip(&text);
    ctx.ticker
        .tooltip(&chip, board, ctx.now, move |board, now| {
            let (stale, synced_at) = freshness(board)?;
            model::stale_text(stale, synced_at.as_deref(), now)
        });
    Some(chip)
}

/// Whether a list of these outcomes needs a column for outcome icons:
/// open work has none.
fn gutter(outcomes: impl IntoIterator<Item = crate::api::Outcome>) -> bool {
    outcomes
        .into_iter()
        .any(|o| model::Tone::of(o) != model::Tone::Open)
}

fn mine_row(board: &Board, row: &MineRow, ctx: &Ctx, gutter: bool) -> gtk::ListBoxRow {
    let mut chips = vec![];
    // Each account with its own reasons, never another's.
    let several = row.waits_on.len() > 1;
    for w in &row.waits_on {
        for reason in &w.reasons {
            let label = model::reason_label(reason);
            let text = if several {
                format!("{}: {label}", w.account)
            } else {
                label
            };
            let chip = widgets::chip(&text, Some("accent"), None);
            chip.set_tooltip_text(Some(&format!("Waits on {}", w.account)));
            chips.push(chip);
        }
    }
    chips.extend(widgets::outcome_chip(row.outcome, &row.outcome_reason));
    if row.from_hub {
        chips.push(widgets::hub_chip());
    } else {
        chips.push(widgets::zone_chip(row.zone));
    }
    let id = row.id.clone();
    chips.extend(stale_chip(board, ctx, move |b| {
        find_row(b, &id).map(|r| (r.stale, r.synced_at.clone()))
    }));
    let source = row.waits_on.first().map(|w| w.connection_id.as_str());
    let meta = format!(
        "{} · {} · {}",
        model::source_label(&board.snapshot, source, &row.provider),
        row.key,
        row.status
    );
    widgets::item_row(ItemRow {
        title: &row.title,
        provider: &row.provider,
        meta,
        outcome: row.outcome,
        outcome_reason: &row.outcome_reason,
        url: &row.url,
        chips,
        note: None,
        actions: vec![],
        gutter,
    })
}

fn mine(board: &Board, ctx: &Ctx) -> Content {
    let m = &board.mine;
    if m.mine.is_empty() && m.watching.is_empty() && m.watch_errors.is_empty() {
        return widgets::empty(
            "object-select-symbolic",
            "Nothing waits on you",
            "Review requests, assignments, mentions and your changes that need action show here, from every connected account.",
        );
    }
    let mut children = vec![];
    let group = widgets::group(
        "Waiting on you",
        Some("Review requests, assignments, mentions and your changes that need action, from every account. Each shows once, with every reason."),
    );
    if m.mine.is_empty() {
        group.add(&widgets::action_row("Nothing waits on you", ""));
    } else {
        let list = widgets::boxed_list();
        let gutter = gutter(m.mine.iter().map(|r| r.outcome));
        for row in &m.mine {
            list.append(&mine_row(board, row, ctx, gutter));
        }
        group.add(&list);
    }
    children.push(group.upcast());
    if !m.watching.is_empty() || !m.watch_errors.is_empty() {
        let group = widgets::group(
            "Watching",
            Some("What watching other users found. It is theirs, never yours."),
        );
        for e in &m.watch_errors {
            let row = widgets::action_row(
                &format!("Watching {} isn't up to date", e.login),
                &format!("{}. Showing what an earlier sync found.", e.error),
            );
            row.add_prefix(&widgets::warning_icon());
            group.add(&row);
        }
        if !m.watching.is_empty() {
            let list = widgets::boxed_list();
            let gutter = gutter(m.watching.iter().map(|r| r.outcome));
            for row in &m.watching {
                list.append(&mine_row(board, row, ctx, gutter));
            }
            group.add(&list);
        }
        children.push(group.upcast());
    }
    widgets::page(&children)
}

/// What an item row has besides the item: its effort unless the row is in
/// it, a note, and buttons.
#[derive(Default)]
struct Extra<'a> {
    in_effort: bool,
    /// See [`ItemRow::gutter`].
    gutter: bool,
    note: Option<&'a str>,
    actions: Vec<gtk::Widget>,
}

fn item_row(board: &Board, item: &Item, ctx: &Ctx, extra: Extra) -> gtk::ListBoxRow {
    let snapshot = &board.snapshot;
    let mut chips = vec![];
    chips.extend(widgets::outcome_chip(item.outcome, &item.outcome_reason));
    if let Some(effort) = item
        .effort_id
        .as_deref()
        .filter(|_| !extra.in_effort)
        .and_then(|id| snapshot.efforts.iter().find(|e| e.id == id))
    {
        chips.push(widgets::chip(
            &effort.title,
            None,
            Some("checkbox-checked-symbolic"),
        ));
    }
    if item.from_hub {
        chips.push(widgets::hub_chip());
    }
    let id = item.id.clone();
    chips.extend(stale_chip(board, ctx, move |b| {
        find_item(b, &id).map(|i| (i.stale, i.synced_at.clone()))
    }));
    let meta = format!(
        "{} · {} · {}",
        model::source_label(snapshot, item.connection_id.as_deref(), &item.provider),
        item.key,
        item.status
    );
    widgets::item_row(ItemRow {
        title: &item.title,
        provider: &item.provider,
        meta,
        outcome: item.outcome,
        outcome_reason: &item.outcome_reason,
        url: &item.url,
        chips,
        note: extra.note,
        actions: extra.actions,
        gutter: extra.gutter,
    })
}

fn next(board: &Board, ctx: &Ctx) -> Content {
    let mut items: Vec<&Item> = board
        .snapshot
        .items
        .iter()
        .filter(|i| model::is_next(i))
        .collect();
    if items.is_empty() {
        return widgets::empty(
            "object-select-symbolic",
            "No next actions",
            "Open work with a next action or a review, and closed work that needs your decision, shows here.",
        );
    }
    // Decisions first, then the most recently changed.
    items.sort_by(|a, b| {
        model::needs_decision(b.outcome)
            .cmp(&model::needs_decision(a.outcome))
            .then(b.updated_at.cmp(&a.updated_at))
    });
    let group = widgets::group(
        "Next actions",
        Some("Next actions and reviews, and closed work that needs your decision: closed is not delivered."),
    );
    let list = widgets::boxed_list();
    let gutter = gutter(items.iter().map(|i| i.outcome));
    for item in items {
        let extra = Extra {
            gutter,
            ..Extra::default()
        };
        list.append(&item_row(board, item, ctx, extra));
    }
    group.add(&list);
    widgets::page(&[group.upcast()])
}

fn inbox(board: &Board, ctx: &Ctx) -> Content {
    let groups = model::inbox_groups(&board.snapshot.items);
    if groups.is_empty() {
        return widgets::empty(
            "mail-read-symbolic",
            "The inbox is empty",
            "Connect an account, or add a source link with the CLI or the web page.",
        );
    }
    let children: Vec<gtk::Widget> = groups
        .into_iter()
        .map(|(name, items)| {
            let group = widgets::group(&format!("{name} ({})", items.len()), None);
            let list = widgets::boxed_list();
            let gutter = gutter(items.iter().map(|i| i.outcome));
            for item in items {
                let extra = Extra {
                    gutter,
                    ..Extra::default()
                };
                list.append(&item_row(board, item, ctx, extra));
            }
            group.add(&list);
            group.upcast()
        })
        .collect();
    widgets::page(&children)
}

fn efforts(board: &Board, ctx: &Ctx) -> Content {
    let snapshot = &board.snapshot;
    if snapshot.efforts.is_empty() {
        return widgets::empty(
            "checkbox-checked-symbolic",
            "No efforts yet",
            "Group related tickets, changes and advisories into an effort in the web page or with `workboard effort`.",
        );
    }
    let group = widgets::group(
        "Efforts",
        Some("Progress counts only delivered work. Closed without delivery and unknown work needs attention instead."),
    );
    for effort in &snapshot.efforts {
        let p = &effort.progress;
        let row = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(&effort.title))
            .subtitle(glib::markup_escape_text(&model::progress_text(p)))
            .expanded(ctx.expanded.contains(&effort.id))
            .build();
        let id = effort.id.clone();
        let memory = ctx.expanded.clone();
        row.connect_expanded_notify(move |row| memory.set(&id, row.is_expanded()));
        let progress = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        progress.append(
            &gtk::Label::builder()
                .label(format!("{}/{} delivered", p.delivered, p.total))
                .xalign(1.0)
                .css_classes(["caption", "numeric"])
                .build(),
        );
        progress.append(
            &gtk::ProgressBar::builder()
                .fraction(model::delivered_fraction(p))
                .css_classes(["effort-progress"])
                .build(),
        );
        if effort.from_hub {
            row.add_suffix(&widgets::hub_chip());
        }
        if p.needs_attention > 0 {
            let icon = widgets::warning_icon();
            icon.set_tooltip_text(Some("Some of its work needs your decision"));
            row.add_prefix(&icon);
        }
        row.add_suffix(&progress);
        let items: Vec<&Item> = snapshot
            .items
            .iter()
            .filter(|i| i.effort_id.as_deref() == Some(&effort.id))
            .collect();
        let gutter = gutter(items.iter().map(|i| i.outcome));
        for item in items {
            let extra = Extra {
                in_effort: true,
                gutter,
                ..Extra::default()
            };
            row.add_row(&item_row(board, item, ctx, extra));
        }
        if !effort.note.is_empty() {
            row.add_row(&widgets::action_row("Note", &effort.note));
        }
        group.add(&row);
    }
    widgets::page(&[group.upcast()])
}

/// A connection's subtitle: its source, account and zone, and how its
/// syncs go.
fn connection_subtitle(c: &Connection, now: i64) -> String {
    let (name, _) = model::provider(&c.provider);
    let zone = match c.zone {
        crate::api::Zone::Internal => "internal",
        crate::api::Zone::External => "external",
    };
    let watching = c
        .watch_user
        .as_deref()
        .map(|u| format!(" · watching {u}"))
        .unwrap_or_default();
    let (status, _) = model::sync_status(c, now);
    format!("{name} · {} · {zone}{watching}\n{status}", c.account_name)
}

fn connection_row(board: &Board, c: &Connection, ctx: &Ctx) -> adw::ActionRow {
    let (name, _) = model::provider(&c.provider);
    let title = if c.label.is_empty() {
        name
    } else {
        c.label.as_str()
    };
    let row = widgets::action_row(title, "");
    let id = c.id.clone();
    ctx.ticker.subtitle(&row, board, ctx.now, move |b, now| {
        find_connection(b, &id).map(|c| connection_subtitle(c, now))
    });
    row.add_prefix(&widgets::badge(&c.provider));
    let (_, attention) = model::sync_status(c, ctx.now);
    let verify = model::can_verify(c);
    // Its failure, with what kind it is, unless the subtitle's "Needs you"
    // and the Verify Again button say so already.
    // A suffix, so the source badges of all rows line up.
    if c.sync.error_kind.is_some() || c.schedule.needs_user {
        row.add_suffix(&widgets::warning_icon());
    }
    match (&c.sync.error_kind, verify) {
        (_, true) => (),
        (Some(kind), false) => {
            row.add_suffix(&widgets::chip(
                &model::error_kind_label(kind),
                Some("warning"),
                None,
            ));
        }
        (None, false) if attention => {
            let id = c.id.clone();
            let chip = stale_chip(board, ctx, move |b| {
                find_connection(b, &id).map(|c| (c.schedule.stale, c.sync.last_success.clone()))
            });
            if let Some(chip) = chip {
                row.add_suffix(&chip);
            }
        }
        (None, false) => (),
    }
    if c.from_hub {
        row.add_suffix(&widgets::hub_chip());
        return row;
    }
    if verify {
        let verify = gtk::Button::builder()
            .label("Verify Again")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .tooltip_text("Check the stored credential again; scheduled syncs resume if it works")
            .build();
        verify.set_action_name(Some(VERIFY));
        verify.set_action_target_value(Some(&c.id.to_variant()));
        row.add_suffix(&verify);
    } else {
        let sync = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text("Sync Now")
            .sensitive(!c.schedule.running)
            .build();
        sync.set_action_name(Some(SYNC_ONE));
        sync.set_action_target_value(Some(&c.id.to_variant()));
        row.add_suffix(&sync);
    }
    row
}

fn connections(board: &Board, ctx: &Ctx) -> Content {
    let snapshot = &board.snapshot;
    if snapshot.connections.is_empty() {
        return widgets::empty(
            "network-transmit-receive-symbolic",
            "No accounts connected",
            "Connect Jira, GitLab, GitHub, Fedora Forge or the bot in the web page, or with `workboard connect`.",
        );
    }
    let mut children = vec![];
    let (own, hub): (Vec<&Connection>, Vec<&Connection>) =
        snapshot.connections.iter().partition(|c| !c.from_hub);
    if !own.is_empty() {
        let group = widgets::group(
            "On this machine",
            Some("Each syncs on its own schedule, backs off when its source fails, and stops when its credential is rejected until you verify it again."),
        );
        for c in own {
            group.add(&connection_row(board, c, ctx));
        }
        children.push(group.upcast());
    }
    if !hub.is_empty() {
        let group = widgets::group(
            "From the hub",
            Some("Synced by the hub, as of the last pull. Read-only here; manage them on the hub."),
        );
        for c in hub {
            group.add(&connection_row(board, c, ctx));
        }
        children.push(group.upcast());
    }
    widgets::page(&children)
}

/// "1 connection", "2 connections".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn hub_rows(board: &Board, h: &HubStatus, ctx: &Ctx) -> Vec<gtk::Widget> {
    let (status, attention) = model::hub_status(h, ctx.now);
    let group = widgets::group(
        "Pulling from the hub",
        Some("The hub's external work, pulled read-only and shown next to this machine's. Nothing is ever sent to the hub."),
    );
    let chip = if h.schedule.needs_user {
        widgets::chip("Needs you", Some("error"), Some("dialog-warning-symbolic"))
    } else if attention {
        widgets::stale_chip(&status)
    } else {
        widgets::chip("Current", Some("success"), Some("object-select-symbolic"))
    };
    let row = widgets::link_row(&h.url, "", &h.url, &[chip]);
    ctx.ticker.subtitle(&row, board, ctx.now, |b, now| {
        b.snapshot.hub.as_ref().map(|h| model::hub_status(h, now).0)
    });
    row.add_prefix(&gtk::Image::from_icon_name("network-server-symbolic"));
    group.add(&row);
    group.add(&widgets::action_row(
        "Last export",
        &format!(
            "{}, {} · pulled every {}",
            count(h.connections, "connection", "connections"),
            count(h.items, "item", "items"),
            model::every(h.schedule.interval_secs)
        ),
    ));
    let mut children: Vec<gtk::Widget> = vec![group.upcast()];
    let theirs: Vec<&Connection> = board
        .snapshot
        .connections
        .iter()
        .filter(|c| c.from_hub)
        .collect();
    if !theirs.is_empty() {
        let group = widgets::group("The hub's connections", None);
        for c in theirs {
            group.add(&connection_row(board, c, ctx));
        }
        children.push(group.upcast());
    }
    children
}

fn hub(board: &Board, ctx: &Ctx) -> Content {
    let snapshot = &board.snapshot;
    if snapshot.mode == Mode::Hub {
        return widgets::empty(
            "network-server-symbolic",
            "This service is a hub",
            "It keeps external work only, and serves it to its companions. The companion app is meant for the laptop's own service.",
        );
    }
    match &snapshot.hub {
        Some(h) => widgets::page(&hub_rows(board, h, ctx)),
        None => widgets::empty(
            "network-server-symbolic",
            "Not pulling from a hub",
            "Run `workboard hub set URL --token-stdin` with the hub's companion token to show its external work here too.",
        ),
    }
}

/// The coordinator's heartbeat, as of `now`.
fn heartbeat_subtitle(v: &BotView, now: i64) -> Option<String> {
    let h = v.agents.as_ref()?.heartbeat.as_ref()?;
    let wake = h
        .next_wake_at
        .as_deref()
        .map(|t| format!(" · next wake {}", model::relative_to(t, now)))
        .unwrap_or_default();
    Some(format!(
        "Session {} · heartbeat {}{wake}",
        h.session,
        model::relative_to(&h.updated_at, now)
    ))
}

fn bot_view_rows(board: &Board, v: &BotView, ctx: &Ctx) -> gtk::Widget {
    let group = widgets::group(&v.label, Some(&format!("@{}", v.bot_login)));
    let ask = gtk::Button::builder()
        .label("Ask the Bot")
        .valign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .tooltip_text("Opens a prefilled issue in the bot's tracker, which you submit yourself")
        .build();
    ask.set_action_name(Some(widgets::OPEN_URI));
    ask.set_action_target_value(Some(&v.ask.issue_url.to_variant()));
    group.set_header_suffix(Some(&ask));
    let Some(agents) = &v.agents else {
        group.add(&widgets::link_row(
            "Board",
            "Not read yet",
            &v.board_url,
            &[],
        ));
        return group.upcast();
    };
    // The coordinator.
    match &agents.heartbeat {
        None => group.add(&widgets::action_row("No heartbeat is published", "")),
        Some(h) => {
            let title = format!("Coordinator is {}", h.loop_state);
            let mut chips = vec![];
            if h.stale {
                let chip = widgets::chip(
                    "May have stopped",
                    Some("warning"),
                    Some("dialog-warning-symbolic"),
                );
                chip.set_tooltip_text(Some("The heartbeat is too old to trust"));
                chips.push(chip);
            }
            let row = match &h.comment_url {
                Some(url) => widgets::link_row(&title, "", url, &chips),
                None => {
                    let row = widgets::action_row(&title, "");
                    for chip in &chips {
                        row.add_suffix(chip);
                    }
                    row
                }
            };
            let id = v.connection_id.clone();
            ctx.ticker.subtitle(&row, board, ctx.now, move |b, now| {
                heartbeat_subtitle(find_bot(b, &id)?, now)
            });
            row.add_prefix(&gtk::Image::from_icon_name("system-run-symbolic"));
            group.add(&row);
        }
    }
    // The board.
    match &agents.board {
        Some(board_summary) => {
            let items: usize = board_summary.statuses.values().sum();
            let statuses: Vec<String> = board_summary
                .statuses
                .iter()
                .map(|(status, n)| format!("{n} {status}"))
                .collect();
            // The board shown is never taken for all of it.
            let mut chips = vec![];
            if board_summary.unreadable > 0 {
                let chip = widgets::chip(
                    &format!("{} unreadable", board_summary.unreadable),
                    Some("warning"),
                    Some("dialog-warning-symbolic"),
                );
                chip.set_tooltip_text(Some(
                    "Items this token can't read: the board above isn't all of it",
                ));
                chips.push(chip);
            }
            let row = widgets::link_row(
                &format!("Board: {}", count(items, "item", "items")),
                &format!(
                    "{} · {}",
                    statuses.join(", "),
                    count(
                        board_summary.fork_prs,
                        "open fork pull request",
                        "open fork pull requests"
                    )
                ),
                &v.board_url,
                &chips,
            );
            row.add_prefix(&gtk::Image::from_icon_name("view-grid-symbolic"));
            group.add(&row);
        }
        None => group.add(&widgets::link_row(
            "Board",
            "Not read yet",
            &v.board_url,
            &[],
        )),
    }
    if agents.stale {
        let row = widgets::action_row("Not current", "");
        let id = v.connection_id.clone();
        ctx.ticker.subtitle(&row, board, ctx.now, move |b, now| {
            let agents = find_bot(b, &id)?.agents.as_ref()?;
            model::stale_text(agents.stale, agents.last_success.as_deref(), now)
        });
        row.add_prefix(&widgets::warning_icon());
        group.add(&row);
    }
    if let Some(error) = &agents.error {
        let row = widgets::action_row(
            "Not updated",
            &format!("Showing the last heartbeat read: {error}"),
        );
        row.add_prefix(&widgets::warning_icon());
        group.add(&row);
    }
    group.upcast()
}

fn agent_runs(board: &Board, v: &BotView, ctx: &Ctx) -> Option<gtk::Widget> {
    let h = v.agents.as_ref()?.heartbeat.as_ref()?;
    let group = widgets::group(
        &format!("Agent runs ({})", h.runs.len()),
        Some(
            "The workers the coordinator runs. A run that finished delivered nothing by finishing.",
        ),
    );
    if h.runs.is_empty() {
        group.add(&widgets::action_row("No agent runs", ""));
    }
    for r in &h.runs {
        // The status after the title, so titles line up.
        let chip = widgets::chip(&r.status, None, None);
        let row = widgets::link_row(&r.item_key, "", &r.item_url, &[chip]);
        row.add_prefix(&gtk::Image::from_icon_name("system-run-symbolic"));
        let (id, name) = (v.connection_id.clone(), r.name.clone());
        ctx.ticker.subtitle(&row, board, ctx.now, move |b, now| {
            let runs = &find_bot(b, &id)?.agents.as_ref()?.heartbeat.as_ref()?.runs;
            let r = runs.iter().find(|r| r.name == name)?;
            let devspace = r
                .devspace
                .as_deref()
                .map(|d| format!(" · on {d}"))
                .unwrap_or_default();
            Some(format!(
                "{} · started {}{devspace}",
                r.name,
                model::relative_to(&r.started_at, now)
            ))
        });
        group.add(&row);
    }
    if h.skipped > 0 {
        group.add(&widgets::action_row(
            &format!("{} more left out, as malformed", h.skipped),
            "",
        ));
    }
    Some(group.upcast())
}

/// The buttons that ask the bot about an item: a comment on it, or a new
/// issue, which the person submits themselves.
fn ask_buttons(board: &Board, item: &Item) -> Vec<gtk::Widget> {
    let Some(ask) = board.bots.iter().find_map(|v| v.asks.get(&item.id)) else {
        return vec![];
    };
    let mut buttons = vec![];
    if let (Some(url), Some(comment)) = (&ask.comment_url, &ask.comment) {
        let button = gtk::Button::builder()
            .icon_name("chat-message-new-symbolic")
            .css_classes(["flat"])
            .tooltip_text(format!(
                "Comment on it: copies “{}” to paste, and opens it",
                comment.trim()
            ))
            .build();
        button.set_action_name(Some(COMMENT));
        button.set_action_target_value(Some(&(comment.as_str(), url.as_str()).to_variant()));
        buttons.push(button.upcast());
    }
    buttons.push(
        widgets::link_button(
            "mail-message-new-symbolic",
            "Ask the bot about it in a new issue, which you submit yourself",
            &ask.issue_url,
        )
        .upcast(),
    );
    buttons
}

fn bot(board: &Board, ctx: &Ctx) -> Content {
    let waiting = model::bot_waiting(&board.snapshot);
    if board.bots.is_empty() && waiting.is_empty() {
        return widgets::empty(
            "system-run-symbolic",
            "No bot is connected",
            "Connect the bot's board, fork pull requests and heartbeat with `workboard connect bot`, here or on the hub.",
        );
    }
    let mut children = vec![];
    for v in &board.bots {
        children.push(bot_view_rows(board, v, ctx));
    }
    if !waiting.is_empty() {
        let group = widgets::group(
            &format!("Waiting on you ({})", waiting.len()),
            Some("Items the bot marked Needs human or Draft. Approvals stay on GitHub, on the pull request itself."),
        );
        let list = widgets::boxed_list();
        let gutter = gutter(waiting.iter().map(|i| i.outcome));
        for item in waiting {
            let fields = item.bot.clone().unwrap_or_default();
            let mut chips_note = vec![];
            chips_note.extend(fields.priority.clone());
            chips_note.extend(fields.why.clone().filter(|w| !w.is_empty()));
            let note = chips_note.join(" · ");
            let extra = Extra {
                note: Some(&note),
                actions: ask_buttons(board, item),
                gutter,
                ..Extra::default()
            };
            list.append(&item_row(board, item, ctx, extra));
        }
        group.add(&list);
        children.push(group.upcast());
    }
    for v in &board.bots {
        children.extend(agent_runs(board, v, ctx));
    }
    if board.bots.is_empty() {
        children.push(
            widgets::group(
                "Agent runs",
                Some("The bot is connected on the hub; its heartbeat and agent runs show in the hub's web page."),
            )
            .upcast(),
        );
    }
    widgets::page(&children)
}

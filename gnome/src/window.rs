//! The main window: a sidebar of views next to the chosen one, collapsing
//! to one pane at a time on narrow widths, as GNOME's adaptive apps do.

use crate::{
    api::Board,
    model,
    views::{Ctx, Expanded, Ticker, View},
    widgets::Content,
};
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;

/// Narrower than this, the sidebar and the view take turns.
const NARROW: &str = "max-width: 640sp";

/// One view's place in the window. Its scrolling column stays as it is
/// rebuilt, so the scroll position does too.
struct Slot {
    view: View,
    stack: gtk::Stack,
    column: gtk::Box,
    empty: adw::Bin,
    badge: gtk::Label,
    /// What it shows, as last built ([`View::shape`]): it is rebuilt only
    /// when that changes.
    shape: RefCell<Option<String>>,
    /// Its texts that tell time, updated in place between rebuilds.
    ticker: Ticker,
}

pub struct Window {
    pub window: adw::ApplicationWindow,
    split: adw::NavigationSplitView,
    sidebar: gtk::ListBox,
    content: adw::NavigationPage,
    title: adw::WindowTitle,
    stack: gtk::Stack,
    slots: Vec<Slot>,
    banner: adw::Banner,
    toasts: adw::ToastOverlay,
    status: gtk::Label,
    sync: gtk::Button,
    expanded: Expanded,
}

fn menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let sources = gio::Menu::new();
    sources.append(Some("_Sync Sources"), Some("app.sync"));
    sources.append(Some("_Open the Web Page"), Some("app.web"));
    menu.append_section(None, &sources);
    let app = gio::Menu::new();
    app.append(Some("_About Workboard"), Some("app.about"));
    app.append(Some("_Quit"), Some("app.quit"));
    menu.append_section(None, &app);
    menu
}

fn slot(view: View) -> (Slot, gtk::ListBoxRow) {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    let clamp = adw::Clamp::builder()
        .maximum_size(880)
        .tightening_threshold(600)
        .child(&column)
        .build();
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();
    let empty = adw::Bin::new();
    let stack = gtk::Stack::new();
    stack.add_named(&scrolled, Some("page"));
    stack.add_named(&empty, Some("empty"));

    let badge = gtk::Label::builder()
        .css_classes(["count", "numeric"])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    row_box.append(&gtk::Image::from_icon_name(view.icon()));
    row_box.append(
        &gtk::Label::builder()
            .label(view.title())
            .xalign(0.0)
            .hexpand(true)
            .build(),
    );
    row_box.append(&badge);
    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    (
        Slot {
            view,
            stack,
            column,
            empty,
            badge,
            shape: RefCell::new(None),
            ticker: Ticker::default(),
        },
        row,
    )
}

impl Window {
    pub fn new(app: &adw::Application) -> Self {
        let sidebar = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        let mut slots = vec![];
        for view in View::ALL {
            let (slot, row) = slot(view);
            sidebar.append(&row);
            stack.add_named(&slot.stack, Some(view.id()));
            slots.push(slot);
        }

        // The sidebar.
        let status = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .wrap(true)
            .xalign(0.0)
            .margin_start(18)
            .margin_end(18)
            .margin_top(6)
            .margin_bottom(12)
            .build();
        let sidebar_toolbar = adw::ToolbarView::new();
        sidebar_toolbar.add_top_bar(
            &adw::HeaderBar::builder()
                .title_widget(&adw::WindowTitle::new("Workboard", ""))
                .build(),
        );
        sidebar_toolbar.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&sidebar)
                .build(),
        ));
        sidebar_toolbar.add_bottom_bar(&status);
        let sidebar_page = adw::NavigationPage::builder()
            .title("Workboard")
            .tag("sidebar")
            .child(&sidebar_toolbar)
            .build();

        // The chosen view.
        let title = adw::WindowTitle::new(View::Mine.title(), "");
        let sync = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Sync Sources")
            .action_name("app.sync")
            .build();
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .tooltip_text("Main Menu")
                .menu_model(&menu())
                .primary(true)
                .build(),
        );
        header.pack_end(&sync);
        let banner = adw::Banner::builder()
            .button_label("Retry")
            .action_name("app.refresh")
            .build();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&stack));
        let content_toolbar = adw::ToolbarView::new();
        content_toolbar.add_top_bar(&header);
        content_toolbar.add_top_bar(&banner);
        content_toolbar.set_content(Some(&toasts));
        let content = adw::NavigationPage::builder()
            .title(View::Mine.title())
            .tag("content")
            .child(&content_toolbar)
            .build();

        let split = adw::NavigationSplitView::builder()
            .sidebar(&sidebar_page)
            .content(&content)
            .min_sidebar_width(220.0)
            .build();
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Workboard")
            .icon_name(crate::APP_ID)
            .default_width(1100)
            .default_height(760)
            .width_request(360)
            .height_request(400)
            .content(&split)
            // Notifications keep coming while the window is closed.
            .hide_on_close(true)
            .build();
        let breakpoint = adw::Breakpoint::new(
            adw::BreakpointCondition::parse(NARROW)
                .expect("invariant: a valid breakpoint condition"),
        );
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(breakpoint);

        let this = Self {
            window,
            split,
            sidebar,
            content,
            title,
            stack,
            slots,
            banner,
            toasts,
            status,
            sync,
            expanded: Expanded::default(),
        };
        if let Some(first) = this.sidebar.row_at_index(0) {
            this.sidebar.select_row(Some(&first));
        }
        this
    }

    /// Show `view`, also when only one pane fits.
    pub fn show(&self, view: View) {
        if let Some(row) = self.sidebar.row_at_index(index(view)) {
            if !row.is_selected() {
                self.sidebar.select_row(Some(&row));
            }
        }
        self.stack.set_visible_child_name(view.id());
        self.title.set_title(view.title());
        self.content.set_title(view.title());
        self.split.set_show_content(true);
    }

    /// Calls `f` with the view the person picks in the sidebar.
    pub fn connect_view_chosen(&self, f: impl Fn(View) + 'static) {
        self.sidebar.connect_row_activated(move |_, row| {
            if let Some(view) = usize::try_from(row.index())
                .ok()
                .and_then(|i| View::ALL.get(i))
            {
                f(*view);
            }
        });
    }

    pub fn toast(&self, text: &str) {
        self.toasts
            .add_toast(adw::Toast::new(&glib::markup_escape_text(text)));
    }

    /// Whether a sync of every source is running.
    pub fn set_syncing(&self, syncing: bool) {
        if syncing {
            self.sync.set_child(Some(&adw::Spinner::new()));
        } else {
            self.sync.set_icon_name("view-refresh-symbolic");
        }
    }

    /// Show `board`, aged as of `now`, or why there is none. `offline`
    /// says the service didn't answer the last time; what it answered
    /// before is then shown, marked stale.
    pub fn render(&self, board: Option<&Board>, offline: Option<&str>, now: i64) {
        let Some(board) = board else {
            self.banner.set_revealed(false);
            for slot in &self.slots {
                slot.ticker.clear();
                *slot.shape.borrow_mut() = None;
            }
            let Some(error) = offline else {
                self.status.set_label("Loading");
                for slot in &self.slots {
                    let spinner = adw::Spinner::builder()
                        .width_request(32)
                        .height_request(32)
                        .halign(gtk::Align::Center)
                        .valign(gtk::Align::Center)
                        .build();
                    fill(slot, Content::Empty(spinner.upcast()));
                }
                return;
            };
            self.status.set_label("Can't reach Workboard");
            for slot in &self.slots {
                let page = adw::StatusPage::builder()
                    .icon_name("network-offline-symbolic")
                    .title("Can't Reach Workboard")
                    .description(glib::markup_escape_text(&format!(
                        "{error}. This app shows what the local Workboard service has."
                    )))
                    .build();
                let retry = gtk::Button::builder()
                    .label("Retry")
                    .halign(gtk::Align::Center)
                    .css_classes(["pill", "suggested-action"])
                    .action_name("app.refresh")
                    .build();
                page.set_child(Some(&retry));
                fill(slot, Content::Empty(page.upcast()));
                slot.badge.set_visible(false);
            }
            return;
        };
        let is_offline = offline.is_some();
        match offline {
            Some(error) => {
                self.banner.set_title(&glib::markup_escape_text(&format!(
                    "Can't reach Workboard: {error}. Showing what it last answered, marked stale."
                )));
                self.banner.set_revealed(true);
                self.status.set_label("Can't reach Workboard");
            }
            None => {
                self.banner.set_revealed(false);
                let mode = match board.snapshot.hub {
                    Some(_) => "Companion, pulling from a hub",
                    None => "This machine's work",
                };
                self.status.set_label(mode);
            }
        }
        let aged = model::age(board, is_offline, now);
        let shape = model::structural(&aged);
        for slot in &self.slots {
            // Rebuilt only when what it shows changed, which keeps keyboard
            // focus and a screen reader's place; otherwise only its times
            // are updated.
            let key = slot.view.shape(&shape);
            if slot.shape.borrow().as_deref() == Some(key.as_str()) {
                slot.ticker.tick(&aged, now);
            } else {
                slot.ticker.clear();
                let ctx = Ctx {
                    now,
                    ticker: &slot.ticker,
                    expanded: &self.expanded,
                };
                fill(slot, slot.view.build(&aged, &ctx));
                *slot.shape.borrow_mut() = Some(key);
            }
            match slot.view.badge(&aged) {
                Some((text, attention)) => {
                    slot.badge.set_label(&text);
                    if attention {
                        slot.badge.add_css_class("attention");
                    } else {
                        slot.badge.remove_css_class("attention");
                    }
                    slot.badge.set_visible(true);
                }
                None => slot.badge.set_visible(false),
            }
        }
    }
}

/// Where `view` is in the sidebar.
fn index(view: View) -> i32 {
    View::ALL
        .iter()
        .position(|v| *v == view)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(0)
}

fn fill(slot: &Slot, content: Content) {
    match content {
        Content::Page(children) => {
            while let Some(child) = slot.column.first_child() {
                slot.column.remove(&child);
            }
            for child in &children {
                slot.column.append(child);
            }
            slot.stack.set_visible_child_name("page");
        }
        Content::Empty(widget) => {
            slot.empty.set_child(Some(&widget));
            slot.stack.set_visible_child_name("empty");
        }
    }
}

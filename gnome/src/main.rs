//! Workboard for GNOME: a native companion to the local Workboard service.
//! It shows what the service has, through its HTTP API on loopback, and
//! announces new Mine items as desktop notifications, also while its
//! window is closed. It keeps no work of its own.

mod api;
mod model;
mod notify;
mod views;
mod widgets;
mod window;

use adw::prelude::*;
use anyhow::{Context, Result};
use clap::Parser;
use gtk::{gdk, gio, glib};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};
use views::View;

/// The application ID: the D-Bus name, the desktop file's and the icon's.
/// A build of your own takes its own from `WORKBOARD_APP_ID` at build time,
/// as `make -C gnome APP_ID=...` and the Flatpak manifest set it (see
/// docs/run-your-own.md); otherwise it is this repository's.
pub const APP_ID: &str = match option_env!("WORKBOARD_APP_ID") {
    Some(id) => id,
    None => DEFAULT_APP_ID,
};
const DEFAULT_APP_ID: &str = "io.github.jmarrero_bot.Workboard";
/// Where the About dialog sends people: a build's own homepage, from
/// `WORKBOARD_HOMEPAGE` (`make -C gnome HOMEPAGE=...`), or this repository.
const HOMEPAGE: &str = match option_env!("WORKBOARD_HOMEPAGE") {
    Some(url) => url,
    None => env!("CARGO_PKG_REPOSITORY"),
};
const _: () = assert!(
    valid_app_id(APP_ID),
    "WORKBOARD_APP_ID isn't an application ID, such as org.example.Workboard"
);

/// Whether `id` is an application ID GLib and Flatpak take: at most 255
/// bytes, of at least two elements separated by dots, each of ASCII
/// letters, digits, `_` and `-`, and not starting with a digit.
const fn valid_app_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 255 {
        return false;
    }
    let (mut i, mut elements, mut start) = (0, 1, true);
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'.' {
            if start {
                return false;
            }
            elements += 1;
            start = true;
        } else if (start && b.is_ascii_digit())
            || !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return false;
        } else {
            start = false;
        }
        i += 1;
    }
    !start && elements >= 2
}
/// How often the app asks the service again, as the web page does.
const REFRESH_SECS: u32 = 30;
/// Where, in the app's own state directory, the notified IDs are kept.
const NOTIFIED: &str = "workboard-gnome/notified.json";
/// How long quitting waits for that file to be saved.
const NOTIFIED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// The links the app opens: web pages only.
const WEB_SCHEMES: [&str; 2] = ["https://", "http://"];

#[derive(Parser)]
#[command(
    version,
    about = "Workboard for GNOME: what waits on you, from the local Workboard service"
)]
struct Args {
    /// The local service's data directory, where its token is. The same as
    /// the service's and the CLI's.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Port of the local Workboard service.
    #[arg(long, default_value_t = api::DEFAULT_PORT)]
    port: u16,
    /// Start without a window, to keep notifying, as at login.
    #[arg(long)]
    background: bool,
    /// Read everything the app shows once, print how much there is, and
    /// exit. Needs no display.
    #[arg(long)]
    check: bool,
}

/// What the app knows, shared by its window, actions and timer.
struct State {
    client: api::Client,
    window: RefCell<Option<Rc<window::Window>>>,
    /// The service's last answer.
    board: RefCell<Option<api::Board>>,
    /// Why the last request failed, if it did.
    offline: RefCell<Option<String>>,
    loading: Cell<bool>,
    /// The Mine items already announced; None until the first answer
    /// on a first run.
    seen: RefCell<Option<notify::Seen>>,
    /// Saves them, off the main thread.
    notified: notify::Writer,
    /// Start without a window.
    background: Cell<bool>,
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("Cannot start the app's I/O runtime")
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Run `request` with the client off the main thread, and hand its result
/// to `then` on it.
fn call<T, F>(
    state: &Rc<State>,
    request: impl FnOnce(api::Client) -> F + 'static,
    then: impl FnOnce(&Rc<State>, Result<T>) + 'static,
) where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T>> + Send + 'static,
{
    let state = state.clone();
    let future = request(state.client.clone());
    glib::spawn_future_local(async move {
        let result = match runtime().spawn(future).await {
            Ok(result) => result,
            Err(e) => Err(anyhow::Error::from(e).context("The request stopped")),
        };
        then(&state, result);
    });
}

/// Ask the service for everything again, and show it.
fn reload(state: &Rc<State>) {
    if state.loading.replace(true) {
        return;
    }
    call(
        state,
        |client| async move { client.board().await },
        |state, result| {
            state.loading.set(false);
            match result {
                Ok(board) => {
                    announce(state, &board);
                    *state.offline.borrow_mut() = None;
                    *state.board.borrow_mut() = Some(board);
                }
                Err(e) => {
                    *state.offline.borrow_mut() = Some(format!("{e:#}"));
                }
            }
            render(state);
        },
    );
}

fn render(state: &State) {
    if let Some(window) = state.window.borrow().as_ref() {
        window.render(
            state.board.borrow().as_ref(),
            state.offline.borrow().as_deref(),
            now(),
        );
    }
}

/// Send a notification for each Mine item not seen before, and remember
/// it, also across restarts.
fn announce(state: &State, board: &api::Board) {
    let Some(app) = gio::Application::default() else {
        return;
    };
    let found = notify::update(state.seen.borrow().as_ref(), &board.mine.mine, now());
    for note in notify::notes(&found.new) {
        let notification = gio::Notification::new(&note.title);
        notification.set_body(Some(&note.body));
        notification.set_default_action_and_target_value(
            "app.show-view",
            Some(&View::Mine.id().to_variant()),
        );
        if let Some(url) = &note.url {
            notification.add_button_with_target_value(
                "Open",
                widgets::OPEN_URI,
                Some(&url.to_variant()),
            );
        }
        app.send_notification(Some(&note.id), &notification);
    }
    if found.changed {
        state.notified.save(found.seen.clone());
    }
    *state.seen.borrow_mut() = Some(found.seen);
}

/// The window, made the first time it is needed.
fn window(app: &adw::Application, state: &Rc<State>) -> Rc<window::Window> {
    if let Some(window) = state.window.borrow().as_ref() {
        return window.clone();
    }
    let window = Rc::new(window::Window::new(app));
    let chosen = Rc::downgrade(&window);
    window.connect_view_chosen(move |view| {
        if let Some(window) = chosen.upgrade() {
            window.show(view);
        }
    });
    *state.window.borrow_mut() = Some(window.clone());
    render(state);
    window
}

fn add_action(
    app: &adw::Application,
    name: &str,
    parameter: Option<&glib::VariantTy>,
    f: impl Fn(Option<&glib::Variant>) + 'static,
) {
    let action = gio::SimpleAction::new(name, parameter);
    action.connect_activate(move |_, p| f(p));
    app.add_action(&action);
}

/// Open a web page in the browser.
fn open_uri(state: &State, uri: &str) {
    if !WEB_SCHEMES.iter().any(|s| uri.starts_with(s)) {
        eprintln!("Not opening {uri}: only web pages are opened");
        return;
    }
    let parent = state.window.borrow().as_ref().map(|w| w.window.clone());
    gtk::UriLauncher::new(uri).launch(parent.as_ref(), gio::Cancellable::NONE, |result| {
        if let Err(e) = result {
            eprintln!("Cannot open the browser: {e}");
        }
    });
}

fn toast(state: &State, text: &str) {
    match state.window.borrow().as_ref() {
        Some(window) => window.toast(text),
        None => eprintln!("{text}"),
    }
}

/// A request that changes something on the service.
type Change = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;

/// Run a change on the service, say how it went, and show what it did.
fn change(
    state: &Rc<State>,
    request: impl FnOnce(api::Client) -> Change + 'static,
    done: &'static str,
) {
    call(state, request, move |state, result| {
        if let Some(window) = state.window.borrow().as_ref() {
            window.set_syncing(false);
        }
        match result {
            Ok(()) => toast(state, done),
            Err(e) => toast(state, &format!("{e:#}")),
        }
        reload(state);
    });
}

fn string(p: Option<&glib::Variant>) -> Option<String> {
    p.and_then(|p| p.get::<String>())
}

fn add_actions(app: &adw::Application, state: &Rc<State>) {
    let s = state.clone();
    let a = app.clone();
    add_action(app, "show-view", Some(glib::VariantTy::STRING), move |p| {
        if let Some(view) = string(p).as_deref().and_then(View::of) {
            let window = window(&a, &s);
            window.show(view);
            window.window.present();
        }
    });
    let s = state.clone();
    add_action(app, "open-uri", Some(glib::VariantTy::STRING), move |p| {
        if let Some(uri) = string(p) {
            open_uri(&s, &uri);
        }
    });
    let s = state.clone();
    add_action(
        app,
        "comment",
        Some(glib::VariantTy::new("(ss)").expect("invariant: a valid type")),
        move |p| {
            let Some((text, url)) = p.and_then(|p| p.get::<(String, String)>()) else {
                return;
            };
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&text);
                toast(
                    &s,
                    &format!("Copied “{}” to paste in your comment", text.trim()),
                );
            }
            open_uri(&s, &url);
        },
    );
    let s = state.clone();
    add_action(app, "refresh", None, move |_| reload(&s));
    let s = state.clone();
    add_action(app, "sync", None, move |_| {
        if let Some(window) = s.window.borrow().as_ref() {
            window.set_syncing(true);
        }
        change(
            &s,
            |client| Box::pin(async move { client.sync(None).await }),
            "Synced",
        );
    });
    let s = state.clone();
    add_action(
        app,
        "sync-connection",
        Some(glib::VariantTy::STRING),
        move |p| {
            if let Some(id) = string(p) {
                change(
                    &s,
                    move |client| Box::pin(async move { client.sync(Some(&id)).await }),
                    "Synced",
                );
            }
        },
    );
    let s = state.clone();
    add_action(app, "verify", Some(glib::VariantTy::STRING), move |p| {
        if let Some(id) = string(p) {
            change(
                &s,
                move |client| Box::pin(async move { client.verify(&id).await }),
                "The credential works; scheduled syncs resume",
            );
        }
    });
    let s = state.clone();
    add_action(app, "web", None, move |_| {
        // The token is read off the main thread, like every request's.
        call(
            &s,
            |client| async move { client.pairing_url() },
            |state, result| match result {
                Ok(url) => open_uri(state, &url),
                Err(e) => toast(state, &format!("{e:#}")),
            },
        );
    });
    let s = state.clone();
    add_action(app, "about", None, move |_| {
        let about = adw::AboutDialog::builder()
            .application_name("Workboard")
            .application_icon(APP_ID)
            .developer_name("The Workboard contributors")
            .version(env!("CARGO_PKG_VERSION"))
            .license_type(gtk::License::MitX11)
            .website(HOMEPAGE)
            .comments(
                "What waits on you, from every account, as the local Workboard service has it.",
            )
            .build();
        let parent = s.window.borrow().as_ref().map(|w| w.window.clone());
        about.present(parent.as_ref());
    });
    let a = app.clone();
    add_action(app, "quit", None, move |_| a.quit());
    app.set_accels_for_action("app.sync", &["<Control>r", "F5"]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.set_accels_for_action("window.close", &["<Control>w"]);
}

/// In a Flatpak, ask the Background portal to let the app run with its
/// window closed and start it, in the background, at login: GNOME then
/// lists it among background apps, and the person decides. Outside one,
/// the app just keeps running, and autostart is a desktop file of the
/// person's (docs/gnome.md).
fn request_background() {
    if !std::path::Path::new("/.flatpak-info").exists() {
        return;
    }
    let options = glib::VariantDict::new(None);
    options.insert_value(
        "reason",
        &"Keep announcing new work while the window is closed, and from login".to_variant(),
    );
    options.insert_value("autostart", &true.to_variant());
    options.insert_value(
        "commandline",
        &vec!["workboard-gnome", "--background"].to_variant(),
    );
    let parameters = glib::Variant::tuple_from_iter(["".to_variant(), options.end()]);
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, move |bus| {
        let bus = match bus {
            Ok(bus) => bus,
            Err(e) => return eprintln!("Cannot reach the session bus: {e}"),
        };
        // It answers on a request object, once the person decided; the
        // decision is theirs and the portal's to keep.
        bus.call(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Background",
            "RequestBackground",
            Some(&parameters),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            |result| {
                if let Err(e) = result {
                    eprintln!("The Background portal didn't take the request: {e}");
                }
            },
        );
    });
}

fn startup(app: &adw::Application, state: &Rc<State>) {
    let css = gtk::CssProvider::new();
    css.load_from_string(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // Run from the source tree, before `make install`: its icon is
        // there.
        let icons = gtk::IconTheme::for_display(&display);
        if !icons.has_icon(APP_ID) {
            icons.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
        }
    }
    add_actions(app, state);
    request_background();
    reload(state);
    let s = state.clone();
    glib::timeout_add_seconds_local(REFRESH_SECS, move || {
        reload(&s);
        glib::ControlFlow::Continue
    });
}

/// `--check`: read what the app shows, and say how much there is.
fn check(client: &api::Client) -> Result<()> {
    let board = runtime()
        .block_on(client.board())
        .context("Cannot read what the app shows")?;
    let counts: Vec<String> = View::ALL
        .iter()
        .map(|v| {
            let badge = v.badge(&board).map_or("0".into(), |(text, _)| text);
            format!("{}: {badge}", v.id())
        })
        .collect();
    println!(
        "{}; {} connections, {} items, {} efforts, {} bots, hub {}",
        counts.join(", "),
        board.snapshot.connections.len(),
        board.snapshot.items.len(),
        board.snapshot.efforts.len(),
        board.bots.len(),
        if board.snapshot.hub.is_some() {
            "set"
        } else {
            "not set"
        }
    );
    Ok(())
}

fn main() -> glib::ExitCode {
    let args = Args::parse();
    let client = match args
        .data_dir
        .map_or_else(api::default_data_dir, Ok)
        .and_then(|dir| api::Client::new(dir, args.port))
    {
        Ok(client) => client,
        Err(e) => {
            eprintln!("{e:#}");
            return glib::ExitCode::FAILURE;
        }
    };
    if args.check {
        return match check(&client) {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                glib::ExitCode::FAILURE
            }
        };
    }
    // Read once, before the main loop starts; saved by a thread of its own.
    let seen_path = glib::user_state_dir().join(NOTIFIED);
    let seen = notify::Seen::load(&seen_path).unwrap_or_else(|e| {
        eprintln!("{e:#}; starting over, without announcing what is there now");
        None
    });
    let state = Rc::new(State {
        client,
        window: RefCell::new(None),
        board: RefCell::new(None),
        offline: RefCell::new(None),
        loading: Cell::new(false),
        seen: RefCell::new(seen),
        notified: notify::Writer::spawn(seen_path),
        background: Cell::new(args.background),
    });
    let app = adw::Application::builder().application_id(APP_ID).build();
    let s = state.clone();
    app.connect_startup(move |app| startup(app, &s));
    let s = state.clone();
    app.connect_activate(move |app| {
        let window = window(app, &s);
        // Started in the background: the window exists, hidden, which
        // keeps the app running until it is asked to quit.
        if !s.background.replace(false) {
            window.window.present();
        }
    });
    // The arguments were parsed above; GApplication gets none of them.
    let code = app.run_with_args::<&str>(&[]);
    // The main loop is over: what was announced last is kept before exit.
    state.notified.finish(NOTIFIED_TIMEOUT);
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_as_glib_takes_them() {
        for (id, valid) in [
            (DEFAULT_APP_ID, true),
            ("org.example.Workboard", true),
            ("org.example_team.Work-board2", true),
            ("a.b", true),
            ("Workboard", false),
            ("", false),
            (".org.example", false),
            ("org.example.", false),
            ("org..example", false),
            ("org.3example", false),
            ("org.example/Workboard", false),
            ("org.exämple.Workboard", false),
        ] {
            assert_eq!(valid_app_id(id), valid, "{id}");
            assert_eq!(gio::Application::id_is_valid(id), valid, "{id}");
        }
        let long = format!("org.{}", "a".repeat(252));
        assert!(!valid_app_id(&long) && !gio::Application::id_is_valid(&long));
    }
}

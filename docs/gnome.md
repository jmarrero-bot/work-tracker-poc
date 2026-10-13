# The GNOME app

`workboard-gnome` is a native GNOME app for the companion's laptop, in Rust with GTK 4 and libadwaita. It is a client of the local Workboard service, like the page and the CLI: it reads everything through the service's HTTP API on `127.0.0.1`, with the service's token: the copy the service puts in the session's runtime directory (`$XDG_RUNTIME_DIR/workboard/service-PORT.token`), or else the one in its data directory, which the CLI reads; when the service refuses the first, the app tries the second. It never opens the database and keeps no work of its own; the only thing it writes is which Mine items it already announced (below).

It lives in `gnome/`, a crate of its own in the cargo workspace. A plain `cargo build` still builds only the service, which needs no GTK.

## What it shows

A sidebar of views, next to the chosen one; below 640 pixels wide, as on a phone, the two take turns. It follows the system's light or dark style. Each view says the same as the page:

- **Mine**: what waits on each connected account, once, with each account's own reasons, the source's own status, the outcome, the zone, and how fresh it is. What watching another user found is listed apart, as theirs.
- **Next** and **Inbox**: next actions, reviews and closed work that needs a decision; and everything not settled, grouped by what it needs.
- **Efforts**, with progress that counts delivered work only.
- **Connections**: how each syncs, the kind of its last error, and **Verify Again** for one whose credential was rejected.
- **Hub**: the last pull from the hub and whether its work is stale.
- **Bot**: the bot's board, its coordinator's heartbeat and agent runs, and the items waiting on you. **Ask the Bot** opens a prefilled issue in the browser, which you submit yourself; commenting on an item opens it, with the start of the comment copied to paste.

Each item shows its outcome (open, delivered, closed without delivery, abandoned, unknown) next to its source's status, never instead of it. What comes from the hub is marked "From the hub, read-only", and stale data is marked stale, also when the service can't be reached: the app then shows what it last answered, with a banner.

The app asks the service again every 30 seconds, like the page, and also with its window closed. A view is rebuilt only when what it shows changed; otherwise only its times ("3 min ago") are updated in place, so keyboard focus and a screen reader's place stay. Requests, reading the token, and saving what was announced run off the main thread; quitting waits a moment for that save. Its sync button asks the service to sync every source now.

**Open the Web Page**, in the main menu, opens the same private pairing URL `workboard serve` prints, with the service token in its fragment. The page removes it once paired, but the URL can stay in the browser's history while that service runs. A one-time pairing code instead is [#40](https://github.com/jmarrero-bot/work-tracker-poc/issues/40).

## Notifications and running in the background

A new Mine item is announced with a desktop notification, which opens Mine, or the item in the browser. Each item is announced once, by its ID, also across restarts: the app keeps the IDs it has seen, and about when it last saw each, in `$XDG_STATE_HOME/workboard-gnome/notified.json` (`~/.local/state/...`). It writes that file off the main thread, and only when it changed: a new ID, one forgotten, or one last seen more than a day ago. On the first run it records what waits without announcing it, and more than three new items at once get one notification that counts them.

An ID is forgotten once it has been out of Mine for 30 days. An item that comes back after that, even on the very poll that forgets it, is announced again, once; one that comes back sooner isn't.

Closing the window hides it, and the app keeps running and notifying; **Quit** in the main menu (Ctrl+Q) stops it. `workboard-gnome --background` starts it without a window, as at login. The Flatpak asks the Background portal for that when it starts: GNOME asks you once, then starts it in the background at login and lists it among background apps. Installed from a toolbox, copy the desktop file to `~/.config/autostart/` and add `--background` to its `Exec` line.

## Run it in a toolbox

The companion's toolbox has GTK 4 and libadwaita (on Fedora, `sudo dnf install gtk4-devel libadwaita-devel`). With the service running:

```sh
cargo run -p workboard-gnome
cargo run -p workboard-gnome -- --data-dir /tmp/workboard-demo   # the demo's service
```

`--data-dir` and `--port` mean what they mean for the CLI. `workboard-gnome --check` reads everything the app shows once and prints how much there is, without a display.

To install it, with its desktop file and icon, under `~/.local`:

```sh
make -C gnome install
```

GNOME Shell on the host then lists Workboard. Its desktop file runs `workboard-gnome` on the host, which works when the host has the same GTK and libadwaita, as Fedora Workstation and Silverblue do with a Fedora toolbox of the same release. Otherwise use the Flatpak, or edit the `Exec` line to `toolbox run workboard-gnome`. Either way, a toolbox shares the host's network, so the app reaches the service the toolbox runs on loopback.

## Flatpak

```sh
flatpak install flathub org.gnome.Platform//50 org.gnome.Sdk//50 org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --install --force-clean build-dir gnome/io.github.jmarrero_bot.Workboard.yml
flatpak run io.github.jmarrero_bot.Workboard
```

That is this repository's app ID. A build of your own takes its own, from `make -C gnome APP_ID=...` or a copy of the manifest with another `id` ([run-your-own.md](run-your-own.md#build-the-gnome-app-with-your-own-app-id)).

The build fetches the crates in `Cargo.lock` itself (`--share=network` while building); a Flathub build would vendor them instead. The sandbox gets two permissions beyond a display:

- **The network.** The app only talks to `127.0.0.1`, but Flatpak can't limit it to loopback: without network access the sandbox has a loopback of its own, where the service isn't. So it shares the host's whole network. A Unix socket in the runtime directory would drop this too ([#36](https://github.com/jmarrero-bot/work-tracker-poc/issues/36)).
- **The service's runtime directory, read-only** (`$XDG_RUNTIME_DIR/workboard`), for the token, which the service replaces on every start. The data directory, with the database and the credentials, stays out of the sandbox.

Notifications, opening links and running in the background go through the desktop portals. The service has to run in the same login session, so the runtime directory is the app's; a toolbox shares it.

## Testing

`cargo test -p workboard-gnome` tests the API client against a local fake server, parses recorded answers of the service, and tests the view models (outcomes, reasons, freshness, sync and hub status, the inbox's groups) and which items get announced, without a display.

CI also runs `tests/gnome_smoke.py`: a hub and a companion on loopback with a demo connector plugin, and the app against them on a virtual X server. It checks that the app reads what both services answer with the token from the runtime directory alone, that it starts and keeps running, that a new Mine item is announced once and not again after a restart, also with the window closed and no refresh asked for, that `--background` starts it without a window, and that GTK and libadwaita warn of nothing, such as content wider than 360 pixels. It takes screenshots of each view, in the light and dark styles and at a phone's width, uploaded as the `gnome-screenshots` artifact. CI can't reach GitHub, so the bot's part of the screenshots is canned, added by a proxy in front of the companion. The `flatpak` job builds the Flatpak; the Background portal is only reached there, and isn't tested in CI.

## Credentials

The app holds no credential of its own: it reads the service's token, a file, each time. The service can keep the connections' credentials in GNOME Keyring, after `workboard credentials migrate --to keyring` with the service stopped; see the README's [Authentication and storage](../README.md#authentication-and-storage).

## Not done yet

- A GNOME Shell search provider for items and efforts ([#33](https://github.com/jmarrero-bot/work-tracker-poc/issues/33)).

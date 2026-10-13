# Workboard

A local work inbox with a Rust API/CLI, React + TypeScript dashboard, and SQLite storage. It connects related tickets, merge requests, and release advisories into an **effort** while keeping every source's status and URL.

This is an initial single-user prototype for Linux and macOS, not a hosted team service. No connection to your real accounts has been made. Nothing in it is tied to one person: to run your own hub and companion, see [docs/run-your-own.md](docs/run-your-own.md).

## Status and roadmap

This started as a Workboard prototype from a handoff, and is now built and tested by CI on every pull request. The goal is one board for internal and external work: internal items stay on the work laptop, and a hub serves the external part. The hub is meant to be reached over the tailnet, which depends on Tailscale being allowed on the work laptop; that is still to be confirmed, and the hub's design is revisited if it isn't. The app also becomes the control plane for a harness bot, while the work itself stays public on the forge. The epic is [jmarrero-forge/tracker#8](https://github.com/jmarrero-forge/tracker/issues/8); the slices below are its sub-issues, in order.

1. [Outcome semantics: closed is not delivered](https://github.com/jmarrero-bot/work-tracker-poc/issues/2) (done)
2. [Connector plugins with per-connection zones](https://github.com/jmarrero-bot/work-tracker-poc/issues/3) (done)
3. [GitHub connector (read-only) and the Mine view](https://github.com/jmarrero-bot/work-tracker-poc/issues/4) (done)
4. [Fedora Forge discovery and a Forgejo connector](https://github.com/jmarrero-bot/work-tracker-poc/issues/5) (done), and an [Errata investigation](https://github.com/jmarrero-bot/work-tracker-poc/issues/6)
5. [Scheduled sync with checkpoints and backoff](https://github.com/jmarrero-bot/work-tracker-poc/issues/7) (done)
6. [Hub mode: an external-only store on the tailnet](https://github.com/jmarrero-bot/work-tracker-poc/issues/8) (done)
7. [Bot plugin: the Workstream board, fork PRs and the heartbeat](https://github.com/jmarrero-bot/work-tracker-poc/issues/9) (done)
8. [GitHub App login: web on the hub, device flow on the companion](https://github.com/jmarrero-bot/work-tracker-poc/issues/12) (code done; a live sign-in waits for this repository's hub to get its App, see [docs/github-app.md](docs/github-app.md))
9. [Commands posted as the signed-in user to a control repository](https://github.com/jmarrero-bot/work-tracker-poc/issues/13)
10. [A bot-board backend over the app's API](https://github.com/jmarrero-bot/work-tracker-poc/issues/14)
11. [Conversations in the app](https://github.com/jmarrero-bot/work-tracker-poc/issues/15)
12. [GNOME companion app in GTK4 and libadwaita](https://github.com/jmarrero-bot/work-tracker-poc/issues/10)
13. [Publish the hub image and deploy it from atomic-setup](https://github.com/jmarrero-bot/work-tracker-poc/issues/11)

Approvals and DCO sign-offs stay on GitHub, tied to the operator's review of the exact head of a pull request. The app may show them and link to them, but never records one itself. Commands to the bot are posted as the operator with their own GitHub App user token, so the bot checks them the same way it checks their comments today.

Live Jira, internal GitLab and Errata tests aren't possible from the bot, so those connectors are tested with recorded fixtures until someone connects them from a work laptop.

GitHub is connected read-only, enforced in code: one client is the only way to GitHub, and it refuses any request that could change it (see [docs/architecture.md](docs/architecture.md)). Live tests use the bot's token, so a GitHub connection can also watch another user's activity, as far as that token can see it; that is listed as theirs, never as the bot's. The Mine view lists what waits on each connected account, and why.

Fedora Forge is forge.fedoraproject.org, which runs Forgejo, and it is connected read-only the same way. Fedora's package dist-git, where the bootc, ostree and rpm-ostree packaging pull requests are, is still Pagure on src.fedoraproject.org while Fedora moves it to Forgejo, and takes manual source links until then. [docs/connectors/fedora-forge.md](docs/connectors/fedora-forge.md) has the evidence. The bot has no Fedora account, so the Fedora Forge connector was tested live only without a token, on a public repository.

The service keeps every connection fresh on its own, with no model involved: each syncs on its own schedule, backs off when its source fails, waits out rate limits, and stops when its credential is rejected until you verify it again. Everything shows how fresh it is, and stale data is marked as such.

Every connection is in the internal or the external zone, internal unless marked external when connecting. Internal work stays on the machine that syncs it. Work leaves a machine only through the external export (`workboard export --external`), which carries external work alone.

The hub (`serve --hub`) keeps external work only, refusing anything else at every write, and is meant to be reached over the tailnet through `tailscale serve`. The companion on the work laptop keeps the internal work, pulls the hub's export, read-only, and shows both on one board; it never sends the hub anything but that read. See [docs/hub.md](docs/hub.md).

A harness bot's work is connected read-only too, through the same GitHub client: the items on its board, its open fork pull requests, and its coordinator's heartbeat, whose workers show as agent runs. Items waiting on you show in Mine as "bot waiting on you". The bot's board, tracker, heartbeat issue, login and forge organization are given when connecting, with no defaults, so anyone can connect their own bot; its heartbeat and pull requests are matched by the numeric ID of its account, not its login. "Ask the bot" is only a link: a prefilled issue in the bot's tracker, or a comment on an item, that you open in your browser and submit yourself. See [docs/connectors/bot.md](docs/connectors/bot.md).

Connectors are plugins behind one interface: Jira, GitLab, GitHub, Fedora Forge and the bot are built in, and other sources can be served by a plugin executable in any language; see [docs/plugins.md](docs/plugins.md).

## Run

Requirements: a current stable Rust toolchain, Node.js 20.19+ or 22.12+, npm, and a C compiler for bundled SQLite.

```sh
cd web
npm ci
npm run build
cd ..
cargo build

# Try illustrative data in its own directory.
./target/debug/workboard --data-dir /tmp/workboard-demo serve --demo
```

Open the pairing URL printed by the service. It works once, within 10 minutes; `workboard pair --open` opens a new one. The service listens only on `127.0.0.1:8421`. The dashboard is embedded in the executable; Node is not needed to run the built application.

In another terminal, use the same data directory and port:

```sh
./target/debug/workboard --data-dir /tmp/workboard-demo inbox
./target/debug/workboard --data-dir /tmp/workboard-demo next --json
./target/debug/workboard --data-dir /tmp/workboard-demo status bootc
./target/debug/workboard --data-dir /tmp/workboard-demo open demo-0 --print
```

For real work, start without `--demo` in a separate directory. The default is `$XDG_DATA_HOME/workboard`, or `~/.local/share/workboard`.

```sh
./target/debug/workboard serve
```

The demo has synthetic tickets and changes. Its internal URLs use `.invalid` domains; its public URLs open project home pages, not actual changes. It does not pretend to have connected accounts. Restarting with `--demo` preserves existing demo edits.

In the examples below, use `./target/debug/workboard` in place of `workboard`, or put the executable on your PATH.

## GNOME app

`workboard-gnome` is a native GNOME app (GTK 4 and libadwaita) for the companion's laptop: Mine, Next and the inbox, efforts, connections, the hub and the bot, with notifications for new Mine items, also while its window is closed. It is a client of the running service, like the CLI, and keeps no work of its own. In a toolbox with GTK 4 and libadwaita:

```sh
cargo run -p workboard-gnome                  # the service's default data directory
make -C gnome install                         # with its desktop file and icon, under ~/.local
flatpak-builder --user --install --force-clean build-dir gnome/io.github.jmarrero_bot.Workboard.yml
```

See [docs/gnome.md](docs/gnome.md) for what it shows, notifications and running in the background, and the Flatpak's permissions.

## Connect personal accounts

Use **Connections → Connect account** in the browser. Alternatively, set a token environment variable yourself, then use:

```sh
workboard connect jira --url https://jira.example.com/ --label 'Work Jira' --token-env WORKBOARD_JIRA_TOKEN
workboard connect gitlab-internal --url https://gitlab.example.com/ --label 'Internal GitLab' --token-env WORKBOARD_INTERNAL_GITLAB_TOKEN
workboard connect gitlab-public --url https://gitlab.com/ --label 'Public GitLab' --token-env WORKBOARD_PUBLIC_GITLAB_TOKEN
workboard connect github --url https://github.com/ --label GitHub --external --token-env WORKBOARD_GITHUB_TOKEN
workboard connect fedora --url https://forge.fedoraproject.org/ --label 'Fedora Forge' --external --token-env WORKBOARD_FEDORA_TOKEN
workboard connect bot --url https://github.com/ --label Bot --external --token-env WORKBOARD_GITHUB_TOKEN \
  --board orgs/ORG/NUMBER --tracker OWNER/REPO --heartbeat ISSUE --bot-login BOT_LOGIN --forge-org FORGE_ORG
workboard connections
```

Each new connection syncs right away, then every 5 minutes; you don't need to run `workboard sync`. See [Synchronization behavior](#synchronization-behavior).

Each connection is **internal** unless you add `--external` (or turn on **External** in the connection dialog). Internal work, such as Jira, internal GitLab or Errata, is never exported; external work, such as gitlab.com or Fedora Forge, is what `workboard export --external` prints, and what a hub serves to its companions. A connection's zone can't be changed later; disconnect and connect again instead. Manual source links are internal. An effort with external work is exported with that work only, and its note never; its title only once you share it (`workboard share-title EFFORT_ID`, or the switch on the effort), since a title can name internal work. Until then the export shows a neutral label. A hub's efforts always show their titles to its companions: a hub holds external work only, and a companion is trusted more than its hub. On any other service the switch only shapes `workboard export --external`, which nothing reads today, since a companion sends its hub nothing.

`--token-stdin` also accepts a token from standard input. Tokens are never accepted as CLI argument values. Each connection is verified against the instance's current-user endpoint before saving it. Multiple accounts on an instance are separate connections; no username-based cross-service identity matching is performed.

Current live connectors:

| Source | Authentication | Imported work |
| --- | --- | --- |
| Jira Data Center | Personal access token, Bearer authentication | Active issues assigned to the authenticated user |
| Internal/public GitLab | Personal API token via `PRIVATE-TOKEN`; `read_api` is recommended | Open MRs authored by you, assigned to you, or requesting your review, and the MRs and issues on your To-Do list |
| GitHub (github.com or Enterprise Server) | A token, as a bearer token; Workboard only reads, whatever it allows | Open pull requests and issues requesting your review (directly or through a team) or assigned to you, and your open pull requests, noting those with changes requested or failing checks |
| Fedora Forge (forge.fedoraproject.org, Forgejo) | An access token, `Authorization: token`; `read:user`, `read:issue` and `read:repository` are enough, and Workboard only reads | Open pull requests and issues requesting your review (directly or through a team), assigned to you or mentioning you, and your open pull requests |
| A harness bot's work on GitHub ([docs/connectors/bot.md](docs/connectors/bot.md)) | A GitHub token that can read the bot's board; Workboard only reads | The items on the bot's board, its open fork pull requests, and its heartbeat's agent runs |
| Errata through a connector plugin | Whatever the plugin uses; the token reaches it on standard input only | Whatever the plugin lists ([docs/plugins.md](docs/plugins.md)) |

Internal instances require network/VPN reachability from the machine running the service. TLS certificate verification stays enabled; there is no insecure TLS switch. Jira Cloud OAuth/API-token authentication is not implemented.

Without a plugin, Errata is supported as **manual source links**, not a live connector, and so is Fedora's Pagure dist-git (src.fedoraproject.org). A plugin is an executable named after the source in the data directory's `plugins` directory, such as `~/.local/share/workboard/plugins/errata`; it has to be owned by you and not writable by others, and runs with a minimal environment and a time limit, but as your user, so install only plugins you trust. Each connection trusts the plugin file it was connected with, by its SHA-256, and a plugin that changed doesn't run until you trust the new file (`workboard plugin trust CONNECTION_ID`). A plugin gets only `PATH`, `LANG` and a private, empty `HOME` of each run's own, plus the proxy, CA bundle and time zone variables its connection allows (`--plugin-env HTTPS_PROXY`, or `workboard plugin env`), and no other variable of the service's environment. Errata's live adapter requires the actual instance, API contract and supported authentication method. GitHub CLI, GitLab CLI and macOS keychain credential reuse are planned; this version uses explicitly supplied tokens, kept in files or GNOME Keyring (see [Authentication and storage](#authentication-and-storage)).

## Mine

`workboard mine` (or the Mine tab) lists what waits on the account behind each connection, and why: review requested, team review requested, assigned, mentioned, or your own change needs action because of requested changes or failing checks; GitLab's other To-Do items show as on the to-do list, and the bot's items it marked Needs human or Draft as "bot waiting on you". Each row shows the source's status, the outcome, the zone and the link, and appears once however many listings or connections found it, with each account it waits on and that account's reasons. Internal rows show only here, on this machine; the external export carries the reasons of external items only.

A GitHub connection can also watch another user: their review requests, assignments and open pull requests and issues that the connection's token can see, private repositories included. It is a setting of its own, for a token that isn't theirs, such as the bot's:

```sh
workboard connect github --url https://github.com/ --external --token-env GH_TOKEN --watch-user THEIR-LOGIN
workboard watch CONNECTION_ID THEIR-LOGIN     # or --stop
```

What watching finds is listed under **Watching**, with the watched login, and is never shown as yours, in the inbox, or exported. If a watch fails, for example because the login was renamed, the account's own work still syncs; Watching keeps the last list and shows the error, and `workboard sync` exits unsuccessfully.

## Efforts and source links

- Select inbox items and click **New effort** to group them.
- Open an item to change its effort or follow its source URL.
- The Efforts view shows delivery progress and lets you attach ungrouped items.
- Add advisory or other work URLs with **Add source link**. They are clearly identified as manual.
- Search and filter by service or attention category. Delivered and abandoned work remains available in its own filter.

Closed is not delivered. Every item keeps its source's own status and URL, next to an outcome Workboard derives from it and the reason it was chosen:

- **delivered**: a merged merge request, or a Jira issue resolved as Done, Fixed or a similar resolution;
- **closed without delivery**: closed unmerged, or resolved as Won't Do, Duplicate, Not a Bug and the like;
- **abandoned**: your decision, kept in Workboard;
- **open**, in progress or waiting;
- **unknown**: an item the source no longer shows to you.

Effort progress counts only delivered items. Closed-without-delivery and unknown items need attention instead: once you have decided, mark the item abandoned, and link any rework to the effort. Abandoned items are no longer refreshed, so abandoning open or delivered work takes `--force`. Abandoned items leave the effort's total. A Jira issue in the Done category without a resolution counts as delivered. Finished Jira issues synced before outcomes existed are unknown until the next sync fetches their resolution. Manual links take an explicit outcome; their status text never implies one.

CLI equivalents:

```sh
workboard inbox --json
workboard mine --json                          # what waits on you, and what watching found
workboard effort 'Ship a fix' --note 'Upstream, downstream, release' --item ITEM_ID
workboard link ITEM_ID --effort EFFORT_ID
workboard link ITEM_ID                         # ungroup
workboard add --provider errata --key ADVISORY --title 'Ship advisory' --url https://errata.example.com/advisory/123 --status 'Needs validation' --action next
workboard update ITEM_ID --status Released --outcome delivered
workboard abandon ITEM_ID                      # synced, closed without delivery or unknown; --undo reverts
workboard share-title EFFORT_ID                # let this effort's title leave in the export; --undo stops it
workboard export --external                    # the external zone only, as JSON
workboard bot                                  # each bot's heartbeat, agent runs and board counts
workboard ask [ITEM_ID] [--comment]            # open a link that asks the bot; you submit it
workboard disconnect CONNECTION_ID
```

Source statuses are never changed through this app. Manual links are snapshots; edit their status and outcome in Workboard when the source changes. The service syncs connected sources on schedule, and the page refreshes what the service has every 30 seconds.

## Authentication and storage

By default, credentials are kept in a private local directory, **not encrypted at rest**. On Unix, the directory is mode `0700`; credential files are mode `0600`, and so are the database, the copies made before upgrades, and the write-ahead log and shared memory SQLite keeps beside them, which hold recent rows. The service creates the database `0600` before SQLite opens it, and at start makes any of those files an earlier version left readable by others private again, saying which. Each of them has to be a plain file of yours in the data directory, never a symbolic link, which could put the database where others can read it; otherwise the service refuses to start. The copy made before an upgrade is written under a temporary name and put in place only once it is complete. Only connection metadata is stored in SQLite; tokens are separate files named by a validated UUID. Disconnect removes the credential and its source's cached items. Private filesystem permissions do not protect against programs running as the same user.

In a desktop session on Linux, a data directory can keep them in the Secret Service instead, which GNOME Keyring provides: encrypted at rest, and locked with the session. The connections' tokens and the hub's companion token are kept there, each labeled "Workboard credential ID", and no credential is ever written to a file: `secrets/` holds only `in-secret-service.json`, which names the keyring's store for this data directory, so the credentials are found if the directory moves and its path stays out of the keyring. If the keyring can't be reached when the service starts, the service fails closed: every connection's sync fails, nothing falls back to files, and nothing asks for a password; restart the service once the keyring is there, and the connections sync again on their own, without being verified again: each once its backoff is over, which can take up to an hour, or right away with **Sync sources** (`workboard sync`). The service token the CLI and the GNOME app read stays a file, as it must. A hub always keeps files: it runs without a desktop session.

Moving the credentials is a command of its own, run with the service stopped:

```sh
workboard credentials migrate --to keyring    # or --to files
```

It copies every credential into the store given and reads each copy back, then makes that the store, and only then drops the old copies; each step is made durable on disk before the next one counts on it, so a power cut doesn't lose a credential either. Deleted credential files are unlinked, not scrubbed: if what was on the disk worries you, rotate those tokens. Cut short anywhere, nothing is lost: the marker file says which store is the authority meanwhile, the service refuses to start, and running the command again, to either store, finishes it. Starting the service never moves anything. The service and the command lock the data directory, so neither runs while the other does, and no two services share a data directory.

Every start deletes the partial files a write cut short left in `secrets/`, since one can hold a token, and drops credentials staged by a verify that never completed. Once the database has been read, it also drops credentials no connection or hub link uses, from the store in use, which catches a removal cut short; if anything stored can't be read, nothing is dropped. A copy of a data directory shares its keyring store, so don't run a copy kept by the keyring next to the original ([#41](https://github.com/jmarrero-bot/work-tracker-poc/issues/41)).

The keyring is unlocked when the service starts, which may ask for your password. After that the service never asks, since a sync would wait on you meanwhile. A keyring locked later fails each sync that needs it, which backs off and syncs again on its own once you unlock it, within its backoff of up to an hour, or right away with **Sync sources**. Every call to the Secret Service has a time limit, so a keyring that doesn't answer fails syncs rather than holding the service up.

A new version may upgrade the database when the service starts. Upgrades are one-way, so the service first copies `work.sqlite` to `work.vN.sqlite` in the same directory, N being the old schema version. To go back to an older version, stop the service and restore that copy as `work.sqlite`.

The web client never receives provider credentials in API responses. Credentials entered in the connection dialog travel only to the local backend, are cleared from the form after saving/closing, and are not put in browser storage. The CLI calls the same API through the service's private Unix socket: `$XDG_RUNTIME_DIR/workboard/service-PORT.sock` in a user session, or `service.sock` in the data directory without one (mode `0600`, in a `0700` directory), which the data directory names in `socket-path`. The service takes a connection there only from its own user, by the kernel's peer credentials (`SO_PEERCRED`), and needs no token on it: a browser can't reach a socket, and a proxy such as `tailscale serve` would connect as its own user, root, and be refused. The data directory also names the service that listens there by a random ID, which its answers carry. The CLI asks for it on each connection before it sends a request there, and every request names the service it is for, which another service refuses before acting on it, so a request never reaches another data directory's service that took over the port of one that was killed. A socket a live service listens on is never taken over. So does the GNOME app, whose Flatpak reaches the runtime directory's socket without access to the data directory, which holds the database and the credentials; reading the data directory's name for the socket where it can, it checks the service on each connection the same way before it sends a request. HTTP clients on loopback use the application credential, `service.token` in the data directory, instead; the service no longer puts a copy of it in the runtime directory, and removes one an earlier version left.

Each service start rotates the local application credential, which the CLI uses. On a local service, a browser pairs with a one-time code, which only the CLI and the GNOME app can get (`workboard pair`), and which works once, within 10 minutes. The page exchanges it for an HttpOnly, SameSite=Strict cookie holding a random session ID, never the application token, so nothing in a pairing URL or the browser's history authorizes anything afterwards. The application token itself is taken only on loopback, without a proxy's headers, and never from a request a browser made, which says where it came from (`Origin` or `Sec-Fetch-Site`). While a pairing URL is unused, it is in the browser's command line when `workboard pair --open` or the GNOME app opens it, where other users of the machine can see it: on a shared machine, print it and paste it instead. A code that comes back after it was used, as from someone who read it too, ends the session it opened, and the browser and the service's log say so. A paired session ends after 12 hours unused, and a week after pairing in any case. A hub signs people in with GitHub instead, through a GitHub App: by numeric user ID against an allowlist, with a session cookie holding a random ID, and on the companion with GitHub's device flow, which gives it a credential that only reads the hub's export ([docs/github-app.md](docs/github-app.md)). Until the App is set up, a hub pairs browsers only when started with `--pairing`; a hub also keeps a companion token that only reads its external export (see [docs/hub.md](docs/hub.md)). API routes require authentication, check the request host, and require a matching Origin for cookie-authenticated mutations. No CORS access is enabled. Provider API requests use HTTPS, do not follow redirects, and do not expose remote error bodies or tokens. Source hyperlinks are restricted to HTTP(S).

The service listens on loopback only. A hub is reached on the tailnet through `tailscale serve`, and a local service not at all from elsewhere. Multi-user ownership/ACLs, encrypted server credential storage, and scoped agent access are future work. Signing in to a hub is the one time Workboard sends GitHub anything but reads: the OAuth code exchange and device flow, through a client that can send nothing else. Do not expose either through a public reverse proxy, or Tailscale Funnel; requests that came through Funnel are refused. Use one service per data directory.

Credentials belong to the service, not to disposable jobs. No existing bot token, model credential or host login has been imported. No agents are executed by this prototype.

## Synchronization behavior

The service syncs each connection on its own schedule: right after it is connected, then every 5 minutes, or as often as `workboard serve --sync-interval` (such as `15m`) or the connection's own interval says. Each wait varies by up to 10% at random, so connections drift apart. Connections sync side by side and independently, so a slow or failing source never holds up the others, and a connection never syncs alongside itself. No model is involved.

```sh
workboard connect github --url https://github.com/ --external --token-env WORKBOARD_GITHUB_TOKEN --sync-interval 15m
workboard schedule CONNECTION_ID 1h            # or --default
workboard sync                                 # all now; or `workboard sync CONNECTION_ID`
workboard verify CONNECTION_ID                 # check the credential again; --token-stdin gives a new one
```

`workboard sync`, **Sync sources** in the page, and `POST /api/sync` still sync on request. If a scheduled sync of a connection is running, the request joins it and reports its result, rather than starting a second one.

When a sync fails, the next one backs off: a minute after the first failure, doubling after each next one, up to an hour. A source that asks to wait (GitHub's rate limits, or Fedora Forge's `retry-after`) gets no request with that token until the wait is over, and the next sync is not due before then. A rejected credential (`auth`) stops the connection's scheduled syncs, since retrying can't fix it: `workboard connections` and the page say it needs you. Fix the credential, then run `workboard verify CONNECTION_ID` (with `--token-stdin` or `--token-env` to give it a new token for the same account), or click **Verify again**; its scheduled syncs resume. An explicit sync still runs, and one that succeeds resumes them too.

Each connection keeps its sync state: its last success, its last attempt, when its next sync is due, its failures in a row, and its last error and the kind of that error. What a sync found and that state are saved together, in one transaction, only after the sync has processed everything; nothing is saved before. A sync that fails keeps the previous data and records only its failure; one cut short because the service stopped saves nothing, so the previous data and state stay, and the connection is still due when the service starts again: nothing is skipped. The state survives restarts; GitHub's answers kept for conditional requests don't (see `docs/architecture.md`).

Everything shows how fresh it is. A connection is **stale** when it never synced, or when its last success is more than three of its intervals old, which one slow or failed sync doesn't reach. Each synced item carries its connection's last success and stale flag, in the API, `inbox --json` and `mine --json`; what watching found carries the last success of its own, since a watch can keep failing while the account syncs. `workboard connections`, `inbox` and `mine` and the page mark stale data, so it is never shown as current. The page also ages what it shows by each connection's threshold, and once it can't reach the service it says so and marks everything synced stale. The external export carries each connection's last success and stale flag, and the kind of its error, never the message.

Connectors paginate inbox results, and fail rather than save part of a list. Items that were tracked before but no longer appear in the open inbox are fetched individually to detect completion or merge. Sync preserves manual effort assignments. A source failure keeps its previous cache, records a visible error, and does not prevent other sources from syncing. `workboard sync` exits unsuccessfully if any source sync fails. Fedora Forge sends no ETags. GitHub requests are conditional where GitHub returns an ETag, and once GitHub says a rate limit is used up, no more requests count against it until it resets. A wait a source asks for is honored for at most an hour. Each GitHub sync result lists the requests it sent by method, and those it refused.

If a tracked item becomes inaccessible (HTTP 403 or 404), it is marked unknown with its last known status, never complete, and fetched again on the next sync. Other failures fail that source's sync and keep its previous cache. Manual links do not refresh automatically. GitLab pipelines, Jira release fields/blockers, automatic relationship suggestions, live Errata readiness, and hosted/team mode are not implemented yet.

## Verify

```sh
cd web && npm run build && cd ..
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 tests/smoke.py target/debug/workboard
python3 tests/hub_smoke.py target/debug/workboard
# Credentials in the Secret Service, with one on the session bus:
cargo test secret_service -- --ignored
# The GNOME app, with GTK 4 and libadwaita installed:
cargo clippy -p workboard-gnome --all-targets -- -D warnings
cargo test -p workboard-gnome
cargo build -p workboard-gnome
dbus-run-session -- python3 tests/gnome_smoke.py target/debug/workboard target/debug/workboard-gnome screenshots
```

The smoke test creates and removes its own temporary data directory. It checks unauthorized access, browser pairing, cross-origin mutations, CLI access, outcomes and effort progress, persistent grouping, source-link validation, a plugin connection in each zone, the external export and the Mine view, scheduled sync with backoff, a stopped connection and freshness, sync state across a restart, and secret file permissions. The hub smoke test runs a hub and a companion as two processes on loopback, the companion pulling through a logging proxy, and checks that the companion shows both parts, that it sent the hub only GETs of the export, that nothing of its internal work reaches any hub answer or the hub's data directory, and that the hub's work goes stale on the companion once the hub stops. The scheduler's own tests use a clock they control and fake connectors, so they never wait. GitHub and Fedora Forge are tested against recorded answers in `cargo test`, since CI has no token for either. An ignored test reads a public repository on the real Fedora Forge without a token, and prints counts: `cargo test -- --ignored --nocapture live_fedora_forge`, or `workboard-tests --ignored --nocapture live_fedora_forge` with the test binary CI uploads next to `workboard`.

The GNOME smoke test needs Xvfb, ImageMagick, xdotool, PyGObject and python-xlib; it runs the app against a hub and a companion, and saves screenshots of its views (see [docs/gnome.md](docs/gnome.md#testing)).

See `docs/architecture.md` for the next implementation boundaries.

## Container image

`Containerfile` builds the hub's release image: the web UI and a release build of the service, on a distroless base, as an unprivileged user, with its data directory as a volume. CI builds it and runs `tests/image_smoke.py` against it on every pull request, and publishes it to `ghcr.io/jmarrero-bot/work-tracker-poc` on `v*` tags. [docs/hub.md](docs/hub.md#run-the-hub-from-its-image) says how to run it, and [docs/release.md](docs/release.md) how to release it and move a deployment's pin.

# Workboard

A local work inbox with a Rust API/CLI, React + TypeScript dashboard, and SQLite storage. It connects related tickets, merge requests, and release advisories into an **effort** while keeping every source's status and URL.

This is an initial single-user prototype for Linux and macOS, not a hosted team service. No connection to your real accounts has been made.

## Status and roadmap

This is jmarrero's Workboard prototype from his handoff, now built and tested by CI on every pull request. The goal is one board for internal and external work: internal items stay on the work laptop, and a hub serves the external part. The hub is meant to be reached over the tailnet, which depends on Tailscale being allowed on the work laptop; that is still to be confirmed, and the hub's design is revisited if it isn't. The app also becomes the control plane for jmarrero-bot, while the work itself stays public on the forge. The epic is [jmarrero-forge/tracker#8](https://github.com/jmarrero-forge/tracker/issues/8); the slices below are its sub-issues, in order.

1. [Outcome semantics: closed is not delivered](https://github.com/jmarrero-bot/work-tracker-poc/issues/2)
2. [Connector plugins with per-connection zones](https://github.com/jmarrero-bot/work-tracker-poc/issues/3)
3. [GitHub connector (read-only) and the Mine view](https://github.com/jmarrero-bot/work-tracker-poc/issues/4)
4. [Fedora Forge discovery and a Forgejo connector](https://github.com/jmarrero-bot/work-tracker-poc/issues/5), and an [Errata investigation](https://github.com/jmarrero-bot/work-tracker-poc/issues/6)
5. [Scheduled sync with checkpoints and backoff](https://github.com/jmarrero-bot/work-tracker-poc/issues/7)
6. [Hub mode: an external-only store on the tailnet](https://github.com/jmarrero-bot/work-tracker-poc/issues/8)
7. [Bot plugin: the Workstream board, fork PRs and the heartbeat](https://github.com/jmarrero-bot/work-tracker-poc/issues/9)
8. [GitHub App login: web on the hub, device flow on the companion](https://github.com/jmarrero-bot/work-tracker-poc/issues/12)
9. [Commands posted as jmarrero to a control repository](https://github.com/jmarrero-bot/work-tracker-poc/issues/13)
10. [A bot-board backend over the app's API](https://github.com/jmarrero-bot/work-tracker-poc/issues/14)
11. [Conversations in the app](https://github.com/jmarrero-bot/work-tracker-poc/issues/15)
12. [GNOME companion app in GTK4 and libadwaita](https://github.com/jmarrero-bot/work-tracker-poc/issues/10)
13. [Publish the hub image and deploy it from atomic-setup](https://github.com/jmarrero-bot/work-tracker-poc/issues/11)

Approvals and DCO sign-offs stay on GitHub, tied to jmarrero's review of the exact head of a pull request. The app may show them and link to them, but never records one itself. Commands to the bot are posted as jmarrero with his own GitHub App user token, so the bot checks them the same way it checks his comments today.

Live Jira, internal GitLab and Errata tests aren't possible from the bot, so those connectors are tested with recorded fixtures until jmarrero connects them from his laptop.

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

Open the private pairing URL printed by the service. The URL authorizes your browser; keep it private. The service listens only on `127.0.0.1:8421`. The dashboard is embedded in the executable; Node is not needed to run the built application.

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

## Connect personal accounts

Use **Connections → Connect account** in the browser. Alternatively, set a token environment variable yourself, then use:

```sh
workboard connect jira --url https://jira.example.com/ --label 'Work Jira' --token-env WORKBOARD_JIRA_TOKEN
workboard connect gitlab-internal --url https://gitlab.example.com/ --label 'Internal GitLab' --token-env WORKBOARD_INTERNAL_GITLAB_TOKEN
workboard connect gitlab-public --url https://gitlab.com/ --label 'Public GitLab' --token-env WORKBOARD_PUBLIC_GITLAB_TOKEN
workboard connections
workboard sync
```

`--token-stdin` also accepts a token from standard input. Tokens are never accepted as CLI argument values. Each connection is verified against the instance's current-user endpoint before saving it. Multiple accounts on an instance are separate connections; no username-based cross-service identity matching is performed.

Current live connectors:

| Source | Authentication | Imported work |
| --- | --- | --- |
| Jira Data Center | Personal access token, Bearer authentication | Active issues assigned to the authenticated user |
| Internal/public GitLab | Personal API token via `PRIVATE-TOKEN`; `read_api` is recommended | Open MRs authored by you, assigned to you, or requesting your review |

Internal instances require network/VPN reachability from the machine running the service. TLS certificate verification stays enabled; there is no insecure TLS switch. Jira Cloud OAuth/API-token authentication is not implemented.

GitHub, Fedora Forge and Errata are supported as **manual source links**, not live connectors. Errata's live adapter requires the actual instance, API contract and supported authentication method. GitHub CLI, GitLab CLI and OS keychain credential reuse are planned; this version uses explicitly supplied tokens.

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
workboard effort 'Ship a fix' --note 'Upstream, downstream, release' --item ITEM_ID
workboard link ITEM_ID --effort EFFORT_ID
workboard link ITEM_ID                         # ungroup
workboard add --provider errata --key ADVISORY --title 'Ship advisory' --url https://errata.example.com/advisory/123 --status 'Needs validation' --action next
workboard update ITEM_ID --status Released --outcome delivered
workboard abandon ITEM_ID                      # synced, closed without delivery or unknown; --undo reverts
workboard disconnect CONNECTION_ID
```

Source statuses are never changed through this app. Manual links are snapshots; edit their status and outcome in Workboard when the source changes. Sync is explicit: the page refreshes cached data every 30 seconds, but does not poll external services automatically.

## Authentication and storage

The initial credential backend is a private local directory, **not encrypted at rest**. On Unix, the directory is mode `0700`; credential files and database are mode `0600`. Only connection metadata is stored in SQLite; tokens are separate files named by a validated UUID. Disconnect removes the credential and its source's cached items. Private filesystem permissions do not protect against programs running as the same user.

A new version may upgrade the database when the service starts. Upgrades are one-way, so the service first copies `work.sqlite` to `work.vN.sqlite` in the same directory, N being the old schema version. To go back to an older version, stop the service and restore that copy as `work.sqlite`.

The web client never receives provider credentials in API responses. Credentials entered in the connection dialog travel only to the local backend, are cleared from the form after saving/closing, and are not put in browser storage. The CLI calls the same API and reads a private service credential from the same data directory.

Each service start rotates the local application credential. Pairing establishes an HttpOnly, SameSite=Strict browser cookie. API routes require authentication, check the request host, and require a matching Origin for cookie-authenticated mutations. No CORS access is enabled. Provider API requests use HTTPS, do not follow redirects, and do not expose remote error bodies or tokens. Source hyperlinks are restricted to HTTP(S).

The service is intentionally local-only. Multi-user ownership/ACLs, hosted OAuth/OIDC login, encrypted server credential storage, and scoped agent access are future work. Do not expose this server through a public reverse proxy. Use one service per data directory.

The credential handling follows the separation established in `jmarrero/atomic-setup`: credentials belong to the coordinator/service, not disposable jobs. No existing bot token, model credential or host login has been imported. No agents are executed by this prototype.

## Synchronization behavior

Connectors paginate inbox results. Items that were tracked before but no longer appear in the open inbox are fetched individually to detect completion or merge. Sync preserves manual effort assignments and commits each source's fetched item set transactionally. A source failure keeps its previous cache, records a visible error, and does not prevent other sources from syncing. The CLI exits unsuccessfully if any source sync fails. Rate limits are reported without an automatic retry loop.

If a tracked item becomes inaccessible (HTTP 403 or 404), it is marked unknown with its last known status, never complete, and fetched again on the next sync. Other failures fail that source's sync and keep its previous cache. Manual links do not refresh automatically. GitLab pipelines, Jira release fields/blockers, automatic relationship suggestions, live Errata readiness, and hosted/team mode are not implemented yet.

## Verify

```sh
cd web && npm run build && cd ..
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 tests/smoke.py target/debug/workboard
```

The smoke test creates and removes its own temporary data directory. It checks unauthorized access, browser pairing, cross-origin mutations, CLI access, outcomes and effort progress, persistent grouping, source-link validation and secret file permissions.

See `docs/architecture.md` for the next implementation boundaries.

## Container recipe

`Containerfile` is supplied as a starting recipe and has not been built in this environment. Because the server binds loopback only, a container on Linux needs host networking to reach it from the same host browser. Mount a private, writable `/data` directory and run as its owner. The native executable is the recommended first run on Linux or macOS.

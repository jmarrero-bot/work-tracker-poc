# Run your own hub and companion

Nothing in Workboard belongs to one person: logins, organizations, boards, repositories, tailnet names and app IDs are all configuration. This guide sets up a hub and a companion of your own, from nothing. [hub.md](hub.md) explains the design and the trust model behind each step.

You need:

- **a machine that is always on, for the hub**, with Linux and either podman or a Rust toolchain to build Workboard (see the [README](../README.md#run)). It syncs your external work: GitHub, Fedora Forge, a bot's board.
- **your work laptop, for the companion**, which syncs internal work (Jira, internal GitLab, Errata) and shows both on one board;
- **a tailnet** that both are on, with [Tailscale](https://tailscale.com/) and HTTPS certificates enabled, since the hub is reached through `tailscale serve`;
- **a GitHub account**, which signs in to the hub.

In the examples, `YOUR-LOGIN` is your GitHub login and `hub.example.ts.net` your hub's tailnet name; put yours in their place.

## What to create

### A GitHub App, to sign in

People sign in to the hub with GitHub, through a GitHub App you create under your own account. [github-app.md](github-app.md#create-the-app) lists every setting; in short:

- name it anything free, such as `workboard-YOUR-LOGIN`;
- set the callback URL to `https://hub.example.ts.net/auth/github/callback`;
- enable the device flow, which the companion signs in with;
- give it no permissions, and no webhook.

Note its **client ID**, generate a **client secret**, and find the numeric user ID of each person the hub lets in:

```sh
gh api users/YOUR-LOGIN --jq .id
```

The hub allows users by that ID, never by login, since a login can be renamed and taken by someone else.

### A read-only token, for the hub's sources

The hub reads GitHub with a token of yours. Workboard only ever reads, enforced in code ([architecture.md](architecture.md)), whatever the token allows, but give it no more than it needs:

- a **fine-grained token**, with read-only Metadata, Issues and Pull requests on the repositories you work in, if all of them have one owner;
- otherwise a **classic token**, with `read:org`, `read:project` if you connect a bot's board, and `repo` only if your work includes private repositories.

Fedora Forge takes an access token with `read:user`, `read:issue` and `read:repository`.

### Tailnet `serve`

On the hub's machine, find its tailnet name, and serve the hub's loopback port over HTTPS on the tailnet:

```sh
tailscale status --json | jq -r '.Self.DNSName | rtrimstr(".")'   # hub.example.ts.net
tailscale serve --bg --https=443 http://127.0.0.1:8421
```

Use this HTTPS mode only, never Funnel or a TCP forward, and serve nothing else under that name ([hub.md](hub.md#trust-model) says why).

## Every value to set

| Value | Where it goes | Example |
| --- | --- | --- |
| The hub's tailnet name | `serve --hub --allowed-host`, the App's callback, the companion's `hub login` | `hub.example.ts.net` |
| The App's client ID | `serve --github-app-client-id` | `Iv23li...` |
| The App's callback URL | `serve --github-app-callback`, and the App's settings | `https://hub.example.ts.net/auth/github/callback` |
| The App's client secret | `workboard github-app set-secret --token-stdin`, once, into the hub's data directory | |
| Who may sign in | `serve --github-user`, once per numeric user ID | `--github-user 1234567` |
| The hub's data directory | `--data-dir`, on every command for the hub | `~/.local/share/workboard-hub` |
| The sources' tokens | `connect ... --token-env NAME` or `--token-stdin`, never an argument | `--token-env GH_TOKEN` |
| Another user to watch (optional) | `connect github --watch-user` | `--watch-user THEIR-LOGIN` |
| A bot's board, tracker, heartbeat, login and forks (optional) | `connect bot` (below) | `--board orgs/example-org/1` |
| The hub's address, on the companion | `workboard hub login` | `https://hub.example.ts.net/` |
| The GNOME app's ID (optional) | `make -C gnome APP_ID=...`, or the Flatpak manifest's `id` (below) | `org.example.Workboard` |
| The GNOME app's developer ID and homepage (optional) | `make -C gnome DEVELOPER_ID=... HOMEPAGE=...` (below) | `HOMEPAGE=https://github.com/YOUR-LOGIN/work-tracker-poc` |
| The image's name (optional) | the repository variable `IMAGE_NAME` of your fork (below) | `your-login/workboard` |

Nothing else has a default that names anyone.

## Set up the hub

In a data directory of its own, keep the App's secret, start the hub, and connect its sources, each as external:

```sh
export WB="workboard --data-dir $HOME/.local/share/workboard-hub"
$WB github-app set-secret --token-stdin    # paste the client secret
$WB serve --hub --allowed-host hub.example.ts.net \
  --github-app-client-id Iv23li... \
  --github-app-callback https://hub.example.ts.net/auth/github/callback \
  --github-user 1234567
```

In another terminal, with the same data directory:

```sh
export WB="workboard --data-dir $HOME/.local/share/workboard-hub"
export GH_TOKEN=...       # the read-only token
export FEDORA_TOKEN=...   # a Fedora Forge token, if you connect it
$WB connect github --url https://github.com/ --external --token-env GH_TOKEN
$WB connect fedora --url https://forge.fedoraproject.org/ --external --token-env FEDORA_TOKEN
```

Open `https://hub.example.ts.net/` and sign in with GitHub. To run the hub from its container image instead, with a quadlet or `podman run`, see [hub.md](hub.md#run-the-hub-from-its-image).

### The bot connector: your own board, or none

The `bot` connector shows what a harness bot works on: the items on its Projects board, its open pull requests in its forge organization, and the agent runs its coordinator's heartbeat lists ([connectors/bot.md](connectors/bot.md)). Nothing about the bot is assumed, so point it at yours:

```sh
$WB connect bot --url https://github.com/ --external --token-env GH_TOKEN \
  --board orgs/example-org/1 --tracker example-org/tracker --heartbeat 1 \
  --bot-login example-bot --forge-org example-org
```

- `--board`: the bot's Projects (v2) board, as `orgs/OWNER/NUMBER`, `users/OWNER/NUMBER` or its address;
- `--tracker`: the repository where you ask the bot for work, `OWNER/REPO`; "Ask the bot" opens a new issue there, which you submit yourself;
- `--heartbeat`: the tracker issue whose comment by the bot holds its heartbeat (`<!-- bot-heartbeat v1 -->`). A bot that publishes none still shows its board and pull requests;
- `--bot-login`: the bot's GitHub login;
- `--forge-org`: the organization that holds the bot's forks.

**No bot?** Skip this. Nothing else needs it; the Bot view just stays empty.

The hub's bot also shows on the companion, read-only, with Ask links the companion builds itself ([connectors/bot.md](connectors/bot.md#on-a-companion)).

## Set up the companion

On the laptop, run Workboard as usual, connect the internal sources, and sign in to the hub with GitHub's device flow:

```sh
workboard serve
workboard connect jira --url https://jira.example.com/ --label 'Work Jira' --token-env WORKBOARD_JIRA_TOKEN
workboard hub login https://hub.example.ts.net/   # enter the code it shows on github.com
workboard hub show
```

Every connection is internal unless you say `--external`, and internal work never leaves the laptop. Enter only codes your own `hub login` printed.

## Build the GNOME app with your own app ID

The app's ID is its D-Bus name, and names its desktop file, metainfo and icons. This repository's is `io.github.jmarrero_bot.Workboard`; a build of your own should take its own, in reverse DNS of a domain or account you control, so it can run next to another build and you can publish it.

Installed under `~/.local`, as in a toolbox:

```sh
make -C gnome install APP_ID=org.example.Workboard
```

`APP_ID` sets `WORKBOARD_APP_ID` for the build, which the app takes its ID from, and installs the desktop file, metainfo and icons under it. The build refuses an ID that GLib wouldn't take. `cargo build -p workboard-gnome` takes `WORKBOARD_APP_ID` from the environment the same way.

The metainfo names a developer and a homepage too. The developer ID is your app ID without its last element (`org.example` above) unless you set `DEVELOPER_ID`; the homepage, which the About dialog links to as well (`WORKBOARD_HOMEPAGE`), stays this repository's unless you set `HOMEPAGE` to an `https://` address, such as your fork's:

```sh
make -C gnome install APP_ID=org.example.Workboard HOMEPAGE=https://github.com/YOUR-LOGIN/work-tracker-poc
```

As a Flatpak, copy the manifest under your ID, change its `id`, and build that copy; the build takes the ID from there (add `HOMEPAGE=...` to the copy's `make` line for your homepage):

```sh
sed 's/^id: .*/id: org.example.Workboard/' gnome/io.github.jmarrero_bot.Workboard.yml > gnome/org.example.Workboard.yml
flatpak-builder --user --install --force-clean build-dir gnome/org.example.Workboard.yml
flatpak run org.example.Workboard
```

`tests/gnome_smoke.py` takes the ID from `WORKBOARD_APP_ID` too.

## Publish your own image (optional)

To run a hub from an image you built, fork this repository. Its [release workflow](../.github/workflows/release.yml) pushes each `v*` tag to `ghcr.io/OWNER/REPO` of the fork, or to `ghcr.io/` and the repository variable `IMAGE_NAME` (lowercase, under the fork owner's namespace, which is where the workflow's token can push). Make the package public, so the hub's machine pulls it without credentials, and pin its digest, as [release.md](release.md) describes.

## What stays this repository's

A few names identify where this code lives, not a person: its repository, its image, its app ID, and the epic it is tracked in. Each has an override above. `cargo test` fails if a name of one person's setup appears anywhere else (`tests/personal_names.rs`), so a new example can't bring one back.

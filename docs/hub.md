# Hub and companion

One board, in two parts:

- **The hub** runs on a machine that is always on, such as the one a harness bot runs on, and is reachable from anywhere on your tailnet. It holds and syncs external sources only (GitHub, Fedora Forge, and the bot's board, fork pull requests and heartbeat; see [connectors/bot.md](connectors/bot.md)), and serves the web UI.
- **The companion** runs on your work laptop, in GNOME or a toolbox there, with VPN access. It holds the internal sources (Jira, internal GitLab, Errata), pulls the hub's external work, and shows one board with both.

Internal work never leaves the companion. Away from the laptop, the hub shows external work only, with no placeholder for anything internal: it never had any.

Both are the same `workboard` binary. The hub is `serve --hub`; the companion is an ordinary service with a hub set.

## Set up the hub

On the hub's machine, in a data directory of its own:

```sh
workboard --data-dir ~/.local/share/workboard-hub serve --hub --allowed-host hub.example.ts.net \
  --github-app-client-id Iv23li... --github-app-callback https://hub.example.ts.net/auth/github/callback \
  --github-user 1234567
```

People sign in to the hub with GitHub, through a GitHub App that you create; [github-app.md](github-app.md) lists its settings and how to give the hub its client secret. Until the App exists, start the hub with `--pairing` instead of the three `--github-...` options, to pair browsers through the URL it prints, as a local service does. Pairing is off once the App is configured.

The hub listens on `127.0.0.1:8421` only, like any Workboard service. [`tailscale serve`](https://tailscale.com/kb/1312/serve) is the intended front: it serves the hub over HTTPS on the tailnet, with a certificate for the machine's tailnet name, and proxies to the loopback port.

```sh
tailscale serve --bg --https=443 http://127.0.0.1:8421
```

Use this HTTPS mode only, never `--tcp` or `--tls-terminated-tcp`. The hub relies on what the HTTPS proxy does to each request: it terminates TLS, so the hub's cookies can be Secure; it sets `X-Forwarded-For` to the peer's tailnet address, which the hub's sign-in rate limits count by; and it adds the headers the hub refuses the application token with. A TCP forward does none of that.

Give that name to `--allowed-host`, once per name the hub is reached by. The hub accepts a request only if its Host is its loopback address or one of those names, and `tailscale serve` passes on the name the browser used. A browser's changes must carry the HTTPS origin of the name the request came by, so cross-site requests are refused behind the proxy as on loopback. Use `tailscale serve`, never Funnel, which would put the hub on the internet: Funnel marks each request it brings in with a `Tailscale-Funnel-Request` header, which `tailscale serve` strips from requests that arrive with one, and the hub refuses every request that has it, the page included. Only a hub takes `--allowed-host`: a local service may hold internal work, and stays reachable on loopback alone.

Then connect its sources, always as external, with the same CLI on the same data directory:

```sh
export WB="workboard --data-dir $HOME/.local/share/workboard-hub"
$WB connect github --url https://github.com/ --external --token-env GH_TOKEN --watch-user YOUR-LOGIN
$WB connect fedora --url https://forge.fedoraproject.org/ --external --token-env FEDORA_TOKEN
$WB connect bot --url https://github.com/ --external --token-env GH_TOKEN --board orgs/ORG/NUMBER \
  --tracker OWNER/REPO --heartbeat ISSUE --bot-login BOT_LOGIN --forge-org FORGE_ORG
```

Open `https://hub.example.ts.net/` and sign in with GitHub (or, with `--pairing`, open a pairing URL under its tailnet name from `podman exec workboard-hub workboard pair`). Without the App, print the companion token for the laptop:

```sh
$WB companion-token
```

Upgrade the companions before the hub. A companion checks the hub's export whole, and refuses all of it if it has a source or a reason it doesn't know, such as the bot connector's `bot` items and their `bot_waiting` reason, which an older companion doesn't. Fields it doesn't know, such as a bot's board fields, it ignores, and a newer companion takes an older hub's export without them.

## Run the hub from its image

CI publishes the hub's image, `ghcr.io/jmarrero-bot/work-tracker-poc`, for each `v*` tag ([release.md](release.md)); a build of your own publishes its own ([run-your-own.md](run-your-own.md)). Run it from a quadlet, or any container runner, that pins the image by digest and holds only the wiring: the volume, the secrets and the port. The hub's code and its image stay here, and moving the pin is a change you review in your own deployment.

The image runs `workboard serve --hub` as an unprivileged user, from a distroless base with no shell. Its data directory is the volume at `/var/lib/workboard`, which the CLI finds without `--data-dir` (the image sets `XDG_DATA_HOME`). Its health check is an authenticated `workboard connections`, which reads the service's token from that directory.

The hub still listens on `127.0.0.1:8421` only, inside the container. Publish that port to the host's loopback, at the same number, since the hub takes its loopback address only as `127.0.0.1:8421`, and have rootless podman's pasta forward it to the container's loopback with `--host-lo-to-ns-lo` (`pasta --help` lists it if pasta has it). By default pasta, like Docker, forwards a published port to the container's own address, where nothing listens. `tailscale serve` on the host then proxies to the published port as above. Under Docker, use `--network host` instead.

```sh
podman run -d --name workboard-hub --volume workboard-hub:/var/lib/workboard \
  --network pasta:--host-lo-to-ns-lo --publish 127.0.0.1:8421:8421 --cap-drop all --security-opt no-new-privileges --read-only \
  --secret workboard-gh-token,type=env,target=GH_TOKEN \
  ghcr.io/jmarrero-bot/work-tracker-poc@sha256:DIGEST \
  serve --hub --allowed-host hub.example.ts.net \
  --github-app-client-id Iv23li... --github-app-callback https://hub.example.ts.net/auth/github/callback \
  --github-user 1234567
```

The GitHub App's options go on the `serve` line, so in a quadlet they belong in its `Exec=`, after `--allowed-host`: `--github-app-client-id`, `--github-app-callback` and one `--github-user` per allowed numeric user ID ([github-app.md](github-app.md)). The client secret is read from the data directory when the hub starts, not from a flag, and a hub started with the App's options but no secret stops at once and says so in its log. So put it in the volume from the podman secret first, with a one-off container of the same image, and start (or restart) the hub after:

```sh
podman run --rm --volume workboard-hub:/var/lib/workboard \
  --secret workboard-github-app-secret,type=env,target=WORKBOARD_GITHUB_APP_SECRET \
  ghcr.io/jmarrero-bot/work-tracker-poc@sha256:DIGEST github-app set-secret --token-env WORKBOARD_GITHUB_APP_SECRET
```

Do the same after rotating the client secret on GitHub, then restart the hub. The hub itself needs no podman secret for the App. Until the App exists, start the hub with `--pairing` instead of the three `--github-...` options; the container's log has a pairing URL then (`podman logs workboard-hub`), which works once within 10 minutes, and `podman exec workboard-hub workboard pair` prints a new one. With neither, browsers can't sign in at all, and only the CLI and companions with the companion token get in.

The CLI runs in the container, where a secret given as an environment variable is what `--token-env` reads:

```sh
podman exec workboard-hub workboard connect github --url https://github.com/ --external --token-env GH_TOKEN --watch-user YOUR-LOGIN
podman exec workboard-hub workboard companion-token
```

A connection keeps its own copy of the token in the data directory. After replacing the secret, restart the container and give the connection the new token with `podman exec workboard-hub workboard verify ID --token-env GH_TOKEN`.

Check through `tailscale serve` that the hub takes its tailnet name:

```sh
curl -sS https://hub.example.ts.net/api/snapshot
```

`` {"error":"Sign in first: with GitHub on a hub that has its App, otherwise with a pairing URL from `workboard pair`"} `` means the name got through and the hub wants a sign-in; `{"error":"Unexpected host"}` means it isn't one of the `--allowed-host` names. `curl -sS https://hub.example.ts.net/auth/status` says whether the App is configured (`"github_app":true`) and pairing is off.

## Set up the companion

On the laptop, run Workboard as usual, connect the internal sources, and sign in to the hub with GitHub:

```sh
workboard serve
workboard connect jira --url https://jira.example.com/ --label 'Work Jira' --token-env WORKBOARD_JIRA_TOKEN
workboard hub login https://hub.example.ts.net/   # enter the code on github.com, then approve it on the hub's page
workboard hub show
```

`hub login` runs GitHub's device flow through the hub, which gives the laptop a credential that only reads its export, bound to the GitHub user who entered the code, once that user also approves the sign-in on the hub's page, signed in there with GitHub. Without the App, give it the companion token instead: `workboard hub set https://hub.example.ts.net/ --token-stdin`. Either pulls once to check the address and the credential before it keeps them; the credential is stored like a connection's, in a private file. From then on the companion pulls on its own schedule (every 5 minutes, or `serve --sync-interval`), with the same backoff as a connection, and `workboard sync` pulls too. `workboard hub unset` stops it and forgets the token and the hub's work.

A hub address is HTTPS, or plain HTTP only to the laptop's own loopback, so the token never crosses a network in the clear. A hub never pulls from another hub.

## What the companion shows

The board, the Mine view and the CLI show the hub's connections, items and efforts next to the companion's own, marked as the hub's and read-only: grouping, abandoning, verifying or disconnecting them is done on the hub. Their efforts show their titles: a hub always exports them. Their IDs start with `hub:`.

The hub's work is as fresh as the hub's connection that synced it was at the last pull. It is stale as soon as a pull fails, not only after three intervals as a connection's work: while the hub can't be reached, its work is never shown as current. The companion's own work isn't affected.

The hub's bot shows in the companion's Bot view too, read-only: its board, its heartbeat and agent runs, and links to ask it for something, built on the companion ([connectors/bot.md](connectors/bot.md#on-a-companion)).

If the companion connects an account the hub also connects, the hub's copy of that connection and its items is left out, and so is a hub effort left with none of its work, so nothing shows twice or empty: the companion's own copy is shown, being synced right on the laptop. The same account is the same provider, the same instance, and the same numeric account ID, which the export carries for GitHub, GitLab, Fedora Forge and the bot, and which survives a rename. Where either side doesn't know it, such as from an older hub, or for an account whose ID isn't a number, the account name decides instead, without regard to case.

## Trust model

**The hub holds external work only, enforced at every write.** A data directory records whether it is a hub's the first time a service opens it, and is never started as the other: a hub restarted without `--hub` could take internal work, and a local directory may already hold some. Every write into a hub's database says what it is and is checked first, deny by default: a connection must be external, so `connect` without `--external` is refused before the credential is used, and so is a plugin connection that isn't external; what a sync found must belong to its external connection and say external; efforts and links take only its external items; manual links, the demo and pulling from a hub are refused outright. A hub that finds anything else in its database refuses to start. The zone is still the person's choice: an internal instance connected as external would be synced by the hub, so connect only external sources there.

**Nothing of the companion's ever goes to the hub.** The hub has no endpoint that takes data from a companion. The companion's requests to the hub are a `GET` of `api/export/external` at the hub's exact address, without a body or a query, with its credential, and to sign in, `POST`s of `auth/device` and `auth/device/poll` without a body, sent by a client that can do nothing else: anything else is refused before it is sent, and redirects are never followed. What the hub learns from a companion is that it signed in and pulled, and when.

**What comes back is checked whole.** Everything in an export must say it is external, items must belong to connections and efforts in it, links must be HTTP(S), and sizes are bounded; otherwise none of it is kept. The companion's own export is built from its own work alone, so nothing pulled from the hub leaves again through it.

**People sign in with GitHub.** With its GitHub App, the hub lets in only the GitHub users on its allowlist, by numeric user ID: a browser through GitHub's web flow, which gives it a session cookie holding a random ID, never a token; a companion through GitHub's device flow, which gives it an export-only credential. [github-app.md](github-app.md) has the details: sessions, their expiry, CSRF and `state` checks, and rate limits. The CLI reaches the hub through its private Unix socket, in the data directory in the container, which only the hub's own user can connect to; the application token, rotated on every start, is taken on loopback only.

**Pairing is the fallback until the App exists.** With `--pairing` and no App, a browser pairs with a one-time code from `workboard pair`, which works once, within 10 minutes: it gets an HttpOnly, SameSite=Strict cookie holding a random session ID, never the application token, also Secure under the hub's tailnet name. Anyone who can reach the hub on the tailnet and has an unused pairing URL can use it. Once the App is configured, pairing is off, and the hub won't start with `--pairing`.

**Serve nothing else under the hub's tailnet name.** Browsers scope cookies by host name, not by port. Any other service under the same name, another `tailscale serve` port or path included, would be sent the hub's session cookie, and with it the session, or with pairing the application token itself and full access to the hub. Give other services a machine of their own, or a name of their own.

**The Host check is not a boundary against the tailnet.** A client on the tailnet can send `tailscale serve` any Host header, the loopback address included, so the names a hub takes keep browsers' cookies and origins straight, but only a credential lets a request in. The application token is refused through a proxy's headers as well, as defense in depth; what keeps it on the hub machine is that only the data directory holds it.

**The companion's credential reads the export, and nothing else.** The application token rotates on every start, so a companion can't keep it, and it can change anything on the hub. A companion instead has the credential `hub login` gave, bound to its user, or the hub's companion token, made on its first start and kept across restarts. Both only read the external export. `workboard companion-token --rotate` replaces the companion token and revokes every credential `hub login` gave, at once, after which each companion needs `hub login` or `hub set` again.

Tokens are files of mode `0600` in the data directory, not encrypted at rest, as every Workboard credential for now. The hub keeps the companion credentials `hub login` gave by their SHA-256 only.

## What crosses the boundary

From the hub to the companion, and nothing the other way: the hub's external export, as `docs/architecture.md` lists it field by field. It is the hub's external connections without their error messages, their items, and the efforts that have such items, with their progress over those items and their titles. A hub always exports its efforts' titles: it holds external work only, and its companions are trusted more than it is. Of a bot connection, it exports its items' board fields, its login, tracker and board, and what it last read of its heartbeat and board; not its heartbeat issue and forge organization as settings (all of it is public, and the comment link and fork pull requests show them), nor why a read failed. The companion checks all of it again ([connectors/bot.md](connectors/bot.md#on-a-companion)).

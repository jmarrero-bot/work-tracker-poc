# Hub and companion

One board, in two parts:

- **The hub** runs on the bot machine and is reachable from anywhere on jmarrero's tailnet. It holds and syncs external sources only (GitHub, Fedora Forge, and the bot's board, fork pull requests and heartbeat; see [connectors/bot.md](connectors/bot.md)), and serves the web UI.
- **The companion** runs on the GNOME work laptop, a toolbox with VPN access. It holds the internal sources (Jira, internal GitLab, Errata), pulls the hub's external work, and shows one board with both.

Internal work never leaves the companion. Away from the laptop, the hub shows external work only, with no placeholder for anything internal: it never had any.

Both are the same `workboard` binary. The hub is `serve --hub`; the companion is an ordinary service with a hub set.

## Set up the hub

On the bot machine, in a data directory of its own:

```sh
workboard --data-dir ~/.local/share/workboard-hub serve --hub --allowed-host bot.tail1234.ts.net
```

The hub listens on `127.0.0.1:8421` only, like any Workboard service. [`tailscale serve`](https://tailscale.com/kb/1312/serve) is the intended front: it serves the hub over HTTPS on the tailnet, with a certificate for the machine's tailnet name, and proxies to the loopback port.

```sh
tailscale serve --bg --https=443 http://127.0.0.1:8421
```

Give that name to `--allowed-host`, once per name the hub is reached by. The hub accepts a request only if its Host is its loopback address or one of those names, and `tailscale serve` passes on the name the browser used. A browser's changes must carry the HTTPS origin of the name the request came by, so cross-site requests are refused behind the proxy as on loopback. Use `tailscale serve`, never Funnel, which would put the hub on the internet: Funnel marks each request it brings in with a `Tailscale-Funnel-Request` header, which `tailscale serve` strips from requests that arrive with one, and the hub refuses every request that has it, the page included. Only a hub takes `--allowed-host`: a local service may hold internal work, and stays reachable on loopback alone.

Then connect its sources, always as external, with the same CLI on the same data directory:

```sh
export WB='workboard --data-dir ~/.local/share/workboard-hub'
$WB connect github --url https://github.com/ --external --token-env GH_TOKEN --watch-user jmarrero
$WB connect fedora --url https://forge.fedoraproject.org/ --external --token-env FEDORA_TOKEN
$WB connect bot --url https://github.com/ --external --token-env GH_TOKEN --board orgs/jmarrero-forge/1 \
  --tracker jmarrero-forge/tracker --heartbeat 1 --bot-login jmarrero-bot --forge-org jmarrero-forge
```

Open the pairing URL the hub printed for its tailnet name to pair a browser, and print the companion token for the laptop:

```sh
$WB companion-token
```

Upgrade the companions before the hub. A companion checks the hub's export whole, and refuses all of it if it has a source or a reason it doesn't know, such as the bot connector's `bot` items and their `bot_waiting` reason, which an older companion doesn't.

## Run the hub from its image

CI publishes the hub's image, `ghcr.io/jmarrero-bot/work-tracker-poc`, for each `v*` tag ([release.md](release.md)). On jmarrero's bot machine it runs from the quadlet in jmarrero/atomic-setup, `bot/workboard-hub/`, which pins the image by digest and holds only the wiring: the volume, the secrets and the port. The hub's code and its image stay here, and moving the pin is an atomic-setup pull request that jmarrero merges.

The image runs `workboard serve --hub` as an unprivileged user, from a distroless base with no shell. Its data directory is the volume at `/var/lib/workboard`, which the CLI finds without `--data-dir` (the image sets `XDG_DATA_HOME`). Its health check is an authenticated `workboard connections`, which reads the service's token from that directory.

The hub still listens on `127.0.0.1:8421` only, inside the container. Publish that port to the host's loopback, at the same number, since the hub takes its loopback address only as `127.0.0.1:8421`, and have rootless podman's pasta forward it to the container's loopback with `--host-lo-to-ns-lo` (`pasta --help` lists it if pasta has it). By default pasta, like Docker, forwards a published port to the container's own address, where nothing listens. `tailscale serve` on the host then proxies to the published port as above. Under Docker, use `--network host` instead.

```sh
podman run -d --name workboard-hub --volume workboard-hub:/var/lib/workboard \
  --network pasta:--host-lo-to-ns-lo --publish 127.0.0.1:8421:8421 --cap-drop all --security-opt no-new-privileges --read-only \
  --secret jmarrero-bot-gh-token,type=env,target=GH_TOKEN \
  ghcr.io/jmarrero-bot/work-tracker-poc@sha256:DIGEST \
  serve --hub --allowed-host bot.tail1234.ts.net
```

The pairing URL is in the container's log (`podman logs workboard-hub`). The CLI runs in the container, where a secret given as an environment variable is what `--token-env` reads:

```sh
podman exec workboard-hub workboard connect github --url https://github.com/ --external --token-env GH_TOKEN --watch-user jmarrero
podman exec workboard-hub workboard companion-token
```

A connection keeps its own copy of the token in the data directory. After replacing the secret, restart the container and give the connection the new token with `podman exec workboard-hub workboard verify ID --token-env GH_TOKEN`.

Check through `tailscale serve` that the hub takes its tailnet name:

```sh
curl -sS https://bot.tail1234.ts.net/api/snapshot
```

`` {"error":"Open the pairing URL printed by `workboard serve`"} `` means the name got through and the hub wants a login; `{"error":"Unexpected host"}` means it isn't one of the `--allowed-host` names. Once GitHub App login ([#12](https://github.com/jmarrero-bot/work-tracker-poc/issues/12)) lands, its options go on the same `serve` line.

## Set up the companion

On the laptop, run Workboard as usual, connect the internal sources, and point it at the hub with the companion token:

```sh
workboard serve
workboard connect jira --url https://jira.example.com/ --label 'Work Jira' --token-env WORKBOARD_JIRA_TOKEN
workboard hub set https://bot.tail1234.ts.net/ --token-stdin   # paste the companion token
workboard hub show
```

`hub set` pulls once to check the address and the token before it keeps them; the token is stored like a connection's credential, in a private file. From then on the companion pulls on its own schedule (every 5 minutes, or `serve --sync-interval`), with the same backoff as a connection, and `workboard sync` pulls too. `workboard hub unset` stops it and forgets the token and the hub's work.

A hub address is HTTPS, or plain HTTP only to the laptop's own loopback, so the token never crosses a network in the clear. A hub never pulls from another hub.

## What the companion shows

The board, the Mine view and the CLI show the hub's connections, items and efforts next to the companion's own, marked as the hub's and read-only: grouping, abandoning, sharing a title, verifying or disconnecting them is done on the hub. Their IDs start with `hub:`.

The hub's work is as fresh as the hub's connection that synced it was at the last pull. It is stale as soon as a pull fails, not only after three intervals as a connection's work: while the hub can't be reached, its work is never shown as current. The companion's own work isn't affected.

If the companion connects an account the hub also connects, the hub's copy of that connection and its items is left out, and so is a hub effort left with none of its work, so nothing shows twice or empty: the companion's own copy is shown, being synced right on the laptop. The same account is the same provider, the same instance, and the same account name, without regard to case, since the export carries the account's name and not its ID.

## Trust model

**The hub holds external work only, enforced at every write.** A data directory records whether it is a hub's the first time a service opens it, and is never started as the other: a hub restarted without `--hub` could take internal work, and a local directory may already hold some. Every write into a hub's database says what it is and is checked first, deny by default: a connection must be external, so `connect` without `--external` is refused before the credential is used, and so is a plugin connection that isn't external; what a sync found must belong to its external connection and say external; efforts and links take only its external items; manual links, the demo and pulling from a hub are refused outright. A hub that finds anything else in its database refuses to start. The zone is still the person's choice: an internal instance connected as external would be synced by the hub, so connect only external sources there.

**Nothing ever goes to the hub.** The hub has no endpoint that takes data from a companion. The companion's only request to the hub is a `GET` of `api/export/external` at the hub's exact address, without a body or a query, with the companion token, sent by a client that can do nothing else: anything else is refused before it is sent, and redirects are never followed. What the hub learns from a companion is that it pulled, and when.

**What comes back is checked whole.** Everything in an export must say it is external, items must belong to connections and efforts in it, links must be HTTP(S), and sizes are bounded; otherwise none of it is kept. The companion's own export is built from its own work alone, so nothing pulled from the hub leaves again through it.

**Authentication is interim.** The hub uses the prototype's authentication: a bearer token, rotated on every start, which the CLI reads from the data directory, and an HttpOnly, SameSite=Strict browser cookie paired with it through the printed URL. Under the hub's tailnet name, which is only reached over HTTPS, the cookie is also Secure. Anyone who can reach the hub on the tailnet and has the pairing URL can use it.

**Serve nothing else under the hub's tailnet name.** The session cookie is the application token itself, and browsers scope cookies by host name, not by port. Any other service under the same name, another `tailscale serve` port or path included, would be sent the cookie with its own requests to `/api` paths, and with it full access to the hub. Give other services a machine of their own, or a name of their own. GitHub App login ([#12](https://github.com/jmarrero-bot/work-tracker-poc/issues/12)) replaces this.

**The companion token reads the export, and nothing else.** That token rotates on every start, so a companion can't keep it, and it can change anything on the hub. A hub therefore also keeps a companion token, made on its first start and kept across restarts, which only reads the external export. `workboard companion-token --rotate` replaces it at once, after which each companion needs `hub set` again.

Tokens are files of mode `0600` in the data directory, not encrypted at rest, as every Workboard credential for now.

## What crosses the boundary

From the hub to the companion, and nothing the other way: the hub's external export, as `docs/architecture.md` lists it field by field. It is the hub's external connections without their error messages, their items, and the efforts that have such items, with their progress over those items and their title only if shared.

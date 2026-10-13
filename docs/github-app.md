# The hub's GitHub App

A hub signs people in with GitHub, through a GitHub App that whoever runs the hub creates under their own account. A GitHub App, not a classic OAuth App, because its user tokens can only reach the repositories it is installed on, with the permissions it was given, and they expire.

- **In a browser**, the hub uses GitHub's web flow: "Sign in with GitHub" sends the browser to GitHub, and GitHub sends it back to the hub's callback URL with a code, which the hub exchanges for a user token.
- **On the companion**, `workboard hub login` uses GitHub's device flow: it shows a code to enter on github.com, and waits.

Either way, who signed in is the numeric GitHub user ID that `GET /user` answers with the new token. Only the IDs on the hub's allowlist get in. A login is never compared, since logins can be renamed and then registered by someone else.

Nothing here could be tested live yet: this repository's own hub has no App yet. The flows are tested against a fake GitHub (see "Testing" below).

## Create the App

On github.com, open **Settings → Developer settings → GitHub Apps → New GitHub App** (<https://github.com/settings/apps/new>), signed in as the account that runs the hub, and set:

| Setting | Value |
| --- | --- |
| GitHub App name | Any free name, such as `workboard-YOUR-LOGIN`. It is shown on GitHub's authorization page. |
| Homepage URL | Where your Workboard's code is, such as this repository, `https://github.com/jmarrero-bot/work-tracker-poc` |
| Callback URL | `https://HUB-NAME/auth/github/callback`, HUB-NAME being the hub's tailnet name, the one given to `--allowed-host`, such as `https://hub.example.ts.net/auth/github/callback`. Only the browser follows it, so GitHub doesn't need to reach the tailnet. |
| Expire user authorization tokens | On (the default). User tokens then last 8 hours, with a refresh token. |
| Request user authorization (OAuth) during installation | On. Installing the App then signs the installer in too. |
| Enable Device Flow | On. The companion's `workboard hub login` needs it. |
| Setup URL, Redirect on update | Empty, off. |
| Webhook | **Active** off. The hub takes no webhooks. |
| Permissions | None (see below). |
| Where can this GitHub App be installed? | Only on this account. |

Then, on the App's **General** page:

- note the **Client ID** (it starts with `Iv`);
- **Generate a new client secret**, and copy it once; GitHub won't show it again;
- leave the private keys alone: the hub never acts as the App itself, only as the signed-in user.

Find the numeric user ID of each person the hub allows:

```sh
gh api users/YOUR-LOGIN --jq .id
```

### Permissions

**Signing in needs none.** `GET /user` answers with the account of any user token, whatever the App may do, so the App is created with no repository, organization or account permissions.

**Later, for commands posted as the signed-in user ([#13](https://github.com/jmarrero-bot/work-tracker-poc/issues/13)), a separate step:** the hub will post each command as an issue or comment in one control repository, CONTROL-OWNER/CONTROL-REPO, with that user's token. Then the App gets:

- Repository permissions: **Issues: Read and write**, and Metadata: Read-only, which GitHub adds with any repository permission;
- installed on **only** that repository ("Only select repositories").

A user token can do only what both the user and the App's installation may, so their token then reaches that one repository's issues, and nothing else. An App installable "Only on this account" can only be installed on the account that owns it, so the control repository has to be that account's, or the App installable elsewhere; #13 settles how.

## Configure the hub

Keep the client secret in the hub's data directory, in a file only its owner can read. It is read from standard input or an environment variable, never from the command line:

```sh
export WB="workboard --data-dir $HOME/.local/share/workboard-hub"
$WB github-app set-secret --token-stdin    # paste the client secret
```

That writes `github-app.secret` (mode `0600`) in the data directory; the hub refuses to start if others can read it. Then start the hub with the App's client ID, its callback URL, and the allowed user IDs:

```sh
$WB serve --hub --allowed-host hub.example.ts.net \
  --github-app-client-id Iv23li... \
  --github-app-callback https://hub.example.ts.net/auth/github/callback \
  --github-user 1234567
```

The callback has to be HTTPS on one of the `--allowed-host` names, at `/auth/github/callback`. Repeat `--github-user` for each allowed user. A new secret or allowlist takes effect when the hub starts again.

Once the App is configured, browser pairing is off: the hub prints no pairing URL, refuses `POST /api/session`, and won't start with `--pairing`. The application token is then the CLI's alone: read from the data directory, and taken only under the loopback address, without the headers a proxy such as `tailscale serve` adds (`Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `Tailscale-User-Login`). That is defense in depth: a client on the tailnet can send any Host header through the proxy, so what keeps the token on the hub machine is that only the data directory holds it.

With "Request user authorization (OAuth) during installation" on, GitHub sends the browser to the callback right after installing the App, with a sign-in the hub didn't start; the hub refuses it, and **Sign in with GitHub** on the hub's page then works as usual.

On the companion, sign in to the hub instead of pasting its companion token:

```sh
workboard hub login https://hub.example.ts.net/
```

It prints a code and `https://github.com/login/device`; enter the code there, as an allowed user, and the companion starts pulling.

## How the sign-ins work

### The browser

1. The page posts to `/auth/github/login`. The hub takes it only under the callback's name, from that name's own page (its `Origin`), and only ten sign-in attempts a minute. It answers with GitHub's authorization URL, carrying a random `state` and a PKCE challenge (S256) of a random verifier, and sets `__Host-workboard_login`, a cookie that binds the sign-in to this browser: HttpOnly, Secure, SameSite=Lax (so it comes back with GitHub's redirect), for 10 minutes.
2. GitHub sends the browser back to `/auth/github/callback` with a code and the `state`. The hub finds the sign-in by the cookie, uses it up, and checks that `state` matches, in constant time; a sign-in from another browser, a forged or repeated `state`, or one older than 10 minutes is refused before GitHub is asked anything.
3. The hub exchanges the code, with the client secret and the PKCE verifier, for a user token, reads `GET /user` with it, and checks the numeric ID against the allowlist.
4. It keeps the user token, and its refresh token, in `github-users/ID.json` (mode `0600`), for posting commands as that user later (#13). This slice uses it only to tell who signed in.
5. It sets the session cookie, `__Host-workboard_user`: HttpOnly, Secure, SameSite=Strict, Path=/, holding a random session ID, never the token. The session ends after 12 hours unused, and 7 days after sign-in in any case. Unused means no request of yours: each one renews the session, but the page's own refreshes every 30 seconds don't (they say so with an `X-Workboard-Background` header), so an open page left alone still signs out. **Sign out** (a `POST /auth/logout` from the page) ends it at once. As soon as none of a user's sessions is left, signed out, expired or dropped, the hub deletes their token: a sweep every minute ends expired sessions without waiting for a request.

Sessions are kept in memory, so restarting the hub signs everyone out, and it deletes the user tokens kept for them, with its App configured or not. Session IDs, sign-in cookies and device-flow handles are looked up by their SHA-256, so a lookup's timing gives none away.

### The companion

The hub runs GitHub's device flow on the companion's behalf:

1. `workboard hub login URL` asks its local service, which posts to the hub's `/auth/device`, without a body. The hub asks GitHub for a device code and answers with the code to show, the page to enter it on, and a random handle.
2. The companion polls `/auth/device/poll` with the handle, without a body, as often as GitHub allows. The hub asks GitHub at most once per interval, and slows down when GitHub says to.
3. Once the person entered the code, the hub reads `GET /user` with the new token, checks the numeric ID against the allowlist, and answers with a **companion credential**: a random token, bound to that user ID, which reads the export and nothing else, like the companion token. The hub keeps only its SHA-256 (`companions.json`, mode `0600`), and once its user is no longer on the allowlist, the hub deletes it when it starts, so putting the user back doesn't bring it back. Each user keeps at most four, a new one dropping the oldest, and `workboard companion-token --rotate` revokes all of them, with the App configured or not.
4. The companion keeps the credential like the companion token, and pulls.

**Why the hub runs the flow**, rather than the companion getting a user token itself and sending it to the hub: the user token never leaves the hub, and the hub only ever accepts a token it got from GitHub for its own App. A token the companion sent could be any token of the user's, from any app, and it would cross the tailnet to the hub. The companion also needs no GitHub client and no client ID of its own, and still sends the hub nothing of its own: its requests at sign-in are two `POST`s without a body. The hub doesn't keep the device flow's user token; the web sign-in keeps one for #13.

The export-only companion token stays, as a fallback: `workboard hub set URL --token-stdin` still takes it.

**Who can start a companion's sign-in.** Anyone on the tailnet can ask the hub for a device code, since a companion has no credential yet. Someone who did, and got an allowed user to enter their code on github.com, would get a companion credential, which reads the export. GitHub's page names the App and asks the person to confirm, so only enter a code that your own `workboard hub login` printed. A later change could have a signed-in browser approve each companion first.

### Rate limits and logs

Each peer on the tailnet may make ten sign-in attempts a minute, and all of them together a hundred, as a backstop; beyond either, the hub answers HTTP 429 with `Retry-After`. Browsers and companions are counted apart. A browser's attempts are sign-in starts and callbacks with no sign-in to finish, so the browser that started one can always finish it; a companion's are device-flow starts and polls with a handle the hub doesn't know. A peer is the last `X-Forwarded-For` address, which `tailscale serve` sets to the peer's tailnet address itself, so one peer can't use up another's attempts, only the backstop, and only by making a hundred a minute. That is why the hub must be fronted with `tailscale serve` in its HTTPS mode (see [hub.md](hub.md)). At most 16 browser sign-ins and 4 device flows wait at a time, the oldest making way for a new one.

Tokens, codes, the client secret and the `state` are never printed or logged, and never in an error message. GitHub's own refusals are shown by their code alone, such as `bad_verification_code`.

## The one write-shaped request

Everything else Workboard sends GitHub goes through the read-only client, which refuses anything that could change GitHub (see [architecture.md](architecture.md)). The code exchange and the device flow are `POST`s, so they go through their own client, `src/oauth.rs`, whose single send passes only:

- `POST https://github.com/login/oauth/access_token` with a form of exactly `client_id`, `client_secret`, `code`, `code_verifier` and `redirect_uri` (the code exchange), or exactly `client_id`, `device_code` and `grant_type=urn:ietf:params:oauth:grant-type:device_code` (a device-flow poll);
- `POST https://github.com/login/device/code` with a form of exactly `client_id`.

Each must be form-encoded, without an `Authorization` or `Cookie` header, every field once and not empty. Anything else is refused before it is sent, and counted: another method or URL, the API (`api.github.com`), a query string, another field or grant. `GET /user` then goes through the read-only client, like every other read. Clippy refuses the reqwest calls that could send anything else, and the client's one send is a named exception.

## Testing

`cargo test` runs both flows against a fake GitHub on loopback, which serves the token and device-code endpoints and `GET /user`, and checks the PKCE verifier against the challenge it was given: the web flow through the hub's routes, from the login start to a session that reads and changes the board, and the device flow from the companion's `hub login` to its pulls. They also cover the allowlist by numeric ID, a renamed login and a login taken by another ID, sessions expiring unused and after a week, renewal, logout, the cookies' flags, refused CSRF and `state` mismatches, rate limits, and the OAuth client refusing any other URL, method, field or header. `tests/hub_smoke.py` checks a hub with an App configured, as far as it goes without GitHub: the refused pairing, the CLI token on loopback only, the sign-in start's checks and its cookie, and a callback without its browser.

A live test waits for this repository's own hub to get its App.

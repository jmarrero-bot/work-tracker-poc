# Fedora Forge

What "Fedora Forge" is, found from Fedora's own documentation and API answers rather than assumed, and what Workboard connects to.

All checks below were anonymous HTTPS GETs from the bot machine on 2026-10-12, between 04:34 and 04:40 UTC.

## Findings

Fedora runs two forges that matter for a Fedora packager's and contributor's work:

| Instance | Software | What lives there |
| --- | --- | --- |
| https://forge.fedoraproject.org/ | Forgejo v15.0.9 | Fedora teams, SIGs and subprojects, such as `atomic/docs` and `atomic/tracker` (Fedora/CentOS bootc), `atomic-desktops/*` and `iot/*` |
| https://src.fedoraproject.org/ | Pagure, API version 0.31 | Package dist-git (`rpms/*`), including `rpms/bootc`, `rpms/rpm-ostree` and `rpms/ostree`, and their pull requests |

**Fedora Forge is forge.fedoraproject.org, and it is Forgejo.** Fedora's documentation calls it "the official home for Fedora teams, SIGs, and subprojects. Built on the open-source Forgejo platform" ([Fedora Forge Documentation](https://docs.fedoraproject.org/en-US/forge-documentation/)).

**Package dist-git is still Pagure, and is moving to Forgejo.** `rpms/*` is not on forge.fedoraproject.org yet (`GET /api/v1/repos/rpms/bootc` answers that `rpms` doesn't exist). The move is tracked in [forge/distgit-migration](https://forge.fedoraproject.org/forge/distgit-migration), whose task "[Decommission old Pagure Dist-Git](https://forge.fedoraproject.org/forge/distgit-migration/issues/121)" is open, and the Forge team's tracker has dist-git backup and staging work for Forgejo ([forge/forge](https://forge.fedoraproject.org/forge/forge/issues?q=dist-git)). No date is published.

**pagure.io** still serves its web pages, but its API answers HTTP 404 (`/api/0/version` and `/api/0/-/version`); repositories such as `fedora-iot/ostree` now live on Fedora Forge, as `iot/ostree` with `original_url` pointing back at pagure.io. It isn't a target.

### Where a packager's Fedora work is

- On src.fedoraproject.org, `GET /api/0/user/LOGIN/requests/filed?status=all` lists the pull requests a user opened, and `/api/0/user/LOGIN/requests/actionable` those on packages they can act on, such as Packit's `rpms/bootc` updates.
- On forge.fedoraproject.org, `GET /api/v1/users/LOGIN` answers with the account and its numeric ID, and its repositories and public activity are listed too. Public lists don't show assignments or review requests, which need the user's token.

So bootc, ostree and rpm-ostree packaging work is on src.fedoraproject.org today, and moves to Forgejo with dist-git. The Atomic SIG's projects, where the bootc base images and docs live, are on Forgejo already.

## Evidence

Forgejo, forge.fedoraproject.org:

| Request | Answer |
| --- | --- |
| `GET /api/v1/version` | `{"version":"v15.0.9-ee5a3ad"}` |
| `GET /api/forgejo/v1/version` | `{"version":"v15.0.9-ee5a3ad"}` (Forgejo's own endpoint, so this is Forgejo, not Gitea) |
| `GET /swagger.v1.json` | "Forgejo API", version `v15.0.9-ee5a3ad`, base path `/api/v1` |
| `GET /api/swagger` | The Forgejo API reference |
| `GET /api/v1/settings/api` | `{"max_response_items":50,"default_paging_num":30,...}` |
| `GET /api/v1/repos/issues/search?state=all&limit=1` | 200, with `x-total-count: 199120` and a `link` header with `next` and `last` |
| `GET /api/v1/repos/atomic/docs/issues?state=closed&type=pulls&limit=50` | 200, 50 of `x-total-count: 148`, each with `pull_request.merged` |
| `HEAD /api/v1/repos/atomic/images/issues/1` | 405 |

No answer carried `ETag`, `Last-Modified` or any rate-limit header (`x-ratelimit-*`, `ratelimit`, `retry-after`). For comparison, Codeberg, another Forgejo instance, sends `ratelimit-policy` and `ratelimit` from its proxy.

Pagure, src.fedoraproject.org:

| Request | Answer |
| --- | --- |
| `GET /api/0/version` and `GET /api/0/-/version` | `{"version": "0.31"}`, the API version (Pagure's API reference shows `/api/0/-/version` as the endpoint) |
| `GET /api/0/rpms/bootc` | `rpms/bootc`, owner `walters` |
| `GET /api/0/rpms/bootc/pull-requests?status=all&per_page=5` | 31 pages |

## Forgejo's API, as Workboard uses it

- **Authentication:** an access token in `Authorization: token TOKEN` ([API usage](https://forgejo.org/docs/v15.0/user/api-usage/)). On Fedora Forge, a person logs in once with their Fedora Account (OpenID Connect) and creates the token at [Settings, Applications](https://forge.fedoraproject.org/user/settings/applications) ([Fedora's guide](https://docs.fedoraproject.org/en-US/forge-documentation/using_http_auth/)).
- **Read-only scopes:** each scope is `read:` (GET routes only) or `write:` ([Access Token Scope](https://forgejo.org/docs/v15.0/user/token-scope/)). Workboard needs `read:user` (`/user`), `read:issue` (`/repos/issues/search` and issues) and `read:repository` (a pull request's requested reviewers). A token with only these can't change anything; a token with "Public only" access sees public repositories only.
- **Account:** `GET /api/v1/user`, whose numeric `id` is stable.
- **What waits on the account:** `GET /api/v1/repos/issues/search` with `state=open` and `assigned`, `review_requested`, `mentioned` or `created`. These filters apply only to a signed-in request; Forgejo ignores them on an anonymous one and returns everything public (`routers/api/v1/repo/issue.go`, `SearchIssues`). `review_requested` matches requests to the account's teams too, and leaves out pull requests the account already approved or rejected (`models/issues/issue_search.go`).
- **Outcomes:** an issue or pull request has `state` (`open` or `closed`); a pull request has `pull_request.merged` and `pull_request.draft` (true for a work-in-progress title prefix as well). Issues have no field saying why they were closed.
- **Paging:** `page` and `limit`, capped at `max_response_items` (50 here), with `x-total-count` and `link` on every page.
- **Conditional requests:** not available. Forgejo sends no `ETag` or `Last-Modified` on these API answers.
- **Rate limits:** Forgejo itself has none on the API, and Fedora's proxy sends no rate-limit headers. A proxy can still answer 429, with or without `Retry-After`.
- **Moved repositories:** the API redirects a renamed repository to its new path with HTTP 301 (`services/context/repo.go`, `RedirectToRepo`). Workboard follows that once, within the API.

## Decision

Fedora Forge gets a built-in, read-only Forgejo connector, for the `fedora` source and forge.fedoraproject.org (see [architecture.md](../architecture.md)). The migration doesn't say yet whether dist-git moves onto forge.fedoraproject.org or onto a Forgejo instance of its own: in the first case the same connection sees it, in the second it is another connection with the same connector.

There is no Pagure connector. src.fedoraproject.org can still be tracked with manual source links. Whether to write one before dist-git moves is still open; until then, this design note records what one would take:

- **API:** Pagure's `/api/0/` ([reference](https://src.fedoraproject.org/api/0/)). The token's account comes from `POST /api/0/-/whoami`, a POST that a read-only client would have to allow as its one exception, and it is a username: Pagure's API has no numeric user ID. `/api/0/user/USER/requests/filed` and `/requests/actionable` list pull requests a user opened and those on projects they can act on; `/api/0/rpms/NAME/pull-request/ID` fetches one.
- **Authentication:** a Pagure API token, `Authorization: token TOKEN`, with ACLs chosen when it is made. Public pull requests are readable without one.
- **Outcomes:** a pull request's `status` is `Open`, `Merged` or `Closed`, which map to open, delivered and closed without delivery.
- **Mine:** Pagure has no review requests. `assignee` and "actionable" are the closest, and actionable lists every pull request on the packages the user has commit access to, including Packit's automatic ones.

It would be a second connector with its own read-only client, for an instance that is being retired.

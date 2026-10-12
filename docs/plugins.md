# Connector plugins

A connector reads one kind of source (Jira, GitLab, GitHub, ...) for one account. Workboard asks it three things: whose account a credential belongs to, which items are relevant to that account, and what became of items it tracked before. This document records how connectors plug in and the protocol they speak.

## Decision: one protocol, built-ins in process, third-party plugins out of process

The contract is a versioned JSON protocol, described below. Two kinds of connectors implement it:

- **Built-in connectors** (Jira Data Center and GitLab today) are Rust code in the service, behind the `Connector` trait in `src/connector.rs`. The trait's types are the protocol's messages, so a built-in connector answers exactly what a plugin would.
- **Third-party plugins** are executables that speak the protocol as JSON over standard input and output. They can be written in any language and run in their own process.

Why not just one of them:

- A plugin interface that only takes Rust code compiled into the service isn't one others can extend without forking. Rust has no stable ABI for loading libraries at run time, and code loaded into the service would share its memory, where every connection's credential passes.
- Running the built-ins out of process would cost a process start per call and give up what they share in process: one HTTP client that keeps TLS verification, refuses redirects and has a timeout, and type-checked tests against recorded fixtures.
- Because both kinds answer with the same types, everything Workboard checks happens in one place, on the host side (`connector::sync`), whichever connector answered: that the credential still belongs to the connection's account, which items get fetched again, that links are HTTP(S), that an inaccessible item becomes unknown and never done, and that no connector can mark work abandoned. A built-in connector could move out of process without any change there.
- WebAssembly would isolate plugins better than a process does, but needs a runtime and host functions for HTTP. The protocol can be carried over to it later if that's needed.

A separate process is isolation from crashes, hangs and the service's memory, not a sandbox: a plugin runs as the same user as the service and can read what that user can. Install only plugins you trust.

## Installing a plugin

A plugin serves one of the sources that has no built-in connector yet (`github`, `fedora` or `errata`). Put an executable named after the source in the `plugins` directory of the data directory, such as `~/.local/share/workboard/plugins/errata`, then connect as with any other source:

```sh
workboard connect errata --url https://errata.example.com/ --label Errata --token-env WORKBOARD_ERRATA_TOKEN
```

Jira and GitLab always use their built-in connectors. `tests/plugins/fake-forge` is a small plugin in Python, used by the tests.

Workboard runs a plugin:

- only if it is a regular file, not a symbolic link, owned by the user running Workboard and not writable by its group or others, in a directory that is the same. On Linux, Workboard opens the file, checks it through the open descriptor, and runs that descriptor (`/proc/self/fd/N`), so the file checked is the file run even if the path is replaced in between. Elsewhere it runs the path again, and only those ownership checks keep it from being swapped;
- by its absolute path, never one looked up in `PATH`;
- with no arguments, `/` as its working directory, and an environment of only `PATH=/usr/local/bin:/usr/bin:/bin` and `LANG=C.UTF-8`. There is no `HOME`, no proxy variables such as `HTTPS_PROXY`, and no CA bundle variables such as `SSL_CERT_FILE`, so a plugin that needs them has to find them on its own for now;
- with its standard error discarded, since a plugin's own diagnostics could echo the credential;
- in a process group of its own, which is killed when the call ends, so a helper it started can't outlive the call;
- for at most 120 seconds per call. After that it is killed and the call fails as `transient`;
- reading at most 16 MiB of its answer.

An error message from a plugin is shown after the plugin's name, with any copy of the credential replaced by `[redacted]`, without control characters, and cut at 500 characters. An item that contains the credential is refused.

## Zones are not the plugin's business

Each connection has a zone, internal or external, chosen by the person connecting it (see `docs/architecture.md`). A plugin never sees or reports a zone; the items it returns take their connection's zone. A plugin can't move an item out of the internal zone.

## Protocol, version 1

Each call runs the plugin once. Workboard writes one JSON request to its standard input and closes it, then reads one JSON response from its standard output. The plugin exits with status 0 after writing the response, including when the response is an error. Anything else (a crash, a non-zero exit, no response, output that isn't the response) fails the call.

### Requests

```json
{
  "protocol": 1,
  "method": "list",
  "instance": {"base_url": "https://gitlab.example.com/"},
  "credential": "glpat-...",
  "account": {"id": "1234", "name": "jdoe"}
}
```

| Field | Meaning |
| --- | --- |
| `protocol` | The protocol version, `1`. A plugin answers a version it doesn't know with an `invalid` error. |
| `method` | `verify`, `list` or `refresh`. |
| `instance.base_url` | The instance's canonical HTTPS URL, ending in `/`. Instance subpaths are kept. |
| `credential` | The connection's credential, for this call only. It is passed here and nowhere else: never in arguments or the environment. A plugin never prints or stores it. |
| `account` | For `list` and `refresh`: the account `verify` returned, already checked against the connection. |
| `items` | For `refresh`: the tracked items to fetch again, each `{"remote_id": ..., "key": ...}`. |

### Responses

Every response carries `"protocol": 1` and exactly one of these:

| Method | Field | Meaning |
| --- | --- | --- |
| `verify` | `account` | `{"id": ..., "name": ...}`. `id` is stable on the instance, such as a numeric user ID, never a display name that can change. Workboard stores it when connecting and refuses a later credential for another account. |
| `list` | `items` | Every item relevant to the account, as items below. All of them or an error: never a partial list. Each item at most once. |
| `refresh` | `results` | One answer per requested item: `{"remote_id": ..., "item": {...}}`, or `{"remote_id": ..., "error": {"kind": "inaccessible", "message": ...}}` when the item is missing or hidden from the account. |
| any | `error` | `{"kind": ..., "message": ...}`, failing the whole call. |

An item:

| Field | Meaning |
| --- | --- |
| `remote_id` | Stable identity on the instance, up to 256 ASCII letters, digits and `-_.:~@+=`. It becomes part of the local item ID, so it never changes for the same item. |
| `key` | The reference people use, such as `RHEL-1` or `group/project!4`. |
| `title` | The item's title. |
| `url` | The item's own web page, HTTP(S) without credentials. |
| `status` | The source's own status, unmodified. Workboard always shows it. |
| `outcome` | What the status means for the work: `open`, `delivered`, `closed_without_delivery` or `unknown`. Closed is not delivered: a change closed without merging, or a ticket resolved as Won't Do, is `closed_without_delivery`. Never `abandoned`, which is a person's decision kept in Workboard. |
| `outcome_reason` | A short explanation of the outcome, shown next to it. |
| `action` | While open, why it needs attention: `next`, `review`, `waiting` or `follow-up`. Ignored for items that aren't open. Workboard sets `follow-up` on refreshed items. |
| `updated_at` | When the source last changed the item. |
| `reasons` | Optional. Why the item waits on the account, as a list of `review_requested`, `team_review_requested`, `assigned`, `mentioned`, `changes_requested` (on the account's own change), `checks_failing` (on the account's own change), `authored` or `todo` (on its to-do list for another reason). The Mine view lists open items that have one. Workboard clears them on items fetched again by `refresh`, since the listing no longer has them. |

Text fields are limited to 8192 bytes. Unknown fields are ignored, so a later minor addition doesn't break older plugins or hosts.

### Errors

| `kind` | Meaning | What Workboard does |
| --- | --- | --- |
| `auth` | The credential was rejected, lacks a permission, or belongs to another account. | Keeps the previous items and shows the error on the connection: a person has to fix the credential. |
| `transient` | Network, timeout, rate limit or a server error. | Keeps the previous items and shows the error; a later sync can succeed. |
| `inaccessible` | One tracked item is missing or hidden from the account. Only valid inside `refresh` results. | Marks that item unknown, keeps its last known status, and fetches it again next time. |
| `invalid` | The source answered something unusable, or the request can't be done (for example too many items to list completely). | Keeps the previous items and shows the error. |

Messages are shown to the person. They never contain the credential or a remote response body, which can echo it.

### How a sync uses the methods

1. `verify`. If the account isn't the connection's, the sync fails with `auth`.
2. `list`.
3. `refresh`, only if some items the connection tracked (open or unknown) are missing from the listing, so a merge or a finished ticket is still seen.

Nothing is saved unless every step succeeds.

### Versions

`protocol` changes when a request or response changes in a way an existing plugin or host could misread. Adding an optional field doesn't change it. A host refuses a response for another version.

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

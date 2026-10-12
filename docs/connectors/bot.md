# The bot connector

The `bot` connector shows what a harness bot, such as jmarrero-bot, works on: the items on its Projects board, its open pull requests in its forge organization, and the agent runs its coordinator's heartbeat lists. It only reads, through the same read-only GitHub client as the GitHub connector (see "GitHub is read only" in [../architecture.md](../architecture.md)): REST `GET`s, and GraphQL queries for the board, which only GraphQL serves. Later slices make the work tracker the bot's control plane; this one shows its work.

## Connect it

Nothing about the bot is assumed: each part is given when connecting. jmarrero's values are what `bot-operator --json` says on the bot's machine:

```sh
workboard connect bot --url https://github.com/ --external --token-env GH_TOKEN \
  --board orgs/jmarrero-forge/1 --tracker jmarrero-forge/tracker --heartbeat 1 \
  --bot-login jmarrero-bot --forge-org jmarrero-forge
```

- `--board`: the Projects (v2) board the bot works from, as `orgs/OWNER/NUMBER`, `users/OWNER/NUMBER`, or the board's address.
- `--tracker`: the repository where work is asked of the bot, as `OWNER/REPO`.
- `--heartbeat`: the tracker issue whose comment by the bot holds its heartbeat.
- `--bot-login`: the bot's login: the author of its fork pull requests and of the heartbeat.
- `--forge-org`: the organization that holds the bot's forks.

The bot's work is public, so the connection is external, which the hub requires anyway. The token needs to read the board (`read:project`) and the repositories its items are in. For testing it is the bot's own token; read-only use is enforced in code, whatever the token allows. The Mine view lists what waits on the connection's account, which, with the bot's own token, is the bot's login.

## What it reads

- **Board items.** Each issue or pull request on the board, but archived ones and draft issues, with the board's Status as its native status, and its Priority, Why, News and Branch. Its outcome follows the rule every connector does, closed is not delivered, and the board saying Done isn't either: Done is delivered only if the item's pull request, its own or the one its Branch links, merged, or its issue was closed as completed. Otherwise Done needs a decision. "Needs human" and "Draft" wait on the person, and show in Mine as "bot waiting on you". An item that leaves the board is read again by its ID; one that is gone is unknown, never done. An item whose issue or pull request this token can't see comes with no content: it can't be shown, so it is counted instead, and the sync result, `workboard bot` and the Bot view say "N items this token can't read", so the board is never shown as all there is while there are any.
- **Fork pull requests.** The bot's open pull requests in its forge organization, each linked to the board item whose Branch is its address. One that is a board item itself is that item. Once one closes, as `bot-pr` closes one it promotes upstream, merged or not, it is no longer tracked if a board item linked it: the board item follows the upstream pull request, and carries what became of the work. One that no board item links stays closed without delivery, for a person to decide.
- **The heartbeat.** The first comment by the bot on the heartbeat issue that starts with `<!-- bot-heartbeat v1 -->`, as `bot-heartbeat` in homegit writes it: the coordinator's session and loop state, when it was updated, and each worker as an agent run (its name, the item it works on, its status, and when it started). The comment is untrusted: at most 64 KiB, its JSON at most 32 KiB, names and states of a safe character set, no time more than five minutes ahead of this machine's clock, and every link an issue or pull request on the instance. A worker that isn't well formed is left out and counted. At most 300 comments are read; a heartbeat not among them is an error, not a missing heartbeat. A heartbeat older than fifteen minutes, and past the wake time it gave plus five minutes, is stale; a coordinator that said it stopped is stopped, not stale.

Agent runs are not items: they are kept apart, have no outcome, and a worker that finished delivered nothing by finishing. If the heartbeat can't be read, the board still syncs, and the last heartbeat read is kept, with the error.

`workboard bot` (or the Bot view, or `GET /api/bot`) shows each bot's board counts, as the last sync listed the board (items that left it since aren't counted), its heartbeat and its agent runs. Each sync result lists what was sent, by method, so a person can check it only read.

## Ask the bot

Asking the bot for something takes no new authority: it is a link that the person opens in their own browser, and submits there, as themselves. Workboard sends nothing, and holds no credential that could write.

- **A new issue** in the tracker, labeled `needs-triage`, its body a template, about anything or about one of the bot's items.
- **A comment** on an item's own issue or pull request. GitHub takes no comment text in a link, so the page and the CLI show how it starts, `@BOT-LOGIN `, to paste.

```sh
workboard ask                     # a new issue, about anything
workboard ask ITEM_ID             # a new issue about an item
workboard ask ITEM_ID --comment   # comment on the item itself
workboard ask ITEM_ID --print     # print the link instead of opening it
```

Every value in a link is percent-encoded but for unreserved characters, so no title can end a value, add a parameter or start the fragment.

## Not yet

The external export carries a bot's items and why they wait, but not the board's fields, the agent runs, or the links to ask the bot, so a companion shows the hub's bot items without them; the hub's page has them all.

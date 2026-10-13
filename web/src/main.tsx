import React, { useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Inbox,
  Layers,
  Plug,
  Search,
  Plus,
  RefreshCw,
  ExternalLink,
  ArrowUpRight,
  Check,
  X,
  ChevronRight,
  Terminal,
  Circle,
  GitPullRequest,
  Ticket,
  AlertCircle,
  LayoutGrid,
  Unplug,
  UserCheck,
  Bot,
  Copy,
} from "lucide-react";
import "./style.css";

type Provider =
  | "jira"
  | "gitlab-internal"
  | "gitlab-public"
  | "github"
  | "fedora"
  | "bot"
  | "errata";
// Internal work is shown only on the machine that syncs it; external work
// may leave it in the external export, as a hub's does for its companions.
type Zone = "internal" | "external";
type Outcome =
  | "open"
  | "delivered"
  | "closed_without_delivery"
  | "abandoned"
  | "unknown";
// Why an item waits on an account.
type Reason =
  | "review_requested"
  | "team_review_requested"
  | "assigned"
  | "mentioned"
  | "changes_requested"
  | "checks_failing"
  | "authored"
  | "todo"
  | "bot_waiting";
const reasonLabels: Record<Reason, string> = {
  review_requested: "Review requested",
  team_review_requested: "Team review requested",
  assigned: "Assigned",
  mentioned: "Mentioned",
  changes_requested: "Needs action: changes requested",
  checks_failing: "Needs action: checks failing",
  authored: "Authored",
  todo: "On the to-do list",
  bot_waiting: "Bot waiting on you",
};
// What the bot connector reads besides: a board item's fields, or the board
// item whose Branch links a fork pull request.
type BotFields = {
  kind: "board_item" | "fork_pr";
  priority: string | null;
  why: string | null;
  news: string | null;
  branch: string | null;
  board_item: string | null;
};
type Item = {
  id: string;
  connection_id: string | null;
  provider: Provider;
  remote_id: string;
  key: string;
  title: string;
  url: string;
  status: string;
  outcome: Outcome;
  outcome_reason: string;
  action: string;
  updated_at: string;
  effort_id: string | null;
  abandoned_at: string | null;
  demo: boolean;
  zone: Zone;
  reasons: Reason[];
  // When its connection last synced successfully; null for manual items.
  synced_at: string | null;
  // Too long ago for it to be current.
  stale: boolean;
  // It came from the hub, and is read-only here.
  from_hub: boolean;
  bot?: BotFields;
};
// One worker the bot's coordinator runs, from its heartbeat. Agent runs
// aren't items: one that finished delivered nothing by finishing.
type AgentRun = {
  name: string;
  item_url: string;
  item_key: string;
  status: string;
  started_at: string;
  devspace: string | null;
};
// The coordinator's heartbeat.
type Heartbeat = {
  updated_at: string;
  session: string;
  loop_state: string;
  next_wake_at: string | null;
  runs: AgentRun[];
  // Workers left out as malformed.
  skipped: number;
  comment_url: string | null;
  // Too old to trust: the coordinator may have stopped.
  stale: boolean;
};
// What the last listing of a bot's board found.
type BoardSummary = {
  // Board items by board status.
  statuses: Record<string, number>;
  fork_prs: number;
  // Items this token can't read: while there are any, the board shown
  // isn't all of it.
  unreadable: number;
};
// What a bot connection last read of its heartbeat and its board.
type AgentRuns = {
  connection_id: string;
  // None when the bot publishes none.
  heartbeat: Heartbeat | null;
  // Why the last sync couldn't read it: it is what an earlier one read.
  error: string | null;
  last_success: string | null;
  stale: boolean;
  board: BoardSummary | null;
};
// Links that ask the bot for something, which the person opens and
// submits themselves. Nothing is sent by building them.
type Ask = {
  issue_url: string;
  comment_url: string | null;
  comment: string | null;
};
// A connected bot.
type BotView = {
  connection_id: string;
  label: string;
  bot_login: string;
  board_url: string;
  agents: AgentRuns | null;
  ask: Ask;
  // By item ID.
  asks: Record<string, Ask>;
  // The hub's bot, as of the companion's last pull: read-only here.
  from_hub?: boolean;
};
// Only links that open a web page are shown as links.
const webLink = (url: string | null | undefined) =>
  url && /^https?:\/\//.test(url) ? url : undefined;
// One thing that waits in the Mine view, once, with every reason.
type MineRow = {
  id: string;
  provider: Provider;
  key: string;
  title: string;
  url: string;
  status: string;
  outcome: Outcome;
  outcome_reason: string;
  zone: Zone;
  updated_at: string;
  // Each account it waits on, with that account's own reasons: the
  // connection's account, or the user it watches; and how fresh that
  // connection's view of it is.
  waits_on: {
    account: string;
    connection_id: string;
    reasons: Reason[];
    synced_at: string | null;
    stale: boolean;
  }[];
  // The oldest last success of the connections that found it, and whether
  // any of them is stale.
  synced_at: string | null;
  stale: boolean;
  // Only the hub found it: read-only here.
  from_hub: boolean;
};
// A watch list its last sync couldn't update.
type WatchError = { connection_id: string; login: string; error: string };
// What waits on the connected accounts, and apart from it what watching
// other users found, which is never theirs.
type MineView = {
  mine: MineRow[];
  watching: MineRow[];
  watch_errors: WatchError[];
};
type Connection = {
  id: string;
  provider: Provider;
  label: string;
  base_url: string;
  account_id: string;
  account_name: string;
  zone: Zone;
  // Another GitHub user whose activity, as far as this connection's token
  // can see it, the connection watches.
  watch_user: string | null;
  // How often it syncs, in seconds, if not as often as the service's default.
  sync_interval: number | null;
  sync: SyncState;
  schedule: Schedule;
  // The hub's, shown read-only here: its sync is the hub's, as of the
  // last pull.
  from_hub: boolean;
};
// How a companion's pulls from its hub are going. Its schedule is stale
// as soon as a pull fails.
type HubStatus = {
  url: string;
  sync: SyncState;
  schedule: Schedule;
  connections: number;
  items: number;
};
// How a connection's syncs are scheduled now.
type Schedule = {
  interval_secs: number;
  running: boolean;
  // Its credential was rejected: scheduled syncs stopped until it is
  // verified again.
  needs_user: boolean;
  // It never synced, or not for a few intervals: its items aren't current.
  stale: boolean;
};
type ErrorKind = "auth" | "transient" | "inaccessible" | "invalid";
// What a connection's syncs did. Times are UTC, in RFC 3339.
type SyncState = {
  // When the last sync whose results were saved started.
  last_success: string | null;
  last_attempt: string | null;
  // When the next scheduled sync is due.
  next_due: string | null;
  // Syncs that failed since the last success.
  failures: number;
  error_kind: ErrorKind | null;
  error: string | null;
  // Until when the source asked for no more requests.
  held_until: string | null;
};
type Progress = {
  delivered: number;
  total: number;
  open: number;
  needs_attention: number;
  abandoned: number;
};
type Effort = {
  id: string;
  title: string;
  note: string;
  // Whether the title may leave this machine in the external export.
  share_title: boolean;
  progress: Progress;
  // It came from the hub, and is read-only here.
  from_hub: boolean;
};
// A person's own service, or a hub, which keeps external work only.
type Mode = "local" | "hub";
type Snapshot = {
  items: Item[];
  connections: Connection[];
  efforts: Effort[];
  mode: Mode;
  // The hub this companion pulls from, if one is set.
  hub: HubStatus | null;
};
const providers: Record<
  Provider,
  { name: string; short: string; color: string; live: boolean }
> = {
  jira: { name: "Jira", short: "JI", color: "#6b9bff", live: true },
  "gitlab-internal": {
    name: "Internal GitLab",
    short: "GL",
    color: "#fc936c",
    live: true,
  },
  errata: { name: "Errata", short: "ER", color: "#e9859c", live: false },
  "gitlab-public": {
    name: "Public GitLab",
    short: "GL",
    color: "#eab36c",
    live: true,
  },
  github: { name: "GitHub", short: "GH", color: "#c6cdd9", live: true },
  fedora: { name: "Fedora Forge", short: "FE", color: "#79bbea", live: true },
  bot: { name: "Bot", short: "BT", color: "#b9a3f0", live: true },
};
const names = Object.keys(providers) as Provider[];
const outcomes: Record<Outcome, string> = {
  open: "Open",
  delivered: "Delivered",
  closed_without_delivery: "Closed, not delivered",
  abandoned: "Abandoned",
  unknown: "Unknown",
};
const outcomeNames = Object.keys(outcomes) as Outcome[];
// What shows an outcome: an item, or a row of the Mine view.
type Outcomed = { status: string; outcome: Outcome; outcome_reason: string };
// Finished: delivered, or abandoned by a person.
const settled = (i: Outcomed) =>
  i.outcome === "delivered" || i.outcome === "abandoned";
// Never progress: a person decides what happened.
const needsDecision = (i: Outcomed) =>
  i.outcome === "closed_without_delivery" || i.outcome === "unknown";
const outcomeClass = (i: Outcomed) =>
  i.outcome === "delivered"
    ? "complete"
    : needsDecision(i)
      ? "attention"
      : i.outcome === "abandoned"
        ? "abandoned"
        : "";
function OutcomeIcon({ item, size }: { item: Outcomed; size: number }) {
  if (item.outcome === "delivered") return <Check size={size} />;
  if (needsDecision(item)) return <AlertCircle size={size} />;
  if (item.outcome === "abandoned") return <X size={size} />;
  return <Circle size={size - 4} fill="currentColor" />;
}
// When an ISO time is, seen from now: "in 4 min", "3 h ago".
function relative(iso: string, now = Date.now()) {
  const seconds = Math.round((new Date(iso).getTime() - now) / 1000);
  const span = Math.abs(seconds);
  if (span < 60) return seconds >= 0 ? "within a minute" : "just now";
  const amount =
    span < 7200
      ? `${Math.floor(span / 60)} min`
      : span < 172800
        ? `${Math.floor(span / 3600)} h`
        : `${Math.floor(span / 86400)} d`;
  return seconds >= 0 ? `in ${amount}` : `${amount} ago`;
}
function every(seconds: number) {
  return seconds % 86400 === 0
    ? `${seconds / 86400}d`
    : seconds % 3600 === 0
      ? `${seconds / 3600}h`
      : seconds % 60 === 0
        ? `${seconds / 60}m`
        : `${seconds}s`;
}
// How a connection's syncs are going, and whether that needs attention.
function syncStatus(c: Connection): { text: string; attention: boolean } {
  const s = c.sync;
  if (c.from_hub)
    return {
      text: `${c.schedule.stale ? "Stale: synced" : "Synced"} by the hub, ${s.last_success ? `last ${relative(s.last_success)}` : "never yet"}${s.error_kind ? "; its last sync there failed" : ""}. Read-only here; manage it on the hub.`,
      attention: c.schedule.stale || s.error_kind !== null,
    };
  const last = s.last_success
    ? `${c.schedule.stale ? "Stale: last" : "Last"} synced ${relative(s.last_success)}.`
    : "Never synced.";
  // Past due: the schedule starts it within moments.
  const next = !s.next_due
    ? "soon"
    : new Date(s.next_due).getTime() <= Date.now()
      ? "due now"
      : relative(s.next_due);
  if (c.schedule.needs_user)
    return {
      text: `Needs you: ${s.error ?? "credential rejected"}. Scheduled syncs stopped until the credential works; fix it, then verify again. ${last}`,
      attention: true,
    };
  if (c.schedule.running) return { text: `Syncing now. ${last}`, attention: false };
  if (!s.error)
    return {
      text: `${last} Next sync ${next}, every ${every(c.schedule.interval_secs)}.`,
      attention: c.schedule.stale && s.last_success !== null,
    };
  const held = s.held_until && new Date(s.held_until).getTime() > Date.now();
  return {
    text: held
      ? `${s.error}. The source asked to wait; next try ${next}. ${last}`
      : `${s.error}. ${s.failures} failed ${s.failures === 1 ? "sync" : "syncs"} in a row, backing off; next try ${next}. ${last}`,
    attention: true,
  };
}
// How the pulls from the hub are going.
function hubStatusText(h: HubStatus): string {
  const last = h.sync.last_success
    ? `Last pulled ${relative(h.sync.last_success)}.`
    : "Never pulled.";
  if (h.schedule.needs_user)
    return `Needs you: ${h.sync.error ?? "the hub refused the token"}. Pulls stopped until you run workboard hub set with its current companion token. ${last}`;
  if (h.sync.error)
    return `${h.sync.error}. Its work is stale until a pull works again. ${last}`;
  return `${h.schedule.stale ? "Stale: " : ""}${last}`;
}
function hubAttention(h: HubStatus): boolean {
  return h.schedule.stale || h.schedule.needs_user;
}
// How many of its intervals after its last success a connection's items
// are stale, as the service decides.
const STALE_INTERVALS = 3;
// Whether data last synced at `at` is past the stale threshold of
// something that syncs every `interval` seconds, at `now`.
function pastThreshold(interval: number, at: string | null, now: number) {
  return (
    !at || now - new Date(at).getTime() > STALE_INTERVALS * interval * 1000
  );
}
// The snapshot as of `now`. The service marks staleness when the page
// fetches; this keeps the marks true while the page shows what it fetched:
// everything synced is stale once the service can't be reached, and a
// connection's items once its last success is past the threshold. The
// hub's work is stale once its last pull is: a hub connection's own sync
// time is the hub's, whose interval isn't known here.
function age(d: Snapshot, offline: boolean, now: number): Snapshot {
  const hubStale =
    offline ||
    !d.hub ||
    d.hub.schedule.stale ||
    pastThreshold(d.hub.schedule.interval_secs, d.hub.sync.last_success, now);
  const stale = new Map(
    d.connections.map((c) => [
      c.id,
      c.from_hub
        ? hubStale || c.schedule.stale
        : offline ||
          c.schedule.stale ||
          pastThreshold(c.schedule.interval_secs, c.sync.last_success, now),
    ]),
  );
  return {
    ...d,
    hub: d.hub && {
      ...d.hub,
      schedule: { ...d.hub.schedule, stale: hubStale },
    },
    connections: d.connections.map((c) => ({
      ...c,
      schedule: { ...c.schedule, stale: stale.get(c.id) ?? true },
    })),
    items: d.items.map((i) =>
      i.connection_id
        ? { ...i, stale: i.stale || (stale.get(i.connection_id) ?? true) }
        : i,
    ),
  };
}
// The Mine view as of `now`, the same way, by the connections that found
// each row, as `age` left them. Each is judged by its own last success and
// interval, so a connection that syncs less often doesn't make another's
// view stale.
function ageMine(
  m: MineView,
  connections: Connection[],
  offline: boolean,
  now: number,
): MineView {
  const byId = new Map(connections.map((c) => [c.id, c]));
  const row = (r: MineRow) => ({
    ...r,
    stale:
      r.stale ||
      offline ||
      r.waits_on.some((w) => {
        const c = byId.get(w.connection_id);
        // A hub connection is aged by the hub's pulls, in `age`.
        return (
          w.stale ||
          !c ||
          c.schedule.stale ||
          (!c.from_hub &&
            pastThreshold(c.schedule.interval_secs, w.synced_at, now))
        );
      }),
  });
  return { ...m, mine: m.mine.map(row), watching: m.watching.map(row) };
}
// Marks what is stale, so it is never taken as current.
function Stale({ item }: { item: { stale: boolean; synced_at: string | null } }) {
  if (!item.stale) return null;
  return (
    <>
      <span className="meta-dot">·</span>
      <em className="stale-tag">
        {item.synced_at
          ? `Stale: last synced ${relative(item.synced_at)}`
          : "Stale: not synced"}
      </em>
    </>
  );
}
function progressText(p: Progress) {
  const parts = [
    p.open && `${p.open} open`,
    p.needs_attention &&
      `${p.needs_attention} ${p.needs_attention === 1 ? "needs" : "need"} attention`,
    p.abandoned && `${p.abandoned} abandoned`,
  ].filter(Boolean);
  return parts.length ? parts.join(" · ") : "Nothing remaining";
}
// How the service signs browsers in, and who this one is.
type AuthStatus = {
  github_app: boolean;
  pairing: boolean;
  sign_in: string | null;
  user: { id: number; login: string } | null;
};
async function authStatus(): Promise<AuthStatus | null> {
  const response = await fetch("/auth/status", { credentials: "same-origin" });
  return response.ok ? ((await response.json()) as AuthStatus) : null;
}
// A sign-in step of the hub's GitHub App; the browser sends its origin.
async function authStep(path: string): Promise<Record<string, string>> {
  const response = await fetch(`/auth/${path}`, {
    method: "POST",
    credentials: "same-origin",
  });
  const data = await response.json();
  if (!response.ok)
    throw new Error(data.error ?? `Request failed (${response.status})`);
  return data;
}
// The header of the page's own refreshes, which a hub doesn't count as a
// use of the session, so an open page left alone still goes idle.
const BACKGROUND = { "X-Workboard-Background": "1" };
async function api<T>(
  path: string,
  body?: unknown,
  method?: string,
  background = false,
): Promise<T> {
  const response = await fetch(`/api/${path}`, {
    method: method ?? (body === undefined ? "GET" : "POST"),
    credentials: "same-origin",
    headers: {
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      ...(background ? BACKGROUND : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await response.json();
  if (!response.ok)
    throw new Error(data.error ?? `Request failed (${response.status})`);
  return data as T;
}
function Mark({
  provider,
  small = false,
}: {
  provider: Provider;
  small?: boolean;
}) {
  const p = providers[provider];
  return (
    <span
      className={`source-mark ${small ? "small" : ""}`}
      style={{ color: p.color }}
    >
      {p.short}
    </span>
  );
}
function Status({ item }: { item: Outcomed }) {
  const kind =
    outcomeClass(item) ||
    (item.status.toLowerCase().includes("fail") ? "failed" : "");
  return (
    <span
      className={`status ${kind}`}
      title={`${outcomes[item.outcome]}: ${item.outcome_reason}`}
    >
      <OutcomeIcon item={item} size={12} />
      {item.status}
      {item.outcome !== "open" && (
        <span className="outcome">· {outcomes[item.outcome]}</span>
      )}
    </span>
  );
}
function MineList({
  title,
  note,
  rows,
  empty,
}: {
  title: string;
  note: string;
  rows: MineRow[];
  empty: string;
}) {
  return (
    <section className="inbox-section">
      <div className="section-heading">
        <div>
          <h2>
            {title} <span>{rows.length}</span>
          </h2>
          <p>{note}</p>
        </div>
      </div>
      <div className="work-list">
        {rows.length === 0 ? (
          <div className="empty">
            <UserCheck size={28} />
            <h3>Nothing waits here</h3>
            <p>{empty}</p>
          </div>
        ) : (
          rows.map((r) => (
            <div className="work-row mine-row" key={r.id}>
              <Mark provider={r.provider} />
              <a
                className="item-title"
                href={r.url}
                target="_blank"
                rel="noopener noreferrer"
              >
                <strong>{r.title}</strong>
                <span>
                  {r.key} <span className="meta-dot">·</span>{" "}
                  {providers[r.provider].name}
                  <Stale item={r} />
                </span>
                {r.waits_on.map((w) => (
                  <span key={`${w.account} ${w.connection_id}`}>
                    {w.account}:
                    {w.reasons.map((reason) => (
                      <em className="reason-tag" key={reason}>
                        {reasonLabels[reason]}
                      </em>
                    ))}
                  </span>
                ))}
              </a>
              <Status item={r} />
              <em className={`zone-tag ${r.zone}`}>
                {r.zone === "external" ? "External" : "Internal"}
              </em>
              {r.from_hub && (
                <em className="zone-tag hub" title="From the hub: read-only here">
                  Hub
                </em>
              )}
              <a
                className="open-source"
                href={r.url}
                target="_blank"
                rel="noopener noreferrer"
                aria-label={`Open ${r.title} at source`}
              >
                <ExternalLink size={16} />
              </a>
            </div>
          ))
        )}
      </div>
    </section>
  );
}
// How to ask the bot about an item: a new issue, or a comment on the item,
// whose start is shown to copy, since GitHub takes no comment text in a link.
function AskLinks({ ask, about }: { ask: Ask; about: boolean }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="ask-links">
      <a
        className="primary"
        href={webLink(ask.issue_url)}
        target="_blank"
        rel="noopener noreferrer"
      >
        <Bot size={15} />
        {about ? "Ask the bot about this" : "Ask the bot"}
      </a>
      {ask.comment_url && ask.comment && (
        <>
          <a
            className="quiet"
            href={webLink(ask.comment_url)}
            target="_blank"
            rel="noopener noreferrer"
          >
            Comment on it
          </a>
          <span className="ask-comment">
            Start the comment with <code>{ask.comment}</code>
            <button
              className="icon-button"
              aria-label="Copy the start of the comment"
              onClick={() =>
                navigator.clipboard
                  ?.writeText(ask.comment ?? "")
                  .then(() => setCopied(true))
                  .catch(() => setCopied(false))
              }
            >
              {copied ? <Check size={14} /> : <Copy size={14} />}
            </button>
          </span>
        </>
      )}
      <p className="token-note">
        Opens GitHub in your browser, prefilled; nothing is sent until you
        submit it there, as yourself.
      </p>
    </div>
  );
}
// A connected bot: its board's counts, its coordinator's heartbeat, and
// the agent runs it lists.
function BotCard({ bot, now }: { bot: BotView; now: number }) {
  const board = bot.agents?.board;
  const total = Object.values(board?.statuses ?? {}).reduce(
    (a, b) => a + b,
    0,
  );
  const heartbeat = bot.agents?.heartbeat;
  return (
    <article className="connection-card bot-card">
      <div className="connection-top">
        <Mark provider="bot" />
        <h2>{bot.label}</h2>
        {bot.from_hub && (
          <em
            className="zone-tag hub"
            title="The hub's bot, as of the last pull: read-only here. Ask links are built here, and open GitHub in your browser."
          >
            Hub
          </em>
        )}
        <a
          className="connection-kind live"
          href={webLink(bot.board_url)}
          target="_blank"
          rel="noopener noreferrer"
        >
          Board
        </a>
      </div>
      {board ? (
        <>
          <p className="connection-description">
            {total} board items
            {Object.entries(board.statuses).map(([status, n]) => (
              <em className="reason-tag" key={status}>
                {n} {status}
              </em>
            ))}{" "}
            · {board.fork_prs} open fork pull requests
          </p>
          {board.unreadable > 0 && (
            <p className="connection-description danger">
              And {board.unreadable} items this token can't read: the board
              shown isn't all of it.
            </p>
          )}
        </>
      ) : (
        <p className="connection-description">
          The board hasn't been read yet.
        </p>
      )}
      {!bot.agents ? (
        <p className="connection-description">
          The heartbeat hasn't been read yet.
        </p>
      ) : !heartbeat ? (
        <p className="connection-description">
          {bot.bot_login} publishes no heartbeat.
        </p>
      ) : (
        <>
          <p
            className={
              heartbeat.stale ? "connection-description danger" : "connection-description"
            }
          >
            {heartbeat.stale && "Stale, the coordinator may have stopped: "}
            Coordinator <code>{heartbeat.session}</code> is{" "}
            {heartbeat.loop_state}, as of {relative(heartbeat.updated_at, now)}
            {heartbeat.next_wake_at &&
              `; next wake ${relative(heartbeat.next_wake_at, now)}`}
            {bot.agents.stale && (
              <em className="stale-tag"> · read {bot.agents.last_success ? relative(bot.agents.last_success, now) : "never"}</em>
            )}
          </p>
          <div className="work-list">
            {heartbeat.runs.length === 0 ? (
              <div className="empty">
                <h3>No agent runs</h3>
                <p>The coordinator runs no workers right now.</p>
              </div>
            ) : (
              heartbeat.runs.map((r) => (
                <div className="work-row" key={r.name}>
                  <Bot size={16} />
                  <a
                    className="item-title"
                    href={webLink(r.item_url)}
                    target="_blank"
                    rel="noopener noreferrer"
                  >
                    <strong>{r.name}</strong>
                    <span>
                      {r.item_key} <span className="meta-dot">·</span> started{" "}
                      {relative(r.started_at, now)}
                      {r.devspace && (
                        <>
                          <span className="meta-dot">·</span>on {r.devspace}
                        </>
                      )}
                    </span>
                  </a>
                  <span className="status">{r.status}</span>
                </div>
              ))
            )}
          </div>
          {heartbeat.skipped > 0 && (
            <p className="detail-note">
              {heartbeat.skipped} more workers left out, as malformed.
            </p>
          )}
        </>
      )}
      {bot.agents?.error && (
        <p className="connection-description danger">
          {bot.agents.error}. Showing the last heartbeat read.
        </p>
      )}
      <AskLinks ask={bot.ask} about={false} />
    </article>
  );
}
function App() {
  // What the service last sent; `data` and `mineView` are it as of now.
  const [fetched, setData] = useState<Snapshot>({
    items: [],
    connections: [],
    efforts: [],
    mode: "local",
    hub: null,
  });
  const [page, setPage] = useState<
    "inbox" | "mine" | "bot" | "efforts" | "connections"
  >("inbox");
  // The bots connected here: their heartbeats, and how to ask them.
  const [bots, setBots] = useState<BotView[]>([]);
  // Why the bots' view couldn't be read, if it couldn't.
  const [botsError, setBotsError] = useState("");
  const [fetchedMine, setMineView] = useState<MineView>({
    mine: [],
    watching: [],
    watch_errors: [],
  });
  // When the page last fetched from the service, and since when it can't.
  const [fetchedAt, setFetchedAt] = useState<number | null>(null);
  const [offlineSince, setOfflineSince] = useState<number | null>(null);
  // Moves on with the clock, so freshness is worked out again.
  const [now, setNow] = useState(() => Date.now());
  const data = useMemo(
    () => age(fetched, offlineSince !== null, now),
    [fetched, offlineSince, now],
  );
  const mineView = useMemo(
    () => ageMine(fetchedMine, data.connections, offlineSince !== null, now),
    [fetchedMine, data, offlineSince, now],
  );
  const [loading, setLoading] = useState(true);
  const [paired, setPaired] = useState(false);
  const [auth, setAuth] = useState<AuthStatus | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [query, setQuery] = useState("");
  const [source, setSource] = useState("all");
  const [attention, setAttention] = useState("all");
  const [selected, setSelected] = useState<string[]>([]);
  const [detail, setDetail] = useState<string | null>(null);
  const [modal, setModal] = useState<"effort" | "connect" | "manual" | null>(
    null,
  );
  const [connectProvider, setConnectProvider] = useState<Provider>("jira");
  const [busy, setBusy] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [form, setForm] = useState<Record<string, string>>({});
  const [manualStatus, setManualStatus] = useState("");
  const [manualAction, setManualAction] = useState("next");
  const [manualOutcome, setManualOutcome] = useState<Outcome>("open");
  useEffect(() => {
    if (!modal) return;
    const previous = document.activeElement as HTMLElement | null;
    const dialog = document.querySelector<HTMLElement>(".modal");
    const elements = () =>
      Array.from(
        dialog?.querySelectorAll<HTMLElement>(
          "button:not(:disabled), input, select, a[href]",
        ) ?? [],
      );
    const firstInput = dialog?.querySelector<HTMLElement>("input, select");
    firstInput?.focus();
    function keyboard(event: KeyboardEvent) {
      if (event.key === "Escape" && !busy) {
        setModal(null);
        setForm({});
      }
      if (event.key === "Tab") {
        const list = elements();
        const first = list[0];
        const last = list[list.length - 1];
        if (event.shiftKey && document.activeElement === first) {
          event.preventDefault();
          last?.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first?.focus();
        }
      }
    }
    document.addEventListener("keydown", keyboard);
    return () => {
      document.removeEventListener("keydown", keyboard);
      previous?.focus();
    };
  }, [modal, busy]);
  const demo = data.items.some((i) => i.demo);
  // A hub keeps external work only: no manual links, no internal connections.
  const hub = data.mode === "hub";
  const active = data.items.filter((i) => !settled(i));
  const detailItem = data.items.find((i) => i.id === detail);
  useEffect(() => {
    if (detailItem) {
      setManualStatus(detailItem.status);
      setManualAction(detailItem.action || "next");
      setManualOutcome(detailItem.outcome);
    }
  }, [
    detailItem?.id,
    detailItem?.status,
    detailItem?.action,
    detailItem?.outcome,
  ]);
  async function reload(background = false) {
    const [next, waiting] = await Promise.all([
      api<Snapshot>("snapshot", undefined, undefined, background),
      api<MineView>("mine", undefined, undefined, background),
    ]);
    // Only the bot's section is unavailable if its view fails, not the
    // page; it keeps what it last had.
    const bot = await api<BotView[]>("bot", undefined, undefined, background).then(
      (b) => {
        setBotsError("");
        return b;
      },
      (e: Error) => {
        setBotsError(e.message);
        return null;
      },
    );
    setData(next);
    setMineView(waiting);
    if (bot) setBots(bot);
    setPaired(true);
    setFetchedAt(Date.now());
    setNow(Date.now());
    setOfflineSince(null);
  }
  useEffect(() => {
    (async () => {
      try {
        const token = new URLSearchParams(location.hash.slice(1)).get("token");
        if (token) {
          history.replaceState(null, "", location.pathname);
          const response = await fetch("/api/session", {
            method: "POST",
            headers: { Authorization: `Bearer ${token}` },
          });
          if (!response.ok)
            throw new Error(
              "Pairing expired. Reopen the URL printed by the service.",
            );
        }
        setAuth(await authStatus().catch(() => null));
        await reload();
      } catch (e) {
        setError((e as Error).message);
      } finally {
        setLoading(false);
      }
    })();
    // Keep the last view when the service can't be reached, but marked
    // stale, never as current.
    const poll = () => {
      setNow(Date.now());
      reload(true).catch(() =>
        setOfflineSince((since) => since ?? Date.now()),
      );
    };
    const timer = window.setInterval(poll, 30000);
    // A page that was hidden or asleep looks again as soon as it is seen.
    const visible = () => {
      if (document.visibilityState === "visible") poll();
    };
    document.addEventListener("visibilitychange", visible);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", visible);
    };
  }, []);
  const filtered = useMemo(
    () =>
      data.items.filter(
        (i) =>
          (attention === "complete" ? settled(i) : !settled(i)) &&
          (source === "all" || i.provider === source) &&
          (attention === "all" ||
            attention === "complete" ||
            (attention === "decide"
              ? needsDecision(i)
              : i.action === attention ||
                // Like `workboard next`: a decision is a next action too.
                (attention === "next" && needsDecision(i)))) &&
          `${i.title} ${i.key} ${providers[i.provider].name}`
            .toLowerCase()
            .includes(query.toLowerCase()),
      ),
    [data, source, attention, query],
  );
  function openModal(type: typeof modal, provider: Provider = "jira") {
    setForm({ provider, status: "Open", action: "next", outcome: "open" });
    setConnectProvider(provider);
    setError("");
    setModal(type);
  }
  async function sync() {
    setSyncing(true);
    setError("");
    setNotice("");
    try {
      const r = await api<{
        results: { error?: string; items?: number; label: string }[];
      }>("sync", {});
      await reload();
      const errors = r.results.filter((r) => r.error);
      if (errors.length)
        setError(errors.map((r) => `${r.label}: ${r.error}`).join(" · "));
      else
        setNotice(
          r.results.length
            ? "Connected sources are up to date."
            : "Connect Jira, GitLab or GitHub to start syncing.",
        );
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setSyncing(false);
    }
  }
  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      if (modal === "effort")
        await api("efforts", {
          title: form.title,
          note: form.note ?? "",
          item_ids: selected,
        });
      if (modal === "connect")
        await api("connections", {
          provider: connectProvider,
          label: form.label ?? providers[connectProvider].name,
          base_url: form.url,
          token: form.token,
          // The person's explicit choice, on a hub too, where the form
          // requires it and the service refuses anything else.
          zone: form.zone === "external" ? "external" : "internal",
          watch_user:
            connectProvider === "github" && form.watch?.trim()
              ? form.watch.trim()
              : null,
          bot:
            connectProvider === "bot"
              ? {
                  board: form.board ?? "",
                  tracker: form.tracker ?? "",
                  heartbeat_issue: Number(form.heartbeat),
                  bot_login: form.bot_login ?? "",
                  forge_org: form.forge_org ?? "",
                }
              : null,
        });
      if (modal === "manual")
        await api("items", {
          provider: form.provider,
          key: form.key ?? "",
          title: form.title,
          url: form.url,
          status: form.status,
          action: form.action,
          outcome: form.outcome,
        });
      await reload();
      setModal(null);
      setForm({});
      setSelected([]);
      setNotice(
        modal === "connect"
          ? "Account verified. Its first sync starts now, then it syncs on schedule."
          : "Saved.",
      );
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  async function link(item: string, effort: string) {
    try {
      await api("links", { item, effort: effort || null });
      await reload();
    } catch (e) {
      setError((e as Error).message);
    }
  }
  async function shareTitle(e: Effort, share: boolean) {
    try {
      await api(
        `efforts/${encodeURIComponent(e.id)}`,
        { share_title: share },
        "PATCH",
      );
      await reload();
    } catch (err) {
      setError((err as Error).message);
    }
  }
  async function verify(c: Connection) {
    setError("");
    try {
      await api(`connections/${encodeURIComponent(c.id)}/verify`, {});
      await reload();
      setNotice(`${c.label} works again and syncs on schedule.`);
    } catch (e) {
      setError((e as Error).message);
    }
  }
  async function disconnect(c: Connection) {
    if (
      !window.confirm(
        `Disconnect ${c.label}? This removes its credential and cached items from this app.`,
      )
    )
      return;
    try {
      await api(
        `connections/${encodeURIComponent(c.id)}`,
        undefined,
        "DELETE",
      );
      await reload();
    } catch (e) {
      setError((e as Error).message);
    }
  }
  async function updateManual() {
    if (!detailItem) return;
    setBusy(true);
    try {
      await api(
        `items/${encodeURIComponent(detailItem.id)}`,
        { status: manualStatus, action: manualAction, outcome: manualOutcome },
        "PATCH",
      );
      await reload();
      setNotice("Manual status updated.");
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  async function abandon(item: Item, abandoned: boolean) {
    setBusy(true);
    try {
      await api(`items/${encodeURIComponent(item.id)}/abandon`, {
        abandoned,
      });
      await reload();
      setNotice(abandoned ? "Marked abandoned." : "Abandoned mark removed.");
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  const field = (
    name: string,
    label: string,
    type = "text",
    required = true,
    placeholder = "",
  ) => (
    <label className="form-field">
      {label}
      <input
        name={name}
        type={type}
        value={form[name] ?? ""}
        required={required}
        placeholder={placeholder}
        maxLength={name === "token" ? 16384 : 500}
        onChange={(e) => setForm((f) => ({ ...f, [name]: e.target.value }))}
        autoComplete={type === "password" ? "off" : undefined}
      />
    </label>
  );
  if (loading)
    return (
      <div className="pair-screen">
        <div className="brand-symbol">w</div>
        <h1>Opening your work…</h1>
      </div>
    );
  async function signIn() {
    try {
      const { url } = await authStep("github/login");
      location.assign(url);
    } catch (e) {
      setError((e as Error).message);
    }
  }
  async function signOut() {
    try {
      await authStep("logout");
      location.reload();
    } catch (e) {
      setError((e as Error).message);
    }
  }
  if (!paired && auth?.github_app) {
    const here = auth.sign_in === `${location.origin}/`;
    return (
      <div className="pair-screen">
        <div className="brand-symbol">w</div>
        <h1>Sign in to the hub</h1>
        <p>Sign in with the GitHub account this hub allows.</p>
        {here ? (
          <button onClick={signIn}>Sign in with GitHub</button>
        ) : (
          <a href={auth.sign_in ?? "/"}>Open {auth.sign_in} to sign in</a>
        )}
        <p className="muted">{error}</p>
      </div>
    );
  }
  if (!paired)
    return (
      <div className="pair-screen">
        <div className="brand-symbol">w</div>
        <h1>Pair your browser</h1>
        <p>Start your local service and open the private URL it prints.</p>
        <code>workboard serve</code>
        <p className="muted">{error}</p>
        <button onClick={() => location.reload()}>Try again</button>
      </div>
    );
  return (
    <div className="app-shell">
      <aside className="sidebar">
        <a className="brand" href="/" aria-label="Workboard home">
          <span className="brand-symbol">w</span>workboard
          <span className="version">α</span>
        </a>
        <div className="workspace">
          <span className="avatar">ME</span>
          <div>
            <strong>My workspace</strong>
            <span>Personal · {hub ? "Hub" : "Local"}</span>
            {auth?.user && (
              <span>
                Signed in as {auth.user.login} ·{" "}
                <button className="link-button" onClick={signOut}>
                  Sign out
                </button>
              </span>
            )}
          </div>
        </div>
        <div className="nav-caption">WORKSPACE</div>
        <nav>
          <button
            className={page === "inbox" ? "nav-item active" : "nav-item"}
            onClick={() => {
              setPage("inbox");
              setDetail(null);
            }}
          >
            <Inbox size={18} />
            My inbox<span className="nav-count">{active.length}</span>
          </button>
          <button
            className={page === "mine" ? "nav-item active" : "nav-item"}
            onClick={() => {
              setPage("mine");
              setDetail(null);
            }}
          >
            <UserCheck size={18} />
            Mine<span className="nav-count">{mineView.mine.length}</span>
          </button>
          {(bots.length > 0 || botsError) && (
            <button
              className={page === "bot" ? "nav-item active" : "nav-item"}
              onClick={() => {
                setPage("bot");
                setDetail(null);
              }}
            >
              <Bot size={18} />
              Bot
              <span className="nav-count">
                {bots.reduce(
                  (n, b) => n + (b.agents?.heartbeat?.runs.length ?? 0),
                  0,
                )}
              </span>
            </button>
          )}
          <button
            className={page === "efforts" ? "nav-item active" : "nav-item"}
            onClick={() => {
              setPage("efforts");
              setDetail(null);
            }}
          >
            <Layers size={18} />
            Efforts<span className="nav-count">{data.efforts.length}</span>
          </button>
          <button
            className={page === "connections" ? "nav-item active" : "nav-item"}
            onClick={() => {
              setPage("connections");
              setDetail(null);
            }}
          >
            <Plug size={18} />
            Connections
          </button>
        </nav>
        <div className="nav-caption source-caption">
          SOURCES <span>{names.length}</span>
        </div>
        <div className="source-nav">
          {names.map((p) => (
            <button
              key={p}
              onClick={() => {
                setSource(p);
                setAttention("all");
                setPage("inbox");
              }}
            >
              <Mark provider={p} small />
              <span>{providers[p].name}</span>
              <span className="source-count">
                {active.filter((i) => i.provider === p).length || "–"}
              </span>
            </button>
          ))}
        </div>
        <div className="sidebar-footer">
          <Terminal size={17} />
          <div>
            <strong>Same work. Your terminal.</strong>
            <code>workboard next</code>
          </div>
        </div>
      </aside>
      <main>
        <header className="topbar">
          <span>
            My workspace <ChevronRight size={14} />
            <strong>
              {page === "inbox"
                ? "Inbox"
                : page === "mine"
                  ? "Mine"
                  : page === "bot"
                    ? "Bot"
                    : page === "efforts"
                      ? "Efforts"
                      : "Connections"}
            </strong>
          </span>
          <div className="topbar-right">
            {demo && <span className="demo-tag">DEMO DATA</span>}
            <span className="local-tag">
              <span />
              {hub ? "Hub: external work only" : "Local instance"}
            </span>
          </div>
        </header>
        <div className="main-content">
          <div className="page-heading">
            <div>
              <div className="eyebrow">YOUR WORK, CONNECTED</div>
              <h1>
                {page === "inbox"
                  ? "What needs your attention?"
                  : page === "mine"
                    ? "What waits on you?"
                    : page === "bot"
                      ? "What is the bot doing?"
                      : page === "efforts"
                      ? "From upstream to shipped."
                      : "Connect your work."}
              </h1>
              <p>
                {page === "inbox"
                  ? "One inbox for reviews, next steps, and work waiting on others."
                  : page === "mine"
                    ? "Review requests, assignments, mentions and your changes that need action, from every account. Each shows once, with every reason."
                    : page === "bot"
                      ? "Its coordinator's heartbeat and the agent runs it lists. Its board items and fork pull requests are in your inbox."
                      : page === "efforts"
                      ? "Keep related tickets, changes, and advisories together."
                      : "Your accounts. Your access. Each service keeps its own identity."}
              </p>
            </div>
            <div className="heading-actions">
              {page === "connections" ? (
                <button
                  className="primary"
                  onClick={() => openModal("connect")}
                >
                  <Plus size={16} />
                  Connect source
                </button>
              ) : page === "mine" || page === "bot" ? (
                <button className="quiet" onClick={sync} disabled={syncing}>
                  <RefreshCw size={15} className={syncing ? "spin" : ""} />
                  {syncing ? "Syncing…" : "Sync sources"}
                </button>
              ) : (
                <>
                  {!hub && (
                    <button
                      className="quiet"
                      onClick={() => openModal("manual")}
                    >
                      <Plus size={16} />
                      Add source link
                    </button>
                  )}
                  <button
                    className="primary"
                    onClick={() => openModal("effort")}
                  >
                    <Layers size={16} />
                    New effort
                  </button>
                </>
              )}
            </div>
          </div>
          {offlineSince !== null && (
            <div className="message error" role="alert">
              <AlertCircle size={17} />
              <span>
                Can't reach the Workboard service since{" "}
                {new Date(offlineSince).toLocaleTimeString()}. This is what it
                had{" "}
                {fetchedAt
                  ? `at ${new Date(fetchedAt).toLocaleTimeString()}`
                  : "before"}
                , all marked stale.
              </span>
            </div>
          )}
          {error && !modal && (
            <div className="message error" role="alert">
              <AlertCircle size={17} />
              <span>{error}</span>
              <button aria-label="Dismiss error" onClick={() => setError("")}>
                <X size={15} />
              </button>
            </div>
          )}
          {notice && (
            <div className="message" role="status">
              <Check size={17} />
              <span>{notice}</span>
              <button
                aria-label="Dismiss notification"
                onClick={() => setNotice("")}
              >
                <X size={15} />
              </button>
            </div>
          )}
          {page === "inbox" && (
            <>
              <div className="summary-grid">
                {[
                  {
                    name: "Next actions",
                    value: active.filter(
                      (i) => i.action === "next" || needsDecision(i),
                    ).length,
                    filter: "next",
                    icon: <ArrowUpRight size={18} />,
                    color: "coral",
                    desc: "Ready for you to move forward",
                  },
                  {
                    name: "Reviews",
                    value: active.filter((i) => i.action === "review").length,
                    filter: "review",
                    icon: <GitPullRequest size={18} />,
                    color: "blue",
                    desc: "Changes that need your eyes",
                  },
                  {
                    name: "Waiting",
                    value: active.filter((i) => i.action === "waiting").length,
                    filter: "waiting",
                    icon: <Circle size={17} />,
                    color: "amber",
                    desc: "With a reviewer or another team",
                  },
                ].map((s) => (
                  <button
                    className={`summary-card ${s.color} ${attention === s.filter ? "chosen" : ""}`}
                    key={s.name}
                    onClick={() =>
                      setAttention(attention === s.filter ? "all" : s.filter)
                    }
                  >
                    <div>
                      {s.icon}
                      <span>{s.name}</span>
                    </div>
                    <strong>{s.value.toString().padStart(2, "0")}</strong>
                    <p>{s.desc}</p>
                  </button>
                ))}
              </div>
              {data.efforts.length > 0 && (
                <section className="featured-effort">
                  <div className="featured-icon">
                    <Layers size={21} />
                  </div>
                  <div className="featured-text">
                    <span>
                      IN FLIGHT · {data.efforts.length}{" "}
                      {data.efforts.length === 1 ? "EFFORT" : "EFFORTS"}
                    </span>
                    <h3>{data.efforts[0].title}</h3>
                    <div>
                      {data.items
                        .filter((i) => i.effort_id === data.efforts[0].id)
                        .map((i) => (
                          <span className="mini-stage" key={i.id}>
                            <span
                              className={`stage-${outcomeClass(i)}`}
                              title={`${outcomes[i.outcome]}: ${i.outcome_reason}`}
                            >
                              <OutcomeIcon item={i} size={11} />
                            </span>
                            {providers[i.provider].name}
                          </span>
                        ))}
                    </div>
                  </div>
                  <button className="quiet" onClick={() => setPage("efforts")}>
                    View effort
                    <ChevronRight size={16} />
                  </button>
                </section>
              )}
              <section className="inbox-section">
                <div className="section-heading">
                  <div>
                    <h2>
                      My inbox <span>{filtered.length}</span>
                    </h2>
                    <p>
                      {selected.length
                        ? `${selected.length} selected. Create an effort to group them.`
                        : "Source statuses stay visible. Every item links back to its home."}
                    </p>
                  </div>
                  <button className="quiet" onClick={sync} disabled={syncing}>
                    <RefreshCw size={15} className={syncing ? "spin" : ""} />
                    {syncing ? "Syncing…" : "Sync sources"}
                  </button>
                </div>
                <div className="toolbar">
                  <label className="search">
                    <Search size={16} />
                    <input
                      aria-label="Search work"
                      placeholder="Search your work…"
                      value={query}
                      onChange={(e) => setQuery(e.target.value)}
                    />
                  </label>
                  <select
                    aria-label="Filter by source"
                    value={source}
                    onChange={(e) => setSource(e.target.value)}
                  >
                    <option value="all">All sources</option>
                    {names.map((p) => (
                      <option key={p} value={p}>
                        {providers[p].name}
                      </option>
                    ))}
                  </select>
                  <select
                    aria-label="Filter by attention"
                    value={attention}
                    onChange={(e) => setAttention(e.target.value)}
                  >
                    <option value="all">All active work</option>
                    <option value="next">Next actions</option>
                    <option value="review">Reviews</option>
                    <option value="waiting">Waiting</option>
                    <option value="follow-up">Follow up</option>
                    <option value="decide">Needs a decision</option>
                    <option value="complete">Delivered or abandoned</option>
                  </select>
                </div>
                <div className="work-list">
                  {filtered.length === 0 ? (
                    <div className="empty">
                      <Inbox size={28} />
                      <h3>
                        {data.items.length
                          ? "Nothing in this view"
                          : "Your inbox starts here"}
                      </h3>
                      <p>
                        {data.items.length
                          ? "Try another source, attention filter, or search."
                          : "Connect Jira, GitLab or GitHub; your work comes in with its first sync."}
                      </p>
                      <button
                        className="quiet"
                        onClick={() =>
                          data.items.length
                            ? (setSource("all"),
                              setAttention("all"),
                              setQuery(""))
                            : setPage("connections")
                        }
                      >
                        {data.items.length
                          ? "Clear filters"
                          : "Set up connections"}
                      </button>
                    </div>
                  ) : (
                    filtered.map((i) => (
                      <div
                        className={`work-row ${detail === i.id ? "focused" : ""}`}
                        key={i.id}
                      >
                        <input
                          type="checkbox"
                          aria-label={`Select ${i.title}`}
                          disabled={i.from_hub}
                          title={
                            i.from_hub
                              ? "From the hub: read-only here"
                              : undefined
                          }
                          checked={selected.includes(i.id)}
                          onChange={(e) =>
                            setSelected((s) =>
                              e.target.checked
                                ? [...s, i.id]
                                : s.filter((id) => id !== i.id),
                            )
                          }
                        />
                        <Mark provider={i.provider} />
                        <button
                          className="item-title"
                          onClick={() => setDetail(i.id)}
                        >
                          <strong>{i.title}</strong>
                          <span>
                            {i.key} <span className="meta-dot">·</span>{" "}
                            {providers[i.provider].name}
                            {i.effort_id && (
                              <>
                                <span className="meta-dot">·</span>
                                <Layers size={11} />
                                {
                                  data.efforts.find((e) => e.id === i.effort_id)
                                    ?.title
                                }
                              </>
                            )}
                            {!i.connection_id && !i.demo && (
                              <>
                                <span className="meta-dot">·</span>Manual
                              </>
                            )}
                            {i.from_hub && (
                              <>
                                <span className="meta-dot">·</span>From the
                                hub
                              </>
                            )}
                            <Stale item={i} />
                          </span>
                        </button>
                        <Status item={i} />
                        <a
                          className="open-source"
                          href={i.url}
                          target="_blank"
                          rel="noopener noreferrer"
                          aria-label={`Open ${i.title} at source`}
                        >
                          <ExternalLink size={16} />
                        </a>
                      </div>
                    ))
                  )}
                </div>
              </section>
              {demo && (
                <p className="demo-note">
                  Illustrative work only. Internal links use example domains;
                  public links open project homes. Start a separate data
                  directory for real work.
                </p>
              )}
            </>
          )}
          {page === "mine" && (
            <>
              <MineList
                title="Waiting on you"
                note="From the account behind each connection. Internal work shows only on this machine."
                rows={mineView.mine}
                empty="Nothing waits on your connected accounts. Sources sync on schedule, or sync now to check again."
              />
              {(mineView.watching.length > 0 ||
                mineView.watch_errors.length > 0) && (
                <>
                  {mineView.watch_errors.map((e) => (
                    <div
                      className="message error"
                      role="alert"
                      key={e.connection_id}
                    >
                      <AlertCircle size={17} />
                      <span>
                        {e.error}. Watching shows what an earlier sync found.
                      </span>
                    </div>
                  ))}
                  <MineList
                    title="Watching"
                    note="Found by watching other users' activity that your tokens can see. This is their work, not yours."
                    rows={mineView.watching}
                    empty="Nothing found for the users you watch."
                  />
                </>
              )}
            </>
          )}
          {page === "bot" && botsError && (
            <div className="message error" role="alert">
              <AlertCircle size={17} />
              <span>
                The bot's view is unavailable: {botsError}. This is what it
                had before.
              </span>
            </div>
          )}
          {page === "bot" && (
            <div className="connection-grid bot-grid">
              {bots.map((b) => (
                <BotCard bot={b} now={now} key={b.connection_id} />
              ))}
            </div>
          )}
          {page === "efforts" && (
            <div className="effort-grid">
              {data.efforts.length === 0 && (
                <div className="empty">
                  <Layers size={28} />
                  <h3>Connect the pieces of your work</h3>
                  <p>
                    Select inbox items and create an effort, or create one now
                    and attach items later.
                  </p>
                  <button
                    className="primary"
                    onClick={() => openModal("effort")}
                  >
                    New effort
                  </button>
                </div>
              )}
              {data.efforts.map((e) => {
                const items = data.items.filter((i) => i.effort_id === e.id);
                const p = e.progress;
                return (
                  <article className="effort-card" key={e.id}>
                    <div className="effort-top">
                      <span className="effort-label">
                        <Layers size={14} />
                        {e.from_hub ? "EFFORT · FROM THE HUB" : "EFFORT"}
                      </span>
                      <span>
                        {p.delivered}/{p.total} delivered
                      </span>
                    </div>
                    <h2>{e.title}</h2>
                    {e.note && <p>{e.note}</p>}
                    {e.from_hub ? (
                      <p>
                        The hub's effort, over its external work only.
                        Read-only here.
                      </p>
                    ) : hub ? (
                      <p>
                        Companions see this title with the effort's work: they
                        are trusted more than the hub, which holds external
                        work only. The note stays here.
                      </p>
                    ) : (
                      <label
                        className="zone-choice subtle"
                        title="Nothing reads this machine's export today: a companion pulls from its hub and sends it nothing. This only shapes what workboard export --external prints. A hub's effort titles always reach its companions."
                      >
                        <input
                          type="checkbox"
                          checked={e.share_title}
                          onChange={(ev) => shareTitle(e, ev.target.checked)}
                        />
                        <span>
                          Share the title in this machine's own external
                          export, which nothing reads today. The note never
                          leaves.
                        </span>
                      </label>
                    )}
                    <div className="progress-track">
                      <div
                        style={{
                          width: `${p.total ? (p.delivered / p.total) * 100 : 0}%`,
                        }}
                      />
                    </div>
                    <div className="effort-items">
                      {items.map((i) => (
                        <button key={i.id} onClick={() => setDetail(i.id)}>
                          <Mark provider={i.provider} small />
                          <span>{i.title}</span>
                          <Status item={i} />
                        </button>
                      ))}
                      {items.length === 0 && <p>No linked items yet.</p>}
                    </div>
                    <div className="effort-footer">
                      <span
                        className={p.needs_attention ? "attention-text" : ""}
                      >
                        {progressText(p)}
                      </span>
                      {!e.from_hub && (
                        <select
                          aria-label={`Add an item to ${e.title}`}
                          value=""
                          onChange={(ev) => link(ev.target.value, e.id)}
                        >
                          <option value="">Add item…</option>
                          {data.items
                            .filter((i) => !i.effort_id && !i.from_hub)
                            .map((i) => (
                              <option key={i.id} value={i.id}>
                                {i.title}
                              </option>
                            ))}
                        </select>
                      )}
                    </div>
                  </article>
                );
              })}
            </div>
          )}
          {page === "connections" && (
            <>
              <div className="connection-intro">
                <Plug size={20} />
                <p>
                  Connect a personal account to sync Jira, GitLab or GitHub.
                  Other sources can be tracked with manual links while their
                  connectors are added.
                </p>
                <button className="quiet" onClick={sync} disabled={syncing}>
                  <RefreshCw size={15} className={syncing ? "spin" : ""} />
                  {syncing ? "Syncing…" : "Sync all"}
                </button>
              </div>
              {data.hub && (
                <div className="connection-intro">
                  <Plug size={20} />
                  <p className={hubAttention(data.hub) ? "danger" : undefined}>
                    <strong>Hub</strong> {data.hub.url} ·{" "}
                    {data.hub.items} items of {data.hub.connections}{" "}
                    connections, read-only here. {hubStatusText(data.hub)}
                  </p>
                </div>
              )}
              <div className="connection-grid">
                {names.map((p) => {
                  const connections = data.connections.filter(
                    (c) => c.provider === p,
                  );
                  return (
                    <article className="connection-card" key={p}>
                      <div className="connection-top">
                        <Mark provider={p} />
                        <h2>{providers[p].name}</h2>
                        <span
                          className={`connection-kind ${providers[p].live ? "live" : ""}`}
                        >
                          {providers[p].live
                            ? "Live connector"
                            : "Manual links"}
                        </span>
                      </div>
                      {connections.length ? (
                        connections.map((c) => (
                          <div className="connected-account" key={c.id}>
                            <strong>
                              {c.account_name}
                              <em className={`zone-tag ${c.zone}`}>
                                {c.zone === "external" ? "External" : "Internal"}
                              </em>
                              {c.from_hub && (
                                <em
                                  className="zone-tag hub"
                                  title="The hub's connection, read-only here"
                                >
                                  Hub
                                </em>
                              )}
                              {c.watch_user && (
                                <em
                                  className="zone-tag watching"
                                  title={`Also watching ${c.watch_user}'s activity visible to this token, listed as theirs, not yours`}
                                >
                                  Watching {c.watch_user}
                                </em>
                              )}
                            </strong>
                            <span>{c.base_url}</span>
                            <p
                              className={
                                syncStatus(c).attention ? "danger" : undefined
                              }
                            >
                              {syncStatus(c).text}
                            </p>
                            {!c.from_hub && c.schedule.needs_user && (
                              <button
                                className="quiet"
                                onClick={() => verify(c)}
                                title={`Check the stored credential again. To give it a new one, run workboard verify ${c.id} --token-stdin`}
                              >
                                <UserCheck size={14} />
                                Verify again
                              </button>
                            )}
                            {!c.from_hub && (
                              <button
                                className="quiet danger"
                                onClick={() => disconnect(c)}
                              >
                                <Unplug size={14} />
                                Disconnect
                              </button>
                            )}
                          </div>
                        ))
                      ) : (
                        <p className="connection-description">
                          {p === "jira"
                            ? "Jira Data Center with a personal access token."
                            : p.includes("gitlab")
                              ? "Connect the instance URL and a token with API read access."
                              : p === "github"
                                ? "github.com or GitHub Enterprise Server, with a token. Workboard only reads: it refuses any request that could change GitHub."
                              : p === "bot"
                                ? "A harness bot's work: its board, its fork pull requests and its heartbeat, read with a GitHub token. Workboard only reads."
                              : p === "fedora"
                                ? "forge.fedoraproject.org, with a Forgejo access token (read:user, read:issue and read:repository are enough). Workboard only reads: it refuses any request that could change it. Package dist-git on src.fedoraproject.org takes source links for now."
                              : p === "errata"
                                ? "Track advisory links now. Live sync awaits your Errata API details."
                                : "Keep source links in your inbox. Live sync is not implemented yet."}
                        </p>
                      )}
                      {(providers[p].live || !hub) && (
                        <button
                          className="quiet connection-button"
                          onClick={() =>
                            openModal(
                              providers[p].live ? "connect" : "manual",
                              p,
                            )
                          }
                        >
                          <Plus size={15} />
                          {providers[p].live
                            ? "Connect account"
                            : "Add source link"}
                        </button>
                      )}
                    </article>
                  );
                })}
              </div>
              <p className="demo-note">
                {hub
                  ? "This hub keeps external work only, and refuses anything else. Internal work stays on the companion. Credentials remain on the machine running the hub."
                  : "This prototype runs as one local user. Credentials remain on the machine running the service. Hosted team login is not implemented."}
              </p>
            </>
          )}
        </div>
        <footer className="main-footer">
          <span>
            <LayoutGrid size={13} />
            Workboard · Early prototype
          </span>
          <span>Jira → changes → release</span>
        </footer>
      </main>
      {detailItem && (
        <aside className="detail-panel">
          <div className="detail-top">
            <span>WORK ITEM</span>
            <button
              className="icon-button"
              aria-label="Close item details"
              onClick={() => setDetail(null)}
            >
              <X size={19} />
            </button>
          </div>
          <Mark provider={detailItem.provider} />
          <h2>{detailItem.title}</h2>
          <p className="muted">{detailItem.key}</p>
          <Status item={detailItem} />
          <a
            className="primary source-button"
            href={detailItem.url}
            target="_blank"
            rel="noopener noreferrer"
          >
            <ExternalLink size={16} />
            Open in {providers[detailItem.provider].name}
          </a>
          <dl>
            <dt>Source</dt>
            <dd>{providers[detailItem.provider].name}</dd>
            <dt>Outcome</dt>
            <dd>
              {outcomes[detailItem.outcome]}
              <span className="outcome-reason">
                {detailItem.outcome_reason}
              </span>
            </dd>
            <dt>Attention</dt>
            <dd>
              {needsDecision(detailItem)
                ? "Needs a decision"
                : settled(detailItem)
                  ? "None"
                  : detailItem.action || "None"}
            </dd>
            <dt>Tracking</dt>
            <dd>
              {detailItem.demo
                ? "Illustrative demo item"
                : detailItem.from_hub
                  ? "Synced by the hub; read-only here"
                  : detailItem.connection_id
                    ? "Synced from source"
                    : "Manual source link"}
            </dd>
            <dt>Zone</dt>
            <dd>
              {detailItem.from_hub
                ? "External, from the hub"
                : detailItem.zone === "external"
                  ? "External: may leave this machine in its external export"
                  : "Internal: shown only on this machine"}
            </dd>
            <dt>Last source update</dt>
            <dd>
              {detailItem.updated_at
                ? new Date(detailItem.updated_at).toLocaleString()
                : "Unknown"}
            </dd>
            {detailItem.bot?.priority && (
              <>
                <dt>Priority</dt>
                <dd>{detailItem.bot.priority}</dd>
              </>
            )}
            {detailItem.bot?.why && (
              <>
                <dt>Why</dt>
                <dd>{detailItem.bot.why}</dd>
              </>
            )}
            {detailItem.bot?.news && (
              <>
                <dt>News</dt>
                <dd>{detailItem.bot.news}</dd>
              </>
            )}
            {webLink(detailItem.bot?.branch) && (
              <>
                <dt>Branch</dt>
                <dd>
                  <a
                    href={webLink(detailItem.bot?.branch)}
                    target="_blank"
                    rel="noopener noreferrer"
                  >
                    {detailItem.bot?.branch}
                  </a>
                </dd>
              </>
            )}
            {webLink(detailItem.bot?.board_item) && (
              <>
                <dt>On the bot's board as</dt>
                <dd>
                  <a
                    href={webLink(detailItem.bot?.board_item)}
                    target="_blank"
                    rel="noopener noreferrer"
                  >
                    {detailItem.bot?.board_item}
                  </a>
                </dd>
              </>
            )}
          </dl>
          {bots
            .filter((b) => b.asks[detailItem.id])
            .map((b) => (
              <AskLinks
                ask={b.asks[detailItem.id]}
                about={true}
                key={b.connection_id}
              />
            ))}
          {detailItem.from_hub ? (
            <p className="detail-note">
              {detailItem.effort_id
                ? `In the hub's effort ${data.efforts.find((e) => e.id === detailItem.effort_id)?.title ?? ""}. `
                : ""}
              Group it, or mark it abandoned, on the hub.
            </p>
          ) : (
            <label className="form-field">
              Effort
              <select
                value={detailItem.effort_id ?? ""}
                onChange={(e) => link(detailItem.id, e.target.value)}
              >
                <option value="">Ungrouped</option>
                {data.efforts
                  .filter((e) => !e.from_hub)
                  .map((e) => (
                    <option key={e.id} value={e.id}>
                      {e.title}
                    </option>
                  ))}
              </select>
            </label>
          )}
          {detailItem.connection_id &&
            !detailItem.from_hub &&
            (needsDecision(detailItem) || detailItem.abandoned_at) && (
            <div className="manual-edit">
              <p className="detail-note">
                {detailItem.abandoned_at
                  ? "You marked this abandoned. It no longer counts toward its effort."
                  : "If this will not deliver, mark it abandoned. Link any rework to the effort."}
              </p>
              <button
                className="quiet"
                onClick={() => abandon(detailItem, !detailItem.abandoned_at)}
                disabled={busy}
              >
                {detailItem.abandoned_at ? "Undo abandoned" : "Mark abandoned"}
              </button>
            </div>
          )}
          {!detailItem.connection_id && (
            <div className="manual-edit">
              <label className="form-field">
                Manual status
                <input
                  aria-label="Manual status"
                  value={manualStatus}
                  maxLength={100}
                  onChange={(e) => setManualStatus(e.target.value)}
                />
              </label>
              <label className="form-field">
                Attention
                <select
                  aria-label="Manual attention"
                  value={manualAction}
                  onChange={(e) => setManualAction(e.target.value)}
                >
                  <option value="next">Next action</option>
                  <option value="review">Review</option>
                  <option value="waiting">Waiting</option>
                  <option value="follow-up">Follow up</option>
                </select>
              </label>
              <label className="form-field">
                Outcome
                <select
                  aria-label="Manual outcome"
                  value={manualOutcome}
                  onChange={(e) => setManualOutcome(e.target.value as Outcome)}
                >
                  {outcomeNames.map((o) => (
                    <option key={o} value={o}>
                      {outcomes[o]}
                    </option>
                  ))}
                </select>
              </label>
              <button
                className="quiet"
                onClick={updateManual}
                disabled={busy || !manualStatus.trim()}
              >
                Save manual status
              </button>
            </div>
          )}
          <p className="detail-note">
            Grouping changes your workspace. The original ticket or change stays
            in its source system.
          </p>
        </aside>
      )}
      {modal && (
        <div
          className="modal-backdrop"
          onClick={(e) => {
            if (e.target === e.currentTarget && !busy) {
              setModal(null);
              setForm({});
            }
          }}
        >
          <section
            className="modal"
            role="dialog"
            aria-modal="true"
            aria-labelledby="dialog-title"
          >
            <div className="modal-heading">
              <div className="modal-icon">
                {modal === "effort" ? (
                  <Layers size={22} />
                ) : modal === "connect" ? (
                  <Plug size={22} />
                ) : (
                  <Ticket size={22} />
                )}
              </div>
              <button
                className="icon-button"
                aria-label="Close dialog"
                disabled={busy}
                onClick={() => {
                  setModal(null);
                  setForm({});
                }}
              >
                <X size={20} />
              </button>
            </div>
            <h2 id="dialog-title">
              {modal === "effort"
                ? "Create an effort"
                : modal === "connect"
                  ? "Connect a source"
                  : "Add a source link"}
            </h2>
            <p>
              {modal === "effort"
                ? "Give related work a shared outcome."
                : modal === "connect"
                  ? "We verify your account before saving the connection."
                  : "Track work from any of your six sources."}
            </p>
            <form onSubmit={submit}>
              {modal === "effort" && (
                <>
                  {field(
                    "title",
                    "Effort title",
                    "text",
                    true,
                    "Ship the bootc storage fix",
                  )}
                  {field(
                    "note",
                    "Notes",
                    "text",
                    false,
                    "What needs to happen before this is delivered?",
                  )}
                  <div className="selected-note">
                    {selected.length
                      ? `${selected.length} selected inbox items will be linked.`
                      : "You can attach items from the effort view after creating it."}
                  </div>
                </>
              )}
              {modal === "connect" && (
                <>
                  <label className="form-field">
                    Service
                    <select
                      value={connectProvider}
                      onChange={(e) => {
                        setConnectProvider(e.target.value as Provider);
                        // A new source starts internal again.
                        setForm((f) => ({ ...f, zone: "internal" }));
                      }}
                    >
                      {names
                        .filter((p) => providers[p].live)
                        .map((p) => (
                          <option key={p} value={p}>
                            {providers[p].name}
                            {p === "jira" ? " · Data Center" : ""}
                          </option>
                        ))}
                    </select>
                  </label>
                  {field(
                    "label",
                    "Connection name",
                    "text",
                    false,
                    providers[connectProvider].name,
                  )}
                  {field(
                    "url",
                    "Instance URL",
                    "url",
                    true,
                    connectProvider === "jira"
                      ? "https://jira.example.com/"
                      : connectProvider === "github" ||
                          connectProvider === "bot"
                        ? "https://github.com/"
                        : connectProvider === "fedora"
                          ? "https://forge.fedoraproject.org/"
                          : "https://gitlab.example.com/",
                  )}
                  {field("token", "Personal access token", "password", true)}
                  {connectProvider === "bot" && (
                    <>
                      {field(
                        "board",
                        "The bot's board",
                        "text",
                        true,
                        "orgs/OWNER/NUMBER",
                      )}
                      {field(
                        "tracker",
                        "Its tracker repository",
                        "text",
                        true,
                        "OWNER/REPO",
                      )}
                      {field(
                        "heartbeat",
                        "Its heartbeat issue number",
                        "number",
                        true,
                        "1",
                      )}
                      {field("bot_login", "The bot's login", "text", true)}
                      {field(
                        "forge_org",
                        "The organization of its forks",
                        "text",
                        true,
                      )}
                      <p className="token-note">
                        Reads the board's items, the bot's open fork pull
                        requests and its heartbeat, and nothing else: no
                        request that could change GitHub is sent.
                      </p>
                    </>
                  )}
                  {connectProvider === "github" && (
                    <>
                      {field(
                        "watch",
                        "Also watch a GitHub user (optional)",
                        "text",
                        false,
                        "login",
                      )}
                      <p className="token-note">
                        Their review requests, assignments and pull requests
                        that this token can see, private ones included, are
                        listed under Watching, as theirs. They are never
                        shown as yours.
                      </p>
                    </>
                  )}
                  <label className="zone-choice">
                    <input
                      type="checkbox"
                      checked={form.zone === "external"}
                      required={hub}
                      onChange={(e) =>
                        setForm((f) => ({
                          ...f,
                          zone: e.target.checked ? "external" : "internal",
                        }))
                      }
                    />
                    <span>
                      {hub
                        ? "External, required on a hub: its work is shown wherever the hub is reached. This hub connects external sources only; connect internal work such as Jira, internal GitLab or Errata on the companion."
                        : "External: its work may leave this machine in its external export, and only external sources can be connected on a hub. Leave this off for internal work such as Jira, internal GitLab or Errata. It can't be changed later."}
                    </span>
                  </label>
                  <p className="token-note">
                    Stored in a private file on the service machine. Never
                    returned to the browser. Use a dedicated tracking token
                    where possible.
                  </p>
                </>
              )}
              {modal === "manual" && (
                <>
                  <label className="form-field">
                    Source
                    <select
                      value={form.provider}
                      onChange={(e) =>
                        setForm((f) => ({ ...f, provider: e.target.value }))
                      }
                    >
                      {names.map((p) => (
                        <option key={p} value={p}>
                          {providers[p].name}
                        </option>
                      ))}
                    </select>
                  </label>
                  {field("title", "Title")}
                  {field(
                    "key",
                    "Ticket, change, or advisory reference",
                    "text",
                    false,
                  )}
                  {field("url", "Source URL", "url")}
                  {field("status", "Current status")}
                  <label className="form-field">
                    Attention
                    <select
                      value={form.action}
                      onChange={(e) =>
                        setForm((f) => ({ ...f, action: e.target.value }))
                      }
                    >
                      <option value="next">Next action</option>
                      <option value="review">Review</option>
                      <option value="waiting">Waiting on others</option>
                      <option value="follow-up">Follow up</option>
                    </select>
                  </label>
                  <label className="form-field">
                    Outcome
                    <select
                      value={form.outcome}
                      onChange={(e) =>
                        setForm((f) => ({ ...f, outcome: e.target.value }))
                      }
                    >
                      {outcomeNames.map((o) => (
                        <option key={o} value={o}>
                          {outcomes[o]}
                        </option>
                      ))}
                    </select>
                  </label>
                </>
              )}
              {error && (
                <div className="message error" role="alert">
                  <AlertCircle size={16} />
                  <span>{error}</span>
                </div>
              )}
              <div className="modal-actions">
                <button
                  type="button"
                  className="quiet"
                  disabled={busy}
                  onClick={() => {
                    setModal(null);
                    setForm({});
                  }}
                >
                  Cancel
                </button>
                <button className="primary" disabled={busy}>
                  {busy
                    ? "Saving…"
                    : modal === "connect"
                      ? "Verify and connect"
                      : modal === "effort"
                        ? "Create effort"
                        : "Add link"}
                </button>
              </div>
            </form>
          </section>
        </div>
      )}
    </div>
  );
}
createRoot(document.getElementById("root")!).render(<App />);

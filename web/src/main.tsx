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
} from "lucide-react";
import "./style.css";

type Provider =
  "jira" | "gitlab-internal" | "gitlab-public" | "github" | "fedora" | "errata";
// Internal work is shown only on the machine that syncs it; external work
// may also be shown on the hub.
type Zone = "internal" | "external";
type Outcome =
  | "open"
  | "delivered"
  | "closed_without_delivery"
  | "abandoned"
  | "unknown";
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
};
type Connection = {
  id: string;
  provider: Provider;
  label: string;
  base_url: string;
  account_id: string;
  account_name: string;
  last_sync: string | null;
  error: string | null;
  zone: Zone;
};
type Progress = {
  delivered: number;
  total: number;
  open: number;
  needs_attention: number;
  abandoned: number;
};
type Effort = { id: string; title: string; note: string; progress: Progress };
type Snapshot = { items: Item[]; connections: Connection[]; efforts: Effort[] };
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
  github: { name: "GitHub", short: "GH", color: "#c6cdd9", live: false },
  fedora: { name: "Fedora Forge", short: "FE", color: "#79bbea", live: false },
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
// Finished: delivered, or abandoned by a person.
const settled = (i: Item) =>
  i.outcome === "delivered" || i.outcome === "abandoned";
// Never progress: a person decides what happened.
const needsDecision = (i: Item) =>
  i.outcome === "closed_without_delivery" || i.outcome === "unknown";
const outcomeClass = (i: Item) =>
  i.outcome === "delivered"
    ? "complete"
    : needsDecision(i)
      ? "attention"
      : i.outcome === "abandoned"
        ? "abandoned"
        : "";
function OutcomeIcon({ item, size }: { item: Item; size: number }) {
  if (item.outcome === "delivered") return <Check size={size} />;
  if (needsDecision(item)) return <AlertCircle size={size} />;
  if (item.outcome === "abandoned") return <X size={size} />;
  return <Circle size={size - 4} fill="currentColor" />;
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
async function api<T>(
  path: string,
  body?: unknown,
  method?: string,
): Promise<T> {
  const response = await fetch(`/api/${path}`, {
    method: method ?? (body === undefined ? "GET" : "POST"),
    credentials: "same-origin",
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
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
function Status({ item }: { item: Item }) {
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
function App() {
  const [data, setData] = useState<Snapshot>({
    items: [],
    connections: [],
    efforts: [],
  });
  const [page, setPage] = useState<"inbox" | "efforts" | "connections">(
    "inbox",
  );
  const [loading, setLoading] = useState(true);
  const [paired, setPaired] = useState(false);
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
  async function reload() {
    const next = await api<Snapshot>("snapshot");
    setData(next);
    setPaired(true);
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
        await reload();
      } catch (e) {
        setError((e as Error).message);
      } finally {
        setLoading(false);
      }
    })();
    const timer = window.setInterval(() => {
      reload().catch(() => {
        /* Keep the last view on transient failures. */
      });
    }, 30000);
    return () => window.clearInterval(timer);
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
            : "Connect Jira or GitLab to start syncing.",
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
          zone: form.zone === "external" ? "external" : "internal",
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
          ? "Account verified. Sync to fetch your work."
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
  async function disconnect(c: Connection) {
    if (
      !window.confirm(
        `Disconnect ${c.label}? This removes its credential and cached items from this app.`,
      )
    )
      return;
    try {
      await api(`connections/${c.id}`, undefined, "DELETE");
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
        `items/${detailItem.id}`,
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
      await api(`items/${item.id}/abandon`, { abandoned });
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
            <span>Personal · Local</span>
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
                : page === "efforts"
                  ? "Efforts"
                  : "Connections"}
            </strong>
          </span>
          <div className="topbar-right">
            {demo && <span className="demo-tag">DEMO DATA</span>}
            <span className="local-tag">
              <span />
              Local instance
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
                  : page === "efforts"
                    ? "From upstream to shipped."
                    : "Connect your work."}
              </h1>
              <p>
                {page === "inbox"
                  ? "One inbox for reviews, next steps, and work waiting on others."
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
              ) : (
                <>
                  <button className="quiet" onClick={() => openModal("manual")}>
                    <Plus size={16} />
                    Add source link
                  </button>
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
                          : "Connect Jira or GitLab, then sync to bring in your work."}
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
                        EFFORT
                      </span>
                      <span>
                        {p.delivered}/{p.total} delivered
                      </span>
                    </div>
                    <h2>{e.title}</h2>
                    {e.note && <p>{e.note}</p>}
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
                      <select
                        aria-label={`Add an item to ${e.title}`}
                        value=""
                        onChange={(ev) => link(ev.target.value, e.id)}
                      >
                        <option value="">Add item…</option>
                        {data.items
                          .filter((i) => !i.effort_id)
                          .map((i) => (
                            <option key={i.id} value={i.id}>
                              {i.title}
                            </option>
                          ))}
                      </select>
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
                  Connect a personal account to sync Jira or GitLab. Other
                  sources can be tracked with manual links while their
                  connectors are added.
                </p>
                <button className="quiet" onClick={sync} disabled={syncing}>
                  <RefreshCw size={15} className={syncing ? "spin" : ""} />
                  {syncing ? "Syncing…" : "Sync all"}
                </button>
              </div>
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
                            </strong>
                            <span>{c.base_url}</span>
                            <p>
                              {c.error ??
                                (c.last_sync
                                  ? `Last sync: ${new Date(c.last_sync).toLocaleString()}`
                                  : "Verified. Ready for the first sync.")}
                            </p>
                            <button
                              className="quiet danger"
                              onClick={() => disconnect(c)}
                            >
                              <Unplug size={14} />
                              Disconnect
                            </button>
                          </div>
                        ))
                      ) : (
                        <p className="connection-description">
                          {p === "jira"
                            ? "Jira Data Center with a personal access token."
                            : p.includes("gitlab")
                              ? "Connect the instance URL and a token with API read access."
                              : p === "errata"
                                ? "Track advisory links now. Live sync awaits your Errata API details."
                                : "Keep source links in your inbox. Live sync is not implemented yet."}
                        </p>
                      )}
                      <button
                        className="quiet connection-button"
                        onClick={() =>
                          openModal(providers[p].live ? "connect" : "manual", p)
                        }
                      >
                        <Plus size={15} />
                        {providers[p].live
                          ? "Connect account"
                          : "Add source link"}
                      </button>
                    </article>
                  );
                })}
              </div>
              <p className="demo-note">
                This prototype runs as one local user. Credentials remain on the
                machine running the service. Hosted team login is not
                implemented.
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
                : detailItem.connection_id
                  ? "Synced from source"
                  : "Manual source link"}
            </dd>
            <dt>Zone</dt>
            <dd>
              {detailItem.zone === "external"
                ? "External: may also be shown on the hub"
                : "Internal: shown only on this machine"}
            </dd>
            <dt>Last source update</dt>
            <dd>
              {detailItem.updated_at
                ? new Date(detailItem.updated_at).toLocaleString()
                : "Unknown"}
            </dd>
          </dl>
          <label className="form-field">
            Effort
            <select
              value={detailItem.effort_id ?? ""}
              onChange={(e) => link(detailItem.id, e.target.value)}
            >
              <option value="">Ungrouped</option>
              {data.efforts.map((e) => (
                <option key={e.id} value={e.id}>
                  {e.title}
                </option>
              ))}
            </select>
          </label>
          {detailItem.connection_id &&
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
                      : "https://gitlab.example.com/",
                  )}
                  {field("token", "Personal access token", "password", true)}
                  <label className="zone-choice">
                    <input
                      type="checkbox"
                      checked={form.zone === "external"}
                      onChange={(e) =>
                        setForm((f) => ({
                          ...f,
                          zone: e.target.checked ? "external" : "internal",
                        }))
                      }
                    />
                    <span>
                      External: its work may also be shown away from this
                      machine, on the hub. Leave this off for internal work
                      such as Jira, internal GitLab or Errata. It can't be
                      changed later.
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

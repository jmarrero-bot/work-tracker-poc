mod connector;
mod connectors;
mod db;
mod mine;
mod model;
mod outcome;
mod server;
mod zone;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use model::*;
use std::{
    io::{self, Read},
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    version,
    about = "Your work across Jira, GitLab, GitHub and release systems"
)]
struct Args {
    /// Private local data directory. Use a separate directory for demo data.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Port of the local Workboard service.
    #[arg(long, global = true, default_value_t = 8421)]
    port: u16,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Start the local API and embedded dashboard, bound only to loopback.
    Serve {
        #[arg(long)]
        demo: bool,
    },
    /// Connect Jira Data Center, a GitLab instance, GitHub, or a source
    /// served by a connector plugin, using a personal token. GitHub is only
    /// ever read: requests that could change it are refused.
    Connect {
        #[arg(value_parser=["jira","gitlab-internal","gitlab-public","github","fedora","errata"])]
        provider: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, conflicts_with = "token_stdin")]
        token_env: Option<String>,
        #[arg(long)]
        token_stdin: bool,
        /// Put this connection in the external zone, so its items may also
        /// be shown away from this machine. Connections are internal unless
        /// marked external here; this can't be changed later.
        #[arg(long)]
        external: bool,
        /// For GitHub: also watch this user's activity, as far as this
        /// connection's token can see it. What it finds
        /// is listed as theirs under Watching, never as yours.
        #[arg(long, value_name = "LOGIN")]
        watch_user: Option<String>,
    },
    /// List connection identities and sync health.
    Connections,
    /// Watch another user's activity visible to a GitHub connection's token, or
    /// stop. What it finds is listed as theirs under Watching, never as
    /// yours, and replaces what an earlier watch found.
    Watch {
        connection: String,
        /// The GitHub login to watch.
        #[arg(required_unless_present = "stop")]
        login: Option<String>,
        /// Stop watching.
        #[arg(long, conflicts_with = "login")]
        stop: bool,
    },
    /// Fetch all connected sources. No writes to upstream services.
    Sync,
    /// Show active work. Add --json for agent/script consumption.
    Inbox {
        #[arg(long)]
        json: bool,
    },
    /// Show what waits on the account behind each connection, and why: review
    /// requests, assignments, mentions and your own changes that need
    /// action. What watching another user found is listed apart, as theirs.
    Mine {
        #[arg(long)]
        json: bool,
    },
    /// Show items that need action now, including any closed without delivery
    /// or unknown.
    Next {
        #[arg(long)]
        json: bool,
    },
    /// Show efforts, their delivery progress and their source items.
    Status {
        query: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Create an effort, optionally attaching items by local ID.
    Effort {
        title: String,
        #[arg(long, default_value = "")]
        note: String,
        #[arg(long)]
        item: Vec<String>,
        /// Let the title leave this machine with the effort's external
        /// items. Otherwise the hub shows a neutral label.
        #[arg(long)]
        share_title: bool,
    },
    /// Let an effort's title leave this machine with its external items, for
    /// the hub. Titles are not shared unless you do this, since they can
    /// describe internal work; the note never leaves.
    ShareTitle {
        effort: String,
        /// Stop sharing it.
        #[arg(long)]
        undo: bool,
    },
    /// Link an item to an effort. Omit --effort to ungroup it.
    Link {
        item: String,
        #[arg(long)]
        effort: Option<String>,
    },
    /// Add a source link manually, including Errata, GitHub or Fedora Forge.
    Add {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        url: String,
        #[arg(long, default_value = "Open")]
        status: String,
        #[arg(long, default_value = "next")]
        action: String,
        /// What the work came to. The status text never implies one.
        #[arg(long, value_enum, default_value_t = Outcome::Open)]
        outcome: Outcome,
    },
    /// Update a manually tracked item's status; synced sources remain read-only.
    Update {
        item: String,
        #[arg(long)]
        status: String,
        #[arg(long, default_value = "next")]
        action: String,
        /// What the work came to. Unchanged when omitted.
        #[arg(long, value_enum)]
        outcome: Option<Outcome>,
    },
    /// Mark a synced item that was closed without delivery, or is unknown,
    /// abandoned: a decision kept in Workboard. Manual items take
    /// `update --outcome abandoned` instead.
    Abandon {
        item: String,
        /// Withdraw the decision and show the source's outcome again.
        #[arg(long)]
        undo: bool,
        /// Abandon an open or delivered item too. It is no longer refreshed,
        /// so a later merge is missed.
        #[arg(long, conflicts_with = "undo")]
        force: bool,
    },
    /// Print what may leave this machine as JSON: the external zone's
    /// connections, items and efforts, and nothing of the internal zone.
    Export {
        /// The external zone, the only one that can be exported.
        #[arg(long)]
        external: bool,
    },
    /// Print a source URL, or open it with the system browser.
    Open {
        item: String,
        #[arg(long)]
        print: bool,
    },
    /// Remove a connection, its locally cached items and its stored credential.
    Disconnect { connection: String },
}
fn default_root() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(v).join("workboard"));
    }
    let home = std::env::var_os("HOME").context("Set --data-dir when HOME is unavailable")?;
    Ok(PathBuf::from(home).join(".local/share/workboard"))
}
/// Call the local Workboard service, which the CLI drives with any method.
/// Sources are only read, by the service.
#[allow(clippy::disallowed_methods)]
async fn request<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    root: &std::path::Path,
    port: u16,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<T> {
    let token = std::fs::read_to_string(root.join("service.token"))
        .context("Start `workboard serve` with the same --data-dir first")?;
    let mut req = client
        .request(method, format!("http://127.0.0.1:{port}/api/{path}"))
        .bearer_auth(token.trim());
    if let Some(body) = body {
        req = req.json(&body)
    }
    let res = req
        .send()
        .await
        .context("Cannot reach local Workboard; start the service first")?;
    let status = res.status();
    let v: serde_json::Value = res.json().await?;
    if !status.is_success() {
        bail!(
            "{}",
            v["error"].as_str().unwrap_or("Workboard request failed")
        )
    }
    Ok(serde_json::from_value(v)?)
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let root = match args.data_dir.clone() {
        Some(path) => path,
        None => default_root()?,
    };
    if let Command::Serve { demo } = args.command {
        return server::serve(root, args.port, demo).await;
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(900))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let get = |path: &str| path.to_string();
    match args.command {
        Command::Connect {
            provider,
            url,
            label,
            token_env,
            token_stdin,
            external,
            watch_user,
        } => {
            let token = if let Some(name) = token_env {
                std::env::var(name).context("Token environment variable is not set")?
            } else if token_stdin {
                let mut s = String::new();
                io::stdin().take(16385).read_to_string(&mut s)?;
                s.trim().into()
            } else {
                bail!("Use --token-stdin or --token-env NAME. Tokens are never command-line arguments.")
            };
            let input = ConnectInput {
                label: label.unwrap_or_else(|| provider.clone()),
                provider,
                base_url: url,
                token,
                zone: if external {
                    Zone::External
                } else {
                    Zone::Internal
                },
                watch_user,
            };
            let c: Connection = request(
                &client,
                &root,
                args.port,
                reqwest::Method::POST,
                "connections",
                Some(serde_json::to_value(input)?),
            )
            .await?;
            println!(
                "Connected {} as {} ({}, {})",
                c.label,
                c.account_name,
                c.id,
                c.zone.label()
            );
            if let Some(login) = &c.watch_user {
                println!(
                    "Also watching {login}'s activity visible to this token; it is listed as theirs, not yours"
                );
            }
        }
        Command::Watch {
            connection,
            login,
            stop: _,
        } => {
            let c: Connection = request(
                &client,
                &root,
                args.port,
                reqwest::Method::PATCH,
                &format!("connections/{connection}"),
                Some(serde_json::json!({ "watch_user": login })),
            )
            .await?;
            match &c.watch_user {
                Some(login) => println!(
                    "{} is watching {login}'s activity visible to its token; `workboard mine` lists it as theirs, not yours",
                    c.label
                ),
                None => println!("{} is not watching anyone", c.label),
            }
        }
        Command::Sync => {
            let v: serde_json::Value = request(
                &client,
                &root,
                args.port,
                reqwest::Method::POST,
                "sync",
                Some(serde_json::json!({})),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            if v["results"]
                .as_array()
                .is_some_and(|r| r.iter().any(|x| x["error"].is_string()))
            {
                bail!("One or more sources could not sync; existing cached data was retained")
            }
            if v["results"]
                .as_array()
                .is_some_and(|r| r.iter().any(|x| x["watched"]["error"].is_string()))
            {
                bail!("Watching another user could not be updated; the accounts' own work was synced, and the last watch list kept")
            }
        }
        Command::Connections => {
            let s: SnapshotClient = request(
                &client,
                &root,
                args.port,
                reqwest::Method::GET,
                &get("snapshot"),
                None,
            )
            .await?;
            for c in s.connections {
                let watching = c
                    .watch_user
                    .as_ref()
                    .map(|login| format!("  watching {login}"))
                    .unwrap_or_default();
                println!(
                    "{}  {}  {}  {}{}  {}",
                    c.id,
                    c.label,
                    c.account_name,
                    c.zone.label(),
                    watching,
                    c.error.as_deref().unwrap_or("Ready")
                );
            }
        }
        Command::Inbox { json } | Command::Next { json } => {
            let next = matches!(args.command, Command::Next { .. });
            let s: SnapshotClient = request(
                &client,
                &root,
                args.port,
                reqwest::Method::GET,
                "snapshot",
                None,
            )
            .await?;
            let items: Vec<_> = s
                .items
                .into_iter()
                .filter(|i| {
                    !i.outcome.is_settled()
                        && (!next
                            || i.outcome.needs_attention()
                            || matches!(i.action.as_str(), "next" | "review"))
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?)
            } else {
                for i in items {
                    // Every listed item is open unless it needs attention.
                    let outcome = if i.outcome == Outcome::Open {
                        String::new()
                    } else {
                        format!("  {}: {}", i.outcome.label(), i.outcome_reason)
                    };
                    println!(
                        "{}  [{}] {}  {}{}{}",
                        i.id,
                        i.provider,
                        i.title,
                        i.status,
                        outcome,
                        if i.demo { " (demo)" } else { "" }
                    );
                }
            }
        }
        Command::Mine { json } => {
            let view: mine::Mine = request(
                &client,
                &root,
                args.port,
                reqwest::Method::GET,
                "mine",
                None,
            )
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
            } else {
                print_rows("Waiting on you", &view.mine);
                if !view.watching.is_empty() || !view.watch_errors.is_empty() {
                    println!();
                    print_rows("Watching: their work, not yours", &view.watching);
                    for e in &view.watch_errors {
                        println!("  Not updated, showing the last list: {}", e.error);
                    }
                }
            }
        }
        Command::Status { query, json } => {
            let s: SnapshotClient = request(
                &client,
                &root,
                args.port,
                reqwest::Method::GET,
                "snapshot",
                None,
            )
            .await?;
            let efforts: Vec<_> = s
                .efforts
                .iter()
                .filter(|e| {
                    query.as_ref().is_none_or(|q| {
                        e.effort.title.to_lowercase().contains(&q.to_lowercase())
                            || e.effort.id == *q
                    })
                })
                .collect();
            if json {
                let result: Vec<_> = efforts
                    .iter()
                    .map(|e| serde_json::json!({"effort":e,"items":items_of(&s.items, e)}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&result)?)
            } else {
                for e in efforts {
                    println!(
                        "{}  {}  {}",
                        e.effort.id,
                        e.effort.title,
                        e.progress.summary()
                    );
                    for i in items_of(&s.items, e) {
                        println!(
                            "  [{}] {}: {} ({}: {})",
                            i.provider,
                            i.key,
                            i.status,
                            i.outcome.label(),
                            i.outcome_reason
                        );
                    }
                }
            }
        }
        Command::Effort {
            title,
            note,
            item,
            share_title,
        } => {
            let e: Effort = request(
                &client,
                &root,
                args.port,
                reqwest::Method::POST,
                "efforts",
                Some(serde_json::to_value(EffortInput {
                    title,
                    note,
                    item_ids: item,
                    share_title,
                })?),
            )
            .await?;
            println!("{}  {}", e.id, e.title)
        }
        Command::ShareTitle { effort, undo } => {
            let e: Effort = request(
                &client,
                &root,
                args.port,
                reqwest::Method::PATCH,
                &format!("efforts/{effort}"),
                Some(serde_json::json!({"share_title": !undo})),
            )
            .await?;
            let state = if e.share_title {
                "shared"
            } else {
                "not shared"
            };
            println!("{}  {}: title {state}", e.id, e.title)
        }
        Command::Link { item, effort } => {
            let _: serde_json::Value = request(
                &client,
                &root,
                args.port,
                reqwest::Method::POST,
                "links",
                Some(serde_json::json!({"item":item,"effort":effort})),
            )
            .await?;
            println!("Updated grouping")
        }
        Command::Add {
            provider,
            key,
            title,
            url,
            status,
            action,
            outcome,
        } => {
            let i: Item = request(
                &client,
                &root,
                args.port,
                reqwest::Method::POST,
                "items",
                Some(serde_json::to_value(ManualInput {
                    provider,
                    key,
                    title,
                    url,
                    status,
                    action,
                    outcome,
                })?),
            )
            .await?;
            println!("{}  {}", i.id, i.title)
        }
        Command::Update {
            item,
            status,
            action,
            outcome,
        } => {
            let i: Item = request(
                &client,
                &root,
                args.port,
                reqwest::Method::PATCH,
                &format!("items/{item}"),
                Some(serde_json::json!({"status":status,"action":action,"outcome":outcome})),
            )
            .await?;
            println!("{}  {}", i.id, i.status)
        }
        Command::Abandon { item, undo, force } => {
            let i: Item = request(
                &client,
                &root,
                args.port,
                reqwest::Method::POST,
                &format!("items/{item}/abandon"),
                Some(serde_json::json!({"abandoned": !undo, "force": force})),
            )
            .await?;
            println!("{}  {}: {}", i.id, i.outcome.label(), i.outcome_reason)
        }
        Command::Export { external } => {
            if !external {
                bail!("Only the external zone can be exported: use `workboard export --external`. Internal work never leaves this machine.")
            }
            let v: serde_json::Value = request(
                &client,
                &root,
                args.port,
                reqwest::Method::GET,
                "export/external",
                None,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Command::Open { item, print } => {
            let s: SnapshotClient = request(
                &client,
                &root,
                args.port,
                reqwest::Method::GET,
                "snapshot",
                None,
            )
            .await?;
            let i = s
                .items
                .iter()
                .find(|i| i.id == item)
                .context("Item not found; use inbox --json to find its ID")?;
            if print {
                println!("{}", i.url)
            } else {
                #[cfg(target_os = "macos")]
                let program = "open";
                #[cfg(target_os = "windows")]
                let program = "explorer";
                #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
                let program = "xdg-open";
                std::process::Command::new(program)
                    .arg(&i.url)
                    .spawn()
                    .context("Cannot launch the browser; use --print")?;
            }
        }
        Command::Disconnect { connection } => {
            let _: serde_json::Value = request(
                &client,
                &root,
                args.port,
                reqwest::Method::DELETE,
                &format!("connections/{connection}"),
                None,
            )
            .await?;
            println!("Disconnected")
        }
        Command::Serve { .. } => unreachable!(),
    }
    Ok(())
}
/// Rows of the Mine view under `heading`: why each waits and on whom, its
/// native status and outcome, its zone and its link.
fn print_rows(heading: &str, rows: &[mine::Row]) {
    println!("{heading} ({})", rows.len());
    for r in rows {
        // Each account with its own reasons, never another's.
        let waits_on: Vec<_> = r
            .waits_on
            .iter()
            .map(|w| {
                let reasons: Vec<_> = w.reasons.iter().map(|r| r.label()).collect();
                format!("{}: {}", w.account, reasons.join(", "))
            })
            .collect();
        println!("  [{}] {}  {}", r.provider, r.key, r.title);
        println!(
            "      {}  ·  {}  ·  {}  ·  {}  ·  {}",
            waits_on.join("; "),
            r.status,
            r.outcome.label(),
            r.zone.label(),
            r.url
        );
    }
}
fn items_of<'a>(items: &'a [Item], e: &EffortView) -> Vec<&'a Item> {
    items
        .iter()
        .filter(|i| i.effort_id.as_deref() == Some(&e.effort.id))
        .collect()
}
#[derive(serde::Deserialize)]
struct SnapshotClient {
    connections: Vec<Connection>,
    items: Vec<Item>,
    efforts: Vec<EffortView>,
}

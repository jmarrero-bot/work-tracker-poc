mod connectors;
mod db;
mod model;
mod outcome;
mod server;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use model::*;
use std::{
    io::{self, Read},
    path::PathBuf,
};

#[derive(Parser)]
#[command(version, about = "Your work across Jira, GitLab and release systems")]
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
    /// Connect Jira Data Center or a GitLab instance using a personal token.
    Connect {
        #[arg(value_parser=["jira","gitlab-internal","gitlab-public"])]
        provider: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, conflicts_with = "token_stdin")]
        token_env: Option<String>,
        #[arg(long)]
        token_stdin: bool,
    },
    /// List connection identities and sync health.
    Connections,
    /// Fetch all connected sources. No writes to upstream services.
    Sync,
    /// Show active work. Add --json for agent/script consumption.
    Inbox {
        #[arg(long)]
        json: bool,
    },
    /// Show items that need action now.
    Next {
        #[arg(long)]
        json: bool,
    },
    /// Show efforts and remaining source items.
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
            println!("Connected {} as {} ({})", c.label, c.account_name, c.id);
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
                println!(
                    "{}  {}  {}  {}",
                    c.id,
                    c.label,
                    c.account_name,
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
                        && (!next || matches!(i.action.as_str(), "next" | "review"))
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?)
            } else {
                for i in items {
                    println!(
                        "{}  [{}] {}  {}{}",
                        i.id,
                        i.provider,
                        i.title,
                        i.status,
                        if i.demo { " (demo)" } else { "" }
                    );
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
                        e.title.to_lowercase().contains(&q.to_lowercase()) || e.id == *q
                    })
                })
                .collect();
            let result:Vec<_>=efforts.iter().map(|e|serde_json::json!({"effort":e,"items":s.items.iter().filter(|i|i.effort_id.as_deref()==Some(&e.id)).collect::<Vec<_>>()})).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?)
            } else {
                for e in efforts {
                    let items: Vec<_> = s
                        .items
                        .iter()
                        .filter(|i| i.effort_id.as_deref() == Some(&e.id))
                        .collect();
                    println!(
                        "{}  {}  {}/{} complete",
                        e.id,
                        e.title,
                        items
                            .iter()
                            .filter(|i| i.outcome == Outcome::Delivered)
                            .count(),
                        items.len()
                    );
                    for i in items {
                        println!("  [{}] {}: {}", i.provider, i.key, i.status);
                    }
                }
            }
        }
        Command::Effort { title, note, item } => {
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
                })?),
            )
            .await?;
            println!("{}  {}", e.id, e.title)
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
#[derive(serde::Deserialize)]
struct SnapshotClient {
    connections: Vec<Connection>,
    items: Vec<Item>,
    efforts: Vec<Effort>,
}

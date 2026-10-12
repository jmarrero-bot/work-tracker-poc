mod bot;
mod clock;
mod connector;
mod connectors;
mod db;
mod hub;
mod login;
mod mine;
mod model;
mod oauth;
mod outcome;
mod schedule;
mod secrets;
mod server;
mod zone;
use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use clock::{format_interval, relative};
use model::*;
use std::{
    io::{self, Read},
    path::PathBuf,
    time::{Duration, SystemTime},
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
enum CredentialsCommand {
    /// Copy every credential into the store given, read each copy back,
    /// make it the store, then drop the old copies. Run it with the service
    /// stopped. Cut short, running it again, to either store, finishes it.
    Migrate {
        #[arg(long, value_enum)]
        to: secrets::Backend,
    },
}
#[derive(Subcommand)]
enum Command {
    /// Start the local API and embedded dashboard, bound only to loopback.
    Serve {
        #[arg(long)]
        demo: bool,
        /// Serve a hub: it keeps external work only, refusing internal
        /// connections, manual links and anything else not shown to be
        /// external. A data directory remembers whether it is a hub's, and
        /// is never started as the other.
        #[arg(long, conflicts_with = "demo")]
        hub: bool,
        /// Another name a hub is reached by, such as its tailnet name
        /// behind `tailscale serve`, which serves it over HTTPS. The
        /// service still listens on loopback only. Repeat for each name.
        #[arg(long = "allowed-host", value_name = "NAME", requires = "hub")]
        allowed_hosts: Vec<String>,
        /// Let browsers pair with the hub through the pairing URL it prints,
        /// until its GitHub App is set up. Off once the App is configured.
        #[arg(long, requires = "hub", conflicts_with = "github_app_client_id")]
        pairing: bool,
        /// The client ID of the GitHub App people sign in to the hub with
        /// (see docs/github-app.md). Its client secret is set with
        /// `workboard github-app set-secret`.
        #[arg(long, value_name = "ID", requires_all = ["hub", "github_app_callback", "github_user"])]
        github_app_client_id: Option<String>,
        /// The App's callback URL: https://NAME/auth/github/callback, NAME
        /// being one of the --allowed-host names.
        #[arg(long, value_name = "URL", requires = "github_app_client_id")]
        github_app_callback: Option<String>,
        /// A GitHub user allowed to sign in, by numeric user ID, never by
        /// login. Repeat for each.
        #[arg(long, value_name = "ID", requires = "github_app_client_id")]
        github_user: Vec<u64>,
        /// How often each connection syncs unless it has its own interval,
        /// such as 30m or 1h.
        #[arg(long, value_name = "INTERVAL", default_value = "5m", value_parser = interval)]
        sync_interval: Duration,
    },
    /// Move the credentials between private files and the desktop session's
    /// Secret Service (GNOME Keyring), with the service stopped.
    Credentials {
        #[command(subcommand)]
        command: CredentialsCommand,
    },
    /// Connect Jira Data Center, a GitLab instance, GitHub, Fedora Forge, a
    /// harness bot's work on GitHub, or a source served by a connector
    /// plugin, using a personal token. GitHub, Fedora Forge and the bot are
    /// only ever read: requests that could change them are refused.
    Connect {
        #[arg(value_parser=["jira","gitlab-internal","gitlab-public","github","fedora","bot","errata"])]
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
        /// How often this connection syncs, such as 15m or 1h, if not as
        /// often as the service's default.
        #[arg(long, value_name = "INTERVAL", value_parser = interval)]
        sync_interval: Option<Duration>,
        #[command(flatten)]
        bot: BotArgs,
    },
    /// List connection identities and sync health: when each last synced,
    /// when it syncs next, and whether it needs you.
    Connections,
    /// Check a connection's credential again, or give it a new one for the
    /// same account, and sync it soon. A connection whose credential was
    /// rejected stops syncing on schedule until this succeeds.
    Verify {
        connection: String,
        /// Replace the credential with the one in this environment variable.
        #[arg(long, conflicts_with = "token_stdin")]
        token_env: Option<String>,
        /// Replace the credential with one read from standard input.
        #[arg(long)]
        token_stdin: bool,
    },
    /// Change how often a connection syncs on schedule.
    Schedule {
        connection: String,
        /// Such as 15m, 1h or 1d.
        #[arg(required_unless_present = "default", value_parser = interval)]
        interval: Option<Duration>,
        /// Sync as often as the service's default.
        #[arg(long, conflicts_with = "interval")]
        default: bool,
    },
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
    /// Fetch all connected sources now, or one. Sources also sync on their
    /// own schedule; a sync already running is joined, not started again.
    /// No writes to upstream services.
    Sync {
        /// Only this connection.
        connection: Option<String>,
    },
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
    /// Show each connected bot: its coordinator's heartbeat and the agent
    /// runs it lists, and how many items its board and fork pull requests
    /// have.
    Bot {
        #[arg(long)]
        json: bool,
    },
    /// Ask a connected bot for something, in your browser: a new issue in
    /// its tracker, prefilled and labeled for triage, or with --comment
    /// the item's own issue or pull request to comment on. You submit it
    /// yourself, as yourself; Workboard only builds the link and sends
    /// nothing.
    Ask {
        /// About this item, by its ID (see `inbox --json`), rather than
        /// anything.
        item: Option<String>,
        /// Comment on the item's issue or pull request instead.
        #[arg(long, requires = "item")]
        comment: bool,
        /// Which bot connection, when several are connected.
        #[arg(long, value_name = "CONNECTION")]
        connection: Option<String>,
        /// Print the link instead of opening it.
        #[arg(long)]
        print: bool,
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
        /// items, in the external export. Otherwise the export shows a
        /// neutral label.
        #[arg(long)]
        share_title: bool,
    },
    /// Let an effort's title leave this machine with its external items, in
    /// the external export, as a hub serves it to its companions. Titles are
    /// not shared unless you do this, since they can describe internal
    /// work; the note never leaves.
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
    /// Pull the external work of a hub, to show it with this machine's own,
    /// as its companion. Nothing of this machine's is ever sent to the hub.
    Hub {
        #[command(subcommand)]
        command: HubCommand,
    },
    /// Print a hub's companion token, with which a companion reads the
    /// hub's external export and nothing else. Run it on the hub.
    CompanionToken {
        /// Replace it; the old one stops working at once, and so does every
        /// companion credential `hub login` gave.
        #[arg(long)]
        rotate: bool,
    },
    /// Set up the GitHub App a hub signs people in with.
    GithubApp {
        #[command(subcommand)]
        command: GithubAppCommand,
    },
}
#[derive(Subcommand)]
enum GithubAppCommand {
    /// Keep the App's client secret in the data directory, in a file only
    /// you can read. The hub reads it when it starts.
    SetSecret {
        /// Read the secret from this environment variable.
        #[arg(long, conflicts_with = "token_stdin")]
        token_env: Option<String>,
        /// Read the secret from standard input.
        #[arg(long)]
        token_stdin: bool,
    },
}
/// Where a bot's work is, for `connect bot`. All are needed; none is
/// assumed.
#[derive(clap::Args)]
struct BotArgs {
    /// For the bot: the Projects board it works from, as orgs/OWNER/NUMBER
    /// or users/OWNER/NUMBER, or the board's address.
    #[arg(long, value_name = "BOARD")]
    board: Option<String>,
    /// For the bot: the repository where work is asked of it, as OWNER/REPO.
    #[arg(long, value_name = "OWNER/REPO")]
    tracker: Option<String>,
    /// For the bot: the tracker issue whose comment holds its heartbeat.
    #[arg(long, value_name = "NUMBER")]
    heartbeat: Option<u64>,
    /// For the bot: its GitHub login.
    #[arg(long, value_name = "LOGIN")]
    bot_login: Option<String>,
    /// For the bot: the organization that holds its forks.
    #[arg(long, value_name = "ORG")]
    forge_org: Option<String>,
}
impl BotArgs {
    /// The bot's settings, if any were given; the service checks them.
    fn input(self) -> Result<Option<BotInput>> {
        let given = self.board.is_some()
            || self.tracker.is_some()
            || self.heartbeat.is_some()
            || self.bot_login.is_some()
            || self.forge_org.is_some();
        if !given {
            return Ok(None);
        }
        let missing = |what: &str| anyhow!("Give the bot's {what} too");
        Ok(Some(BotInput {
            board: self.board.ok_or_else(|| missing("--board"))?,
            tracker: self.tracker.ok_or_else(|| missing("--tracker"))?,
            heartbeat_issue: self.heartbeat.ok_or_else(|| missing("--heartbeat"))?,
            bot_login: self.bot_login.ok_or_else(|| missing("--bot-login"))?,
            forge_org: self.forge_org.ok_or_else(|| missing("--forge-org"))?,
        }))
    }
}
#[derive(Subcommand)]
enum HubCommand {
    /// Pull from the hub at URL, with its companion token: the hub's
    /// HTTPS address, such as https://hub.example.ts.net/. A first pull
    /// checks both before they are kept; then it pulls on schedule.
    Set {
        url: String,
        /// Read the companion token from this environment variable.
        #[arg(long, conflicts_with = "token_stdin")]
        token_env: Option<String>,
        /// Read the companion token from standard input.
        #[arg(long)]
        token_stdin: bool,
    },
    /// Sign in to the hub at URL with GitHub, and pull from it with the
    /// credential that gives: open the page it shows and enter the code.
    /// The hub runs GitHub's device flow; it gives this machine a
    /// credential that only reads its external export, as a GitHub user
    /// it allows.
    Login { url: String },
    /// Show the hub this machine pulls from, and how fresh its work is.
    Show,
    /// Stop pulling from the hub, and forget its token and its work.
    Unset,
}
/// A sync interval given on the command line.
fn interval(text: &str) -> Result<Duration, String> {
    let interval = clock::parse_interval(text)?;
    schedule::check_interval(interval).map_err(|e| e.to_string())?;
    Ok(interval)
}
/// A token from the environment variable `env`, or from standard input; a
/// token is never a command-line argument.
fn read_token(env: Option<String>, stdin: bool) -> Result<Option<String>> {
    if let Some(name) = env {
        return Ok(Some(
            std::env::var(name).context("Token environment variable is not set")?,
        ));
    }
    if stdin {
        let mut s = String::new();
        io::stdin().take(16385).read_to_string(&mut s)?;
        return Ok(Some(s.trim().into()));
    }
    Ok(None)
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
    if let Command::Serve {
        demo,
        hub,
        allowed_hosts,
        pairing,
        github_app_client_id,
        github_app_callback,
        github_user,
        sync_interval,
    } = args.command
    {
        let github_app = github_app_client_id.map(|client_id| server::GitHubApp {
            client_id,
            callback: github_app_callback.unwrap_or_default(),
            users: github_user,
        });
        let options = server::Options {
            port: args.port,
            demo,
            interval: sync_interval,
            hub,
            hosts: allowed_hosts,
            pairing,
            github_app,
        };
        return server::serve(root, options).await;
    }
    if let Command::GithubApp {
        command:
            GithubAppCommand::SetSecret {
                token_env,
                token_stdin,
            },
    } = args.command
    {
        let secret = read_token(token_env, token_stdin)?.ok_or_else(|| {
            anyhow!(
                "Use --token-stdin or --token-env NAME. Secrets are never command-line arguments."
            )
        })?;
        login::save_secret(&root, &secret)?;
        println!("Kept the GitHub App's client secret; the hub reads it when it starts next");
        return Ok(());
    }
    if let Command::Credentials {
        command: CredentialsCommand::Migrate { to },
    } = args.command
    {
        return migrate_credentials(&root, to);
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
            sync_interval,
            bot,
        } => {
            let token = read_token(token_env, token_stdin)?.ok_or_else(|| {
                anyhow!("Use --token-stdin or --token-env NAME. Tokens are never command-line arguments.")
            })?;
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
                sync_interval: sync_interval.map(|i| i.as_secs()),
                bot: bot.input()?,
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
        Command::Verify {
            connection,
            token_env,
            token_stdin,
        } => {
            let token = read_token(token_env, token_stdin)?;
            let c: Connection = request(
                &client,
                &root,
                args.port,
                reqwest::Method::POST,
                &format!("connections/{connection}/verify"),
                Some(serde_json::json!({ "token": token })),
            )
            .await?;
            let replaced = if token.is_some() {
                "; its new credential is saved"
            } else {
                ""
            };
            println!(
                "{} is {}'s{replaced}. It syncs again soon, and on schedule",
                c.label, c.account_name
            );
        }
        Command::Schedule {
            connection,
            interval,
            default: _,
        } => {
            let c: Connection = request(
                &client,
                &root,
                args.port,
                reqwest::Method::PATCH,
                &format!("connections/{connection}"),
                Some(serde_json::json!({ "sync_interval": interval.map(|i| i.as_secs()) })),
            )
            .await?;
            let every = format_interval(Duration::from_secs(c.schedule.interval_secs));
            let whose = if c.sync_interval.is_some() {
                "its own interval"
            } else {
                "the service's default"
            };
            println!("{} syncs every {every}, {whose}", c.label);
        }
        Command::Sync { connection } => {
            let v: serde_json::Value = request(
                &client,
                &root,
                args.port,
                reqwest::Method::POST,
                "sync",
                Some(serde_json::json!({ "connection": connection })),
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
            if v["results"]
                .as_array()
                .is_some_and(|r| r.iter().any(|x| x["agents"]["error"].is_string()))
            {
                bail!("A bot's heartbeat could not be read; its board was synced, and the last heartbeat read kept")
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
            let now = Timestamp::of(SystemTime::now());
            if let Some(status) = &s.hub {
                print_hub(status, now);
            }
            for c in s.connections {
                let watching = c
                    .watch_user
                    .as_ref()
                    .map(|login| format!("  watching {login}"))
                    .unwrap_or_default();
                let hub = if c.from_hub { "  from the hub" } else { "" };
                println!(
                    "{}  {}  {}  {}{}{hub}",
                    c.id,
                    c.label,
                    c.account_name,
                    c.zone.label(),
                    watching,
                );
                let status = if c.from_hub {
                    hub_sync_status(&c, now)
                } else {
                    sync_status(&c, now)
                };
                println!("    {status}");
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
                let now = Timestamp::of(SystemTime::now());
                for i in items {
                    // Every listed item is open unless it needs attention.
                    let outcome = if i.outcome == Outcome::Open {
                        String::new()
                    } else {
                        format!("  {}: {}", i.outcome.label(), i.outcome_reason)
                    };
                    println!(
                        "{}  [{}] {}  {}{}{}{}{}",
                        i.id,
                        i.provider,
                        i.title,
                        i.status,
                        outcome,
                        if i.demo { " (demo)" } else { "" },
                        if i.from_hub { "  (from the hub)" } else { "" },
                        staleness(i.stale, i.synced_at, now)
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
                let now = Timestamp::of(SystemTime::now());
                print_rows("Waiting on you", &view.mine, now);
                if !view.watching.is_empty() || !view.watch_errors.is_empty() {
                    println!();
                    print_rows("Watching: their work, not yours", &view.watching, now);
                    for e in &view.watch_errors {
                        println!("  Not updated, showing the last list: {}", e.error);
                    }
                }
            }
        }
        Command::Bot { json } => {
            let views: Vec<bot::BotView> =
                request(&client, &root, args.port, reqwest::Method::GET, "bot", None).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&views)?);
            } else if views.is_empty() {
                println!("No bot is connected; `workboard connect bot` connects one");
            } else {
                let now = Timestamp::of(SystemTime::now());
                for v in &views {
                    print_bot(v, now);
                }
            }
        }
        Command::Ask {
            item,
            comment,
            connection,
            print,
        } => {
            let views: Vec<bot::BotView> =
                request(&client, &root, args.port, reqwest::Method::GET, "bot", None).await?;
            let chosen: Vec<&bot::BotView> = views
                .iter()
                .filter(|v| connection.as_ref().is_none_or(|c| *c == v.connection_id))
                .filter(|v| item.as_ref().is_none_or(|i| v.asks.contains_key(i)))
                .collect();
            let view = match chosen[..] {
                [view] => view,
                [] if views.is_empty() => {
                    bail!("No bot is connected; `workboard connect bot` connects one")
                }
                [] if item.is_some() => {
                    bail!("That isn't an item of a connected bot; `workboard inbox --json` lists their IDs")
                }
                [] => bail!("No bot connection has that ID; `workboard bot` lists them"),
                _ => bail!("Several bots are connected; choose one with --connection"),
            };
            let ask = match &item {
                Some(i) => &view.asks[i],
                None => &view.ask,
            };
            let (url, starts) = if comment {
                let url = ask.comment_url.as_ref().context(
                    "That item isn't an issue or pull request on the bot's GitHub; ask about it in a new issue instead",
                )?;
                (url, ask.comment.as_deref())
            } else {
                (&ask.issue_url, None)
            };
            if print {
                println!("{url}");
            } else {
                open_in_browser(url)?;
                println!("Opened in your browser; nothing is sent until you submit it there");
            }
            if let Some(text) = starts {
                println!("Start the comment with: {text}");
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
                        "{}  {}  {}{}",
                        e.effort.id,
                        e.effort.title,
                        e.progress.summary(),
                        if e.from_hub { "  (from the hub)" } else { "" },
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
                open_in_browser(&i.url)?;
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
        Command::Hub { command } => match command {
            HubCommand::Set {
                url,
                token_env,
                token_stdin,
            } => {
                let token = read_token(token_env, token_stdin)?.ok_or_else(|| {
                    anyhow!("Use --token-stdin or --token-env NAME. Tokens are never command-line arguments.")
                })?;
                let status: hub::Status = request(
                    &client,
                    &root,
                    args.port,
                    reqwest::Method::PUT,
                    "hub",
                    Some(serde_json::json!({ "url": url, "token": token })),
                )
                .await?;
                println!(
                    "Pulling from {}: {} items of {} connections, and on schedule",
                    status.url, status.items, status.connections
                );
            }
            HubCommand::Login { url } => hub_login(&client, &root, args.port, &url).await?,
            HubCommand::Show => {
                let status: Option<hub::Status> =
                    request(&client, &root, args.port, reqwest::Method::GET, "hub", None).await?;
                match status {
                    Some(status) => print_hub(&status, Timestamp::of(SystemTime::now())),
                    None => println!("No hub is set; `workboard hub set URL` sets one"),
                }
            }
            HubCommand::Unset => {
                let _: serde_json::Value = request(
                    &client,
                    &root,
                    args.port,
                    reqwest::Method::DELETE,
                    "hub",
                    None,
                )
                .await?;
                println!("No longer pulling from the hub; its work and token are gone from here");
            }
        },
        Command::CompanionToken { rotate } => {
            let method = if rotate {
                reqwest::Method::POST
            } else {
                reqwest::Method::GET
            };
            let v: serde_json::Value = request(
                &client,
                &root,
                args.port,
                method,
                "companion-token",
                rotate.then(|| serde_json::json!({})),
            )
            .await?;
            let token = v["token"].as_str().context("The hub sent no token")?;
            println!("{token}");
        }
        Command::Serve { .. } | Command::GithubApp { .. } | Command::Credentials { .. } => {
            unreachable!()
        }
    }
    Ok(())
}
/// Sign in to the hub at `url` through the local service: show the code,
/// then poll until the person entered it on GitHub, or it expired.
async fn hub_login(
    client: &reqwest::Client,
    root: &std::path::Path,
    port: u16,
    url: &str,
) -> Result<()> {
    let start: serde_json::Value = request(
        client,
        root,
        port,
        reqwest::Method::POST,
        "hub/login",
        Some(serde_json::json!({ "url": url })),
    )
    .await?;
    let text = |key: &str| start[key].as_str().unwrap_or_default().to_string();
    println!(
        "Open {} and enter the code {}",
        text("verification_uri"),
        text("user_code")
    );
    println!("Waiting for GitHub...");
    let mut interval = start["interval"].as_u64().unwrap_or(5);
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let poll: serde_json::Value = request(
            client,
            root,
            port,
            reqwest::Method::POST,
            "hub/login/poll",
            Some(serde_json::json!({})),
        )
        .await?;
        if poll["status"] == "done" {
            let status: hub::Status = serde_json::from_value(poll["hub"].clone())?;
            println!(
                "Signed in to {} as {}. Pulling from it: {} items of {} connections, and on schedule",
                status.url,
                poll["login"].as_str().unwrap_or_default(),
                status.items,
                status.connections
            );
            return Ok(());
        }
        if let Some(error) = poll["error"].as_str() {
            println!("Still waiting; the last poll failed: {error}");
        }
        interval = poll["interval"].as_u64().unwrap_or(interval);
    }
}
/// `workboard credentials migrate`: offline, under the data directory's
/// lock, in a local service's data directory only.
fn migrate_credentials(root: &std::path::Path, to: secrets::Backend) -> Result<()> {
    db::private_dir(root)?;
    let _lock = db::lock(root)?;
    // A hub keeps files; opening the database as a local one refuses a hub's.
    db::Db::open_as(root, db::Mode::Local)
        .context("Credentials move only in a local service's data directory")?;
    let migrated = secrets::migrate(root, to)?;
    let store = match to {
        secrets::Backend::Files => "private files",
        secrets::Backend::Keyring => "GNOME Keyring",
    };
    println!(
        "The credentials are kept in {store}: {} copied, {} old copies dropped",
        migrated.copied, migrated.dropped
    );
    Ok(())
}
/// Open `url` with the system browser.
fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(url)
        .spawn()
        .context("Cannot launch the browser; use --print")?;
    Ok(())
}
/// A bot connection: its board's counts, its coordinator's heartbeat and
/// the agent runs it lists, and how fresh that is.
fn print_bot(v: &bot::BotView, now: Timestamp) {
    println!(
        "{}  {}  {}  {}",
        v.connection_id, v.label, v.bot_login, v.board_url
    );
    let Some(agents) = &v.agents else {
        println!("    Not read yet");
        return;
    };
    match &agents.board {
        Some(board) => {
            let items: usize = board.statuses.values().sum();
            let statuses: Vec<_> = board
                .statuses
                .iter()
                .map(|(status, n)| format!("{n} {status}"))
                .collect();
            println!(
                "    Board: {items} items ({}); {} open fork pull requests",
                statuses.join(", "),
                board.fork_prs
            );
            // The board shown is never taken for all of it.
            if board.unreadable > 0 {
                println!(
                    "    And {} items this token can't read: the board above isn't all of it",
                    board.unreadable
                );
            }
        }
        None => println!("    Board: not read yet"),
    }
    let read = staleness(agents.stale, agents.last_success, now);
    match &agents.heartbeat {
        None => println!("    No heartbeat is published{read}"),
        Some(h) => {
            let wake = h
                .next_wake_at
                .map(|t| format!(", next wake {}", relative(t, now)))
                .unwrap_or_default();
            let stale = if h.stale {
                "Stale, the coordinator may have stopped: "
            } else {
                ""
            };
            println!(
                "    {stale}Coordinator {} is {}, as of {}{wake}{read}",
                h.session,
                h.loop_state,
                relative(h.updated_at, now)
            );
            println!("    {} agent runs", h.runs.len());
            for r in &h.runs {
                let devspace = r
                    .devspace
                    .as_ref()
                    .map(|d| format!("  on {d}"))
                    .unwrap_or_default();
                println!(
                    "      {}  {}  {}  started {}{devspace}",
                    r.name,
                    r.item_key,
                    r.status,
                    relative(r.started_at, now)
                );
            }
            if h.skipped > 0 {
                println!("      {} more left out, as malformed", h.skipped);
            }
        }
    }
    if let Some(error) = &agents.error {
        println!("    Not updated, showing the last heartbeat read: {error}");
    }
}
/// The hub this machine pulls from, and how its pulls are going.
fn print_hub(status: &hub::Status, now: Timestamp) {
    println!(
        "Hub  {}  {} items of {} connections",
        status.url, status.items, status.connections
    );
    let job = Connection {
        id: "hub".into(),
        sync: status.sync.clone(),
        schedule: status.schedule.clone(),
        ..Connection::default()
    };
    let line = if status.schedule.needs_user {
        let error = status.sync.error.as_deref().unwrap_or("token refused");
        format!("Needs you: {error}. Pulls stopped until the hub takes the token: run `workboard hub set URL` with its current companion token")
    } else {
        sync_status(&job, now)
    };
    println!("    {line}");
}
/// How the hub's connection `c` was syncing on the hub, as of the last pull.
fn hub_sync_status(c: &Connection, now: Timestamp) -> String {
    let last = c.sync.last_success.map_or("never synced".into(), |t| {
        format!("last synced {}", relative(t, now))
    });
    let stale = if c.schedule.stale { "Stale: " } else { "" };
    let failed = if c.sync.error_kind.is_some() {
        "; its last sync there failed"
    } else {
        ""
    };
    format!("{stale}Synced by the hub, {last}{failed}; read-only here")
}
/// How a connection's syncs are going, as of `now`: when it last synced and
/// syncs next, how it fails, or that it needs the person.
fn sync_status(c: &Connection, now: Timestamp) -> String {
    let s = &c.sync;
    let last = s.last_success.map_or("never synced".into(), |t| {
        format!("last synced {}", relative(t, now))
    });
    let error = s.error.as_deref().unwrap_or("unknown error");
    let next = s.next_due.map_or("soon".into(), |t| {
        // The schedule starts it within moments.
        if t <= now {
            "due now".into()
        } else {
            relative(t, now)
        }
    });
    if c.schedule.needs_user {
        return format!(
            "Needs you: {error}. Scheduled syncs stopped until the credential works: fix it, then run `workboard verify {}` (add --token-stdin to give a new one). {}",
            c.id,
            capitalized(&last)
        );
    }
    let state = if c.schedule.running {
        "Syncing now".to_string()
    } else if s.error.is_none() {
        let every = format_interval(Duration::from_secs(c.schedule.interval_secs));
        format!("Next sync {next}, every {every}")
    } else if s.held_until.is_some_and(|held| held > now) {
        format!("The source asked to wait; next try {next}: {error}")
    } else {
        let syncs = if s.failures == 1 { "sync" } else { "syncs" };
        format!(
            "{} failed {syncs} in a row, backing off; next try {next}: {error}",
            s.failures
        )
    };
    // Never synced says so itself.
    let stale = if c.schedule.stale && s.last_success.is_some() {
        "Stale: "
    } else {
        ""
    };
    format!("{stale}{state}; {last}")
}
/// How stale something synced at `synced_at` is, if it is.
fn staleness(stale: bool, synced_at: Option<Timestamp>, now: Timestamp) -> String {
    if !stale {
        return String::new();
    }
    match synced_at {
        Some(t) => format!("  (stale: last synced {})", relative(t, now)),
        None => "  (stale: not synced)".into(),
    }
}
/// `text` with a capital first letter.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or(String::new(), |first| {
        first.to_uppercase().chain(chars).collect()
    })
}
/// Rows of the Mine view under `heading`: why each waits and on whom, its
/// native status and outcome, its zone, its link, and whether it is stale.
fn print_rows(heading: &str, rows: &[mine::Row], now: Timestamp) {
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
        let zone = if r.from_hub {
            "external, from the hub"
        } else {
            r.zone.label()
        };
        println!(
            "      {}  ·  {}  ·  {}  ·  {zone}  ·  {}{}",
            waits_on.join("; "),
            r.status,
            r.outcome.label(),
            r.url,
            staleness(r.stale, r.synced_at, now)
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
    #[serde(default)]
    hub: Option<hub::Status>,
}

//! CLI for the Herdr Desktop / hc browser. Every page command has one owner.

mod address;
mod agent;
mod browser_files;
mod browser_install;
mod cdp;
mod chrome;
mod debug;
mod host;
mod icons;
mod page;
mod page_tools;
mod render;
mod state;
mod view;

use agent::SendKind;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use state::Config;
use std::fmt::Write as _;
use std::path::PathBuf;

pub type R<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(version, about = "An owned live browser for Herdr Desktop and hc")]
struct Cli {
    /// Exact local hc terminal address. Defaults to HC_TERMINAL_ID.
    #[arg(long, global = true)]
    owner: Option<String>,
    /// Select a named, isolated Herdr browser profile.
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Print machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Navigate your owned page, creating it if absent.
    Open {
        url: String,
        /// Also open or reuse a live split beside the owner.
        #[arg(long)]
        show: bool,
    },
    /// Open or reuse the live split beside the owner.
    Show,
    /// List interactive elements and their current numeric references.
    Snapshot,
    /// Click a reference from snapshot, or a CSS selector.
    Click { target: String },
    /// Replace an input's text. Enter is sent only with --submit.
    Type {
        target: String,
        text: String,
        #[arg(long)]
        submit: bool,
    },
    /// Print visible page text.
    Text {
        #[arg(long)]
        max: Option<usize>,
    },
    /// Save a PNG of the page viewport.
    Screenshot { path: PathBuf },
    /// Show recorded console entries since navigation.
    Console {
        #[arg(long, default_value_t = 100)]
        max: usize,
        #[arg(long)]
        clear: bool,
    },
    /// Show recorded requests, failures first.
    Network {
        #[arg(long, default_value_t = 50)]
        max: usize,
        #[arg(long)]
        clear: bool,
    },
    /// Go back in this page's history.
    Back,
    /// Reload this page.
    Reload,
    /// Deliver a browser selection or screenshot file to this owner only.
    Send { kind: SendKind },
    /// Show whether the dedicated browser is running.
    Status,
    /// List existing named browser profiles.
    Profiles,
    /// Stop this helper's dedicated Chrome.
    Stop,
    /// Provision Chrome for Testing explicitly.
    InstallBrowser,
    /// Open a real browser window for manual sign-in to this profile.
    Login { url: Option<String> },
    #[command(hide = true)]
    View {
        /// Open in this terminal without an agent recipient.
        #[arg(long, conflicts_with = "owner")]
        standalone: bool,
        /// Navigate the standalone page once when it starts.
        #[arg(long, requires = "standalone")]
        url: Option<String>,
        /// Open a manual sign-in window before starting the standalone view.
        #[arg(long, requires = "standalone")]
        sign_in: bool,
    },
}

fn print(json_out: bool, text: impl std::fmt::Display, value: Value) {
    if json_out {
        println!("{value}");
    } else {
        println!("{text}");
    }
}

fn title_url(v: &Value) -> String {
    let url = v["url"].as_str().unwrap_or_default();
    match v["title"].as_str().filter(|s| !s.is_empty()) {
        Some(title) => format!("{title} — {url}"),
        None => url.to_owned(),
    }
}

fn run(cli: Cli) -> R<()> {
    state::select_profile(cli.profile)?;
    let j = cli.json;
    match cli.command {
        Cmd::Profiles => {
            let profiles = state::profiles()?;
            print(j, profiles.join("\n"), json!({"profiles": profiles}));
            return Ok(());
        }
        Cmd::Status => {
            if let Some(endpoint) = chrome::endpoint() {
                let mut cdp = cdp::Cdp::connect(&endpoint)?;
                let count = page::page_targets(&mut cdp)?.len();
                print(j, format_args!("running headless, {count} page(s)"), json!({"running": true, "tabs": count}));
            } else if state::login_guard().is_err() {
                print(j, "manual login window open", json!({"running": true, "login": true}));
            } else {
                print(j, "not running", json!({"running": false}));
            }
            return Ok(());
        }
        Cmd::Stop => {
            let stopped = chrome::stop()?;
            print(j, if stopped { "stopped" } else { "not running" }, json!({"stopped": stopped}));
            return Ok(());
        }
        Cmd::InstallBrowser => {
            let version = browser_install::install()?;
            print(j, format_args!("using Chrome for Testing {version}"), json!({"version": version}));
            return Ok(());
        }
        Cmd::Login { url } => {
            return chrome::login(&Config::load()?, url.as_deref());
        }
        Cmd::Show => {
            let owner = host::owner(cli.owner.as_deref())?;
            host::show(&owner).map_err(|e| format!("display failed for {owner}: {e}"))?;
            print(j, format_args!("view pane requested for {owner}; browser startup is not confirmed"), json!({"owner": owner, "displayed": true, "browser_startup_confirmed": false}));
            return Ok(());
        }
        Cmd::View { standalone, url, sign_in } => {
            let owner = if standalone {
                None
            } else {
                Some(host::owner(Some(cli.owner.as_deref().ok_or("view requires --owner or --standalone")?))?)
            };
            let config = Config::load()?;
            if sign_in {
                chrome::login(&config, Some(url.as_deref().unwrap_or(page::HOME_URL)))?;
            }
            return view::run(config, owner, url.as_deref());
        }
        command => owned(command, j, cli.owner.as_deref()),
    }
}

fn owned(command: Cmd, j: bool, explicit: Option<&str>) -> R<()> {
    let owner = host::owner(explicit)?;
    let cfg = Config::load()?;
    let page = || page::resolve(&cfg, &owner, None);
    match command {
        Cmd::Open { url, show } => {
            let mut page = page::resolve(&cfg, &owner, Some("about:blank"))?;
            let mut value = page.navigate(&url)?;
            drop(page);
            let label = title_url(&value);
            if show {
                if let Err(e) = host::show(&owner) {
                    value["displayed"] = json!(false);
                    print(j, format_args!("navigated: {label}"), value);
                    return Err(format!("navigation succeeded; display failed for {owner}: {e}").into());
                }
                value["displayed"] = json!(true);
                value["browser_startup_confirmed"] = json!(false);
                print(j, format_args!("{label} (view pane requested for {owner}; browser startup is not confirmed)"), value);
            } else {
                print(j, label, value);
            }
        }
        Cmd::Snapshot => {
            let value = page()?.snapshot()?;
            if j {
                println!("{value}");
            } else {
                let mut lines = title_url(&value);
                for item in value["items"].as_array().into_iter().flatten() {
                    write!(lines, "\n[{}] {} {:?}", item["ref"], item["role"].as_str().unwrap_or_default(), item["name"].as_str().unwrap_or_default())?;
                    if let Some(href) = item["href"].as_str() {
                        write!(lines, " -> {href}")?;
                    }
                    if let Some(val) = item["value"].as_str() {
                        write!(lines, " value={val:?}")?;
                    }
                    if item["disabled"] == true {
                        lines.push_str(" (disabled)");
                    }
                }
                println!("{lines}");
            }
        }
        Cmd::Click { target } => {
            let mut page = page()?;
            page.click(&target)?;
            let v = page.info()?;
            print(j, format_args!("clicked {target}; now {}", title_url(&v)), v);
        }
        Cmd::Type { target, text, submit } => {
            let mut page = page()?;
            page.type_text(&target, &text, submit)?;
            print(j, format_args!("typed into {target}"), page.info()?);
        }
        Cmd::Text { max } => {
            let mut text = page()?.text()?;
            let max = max.unwrap_or_else(|| cfg.max_chars());
            let total = text.chars().count();
            if let Some((end, _)) = text.char_indices().nth(max) {
                text.truncate(end);
                write!(text, "\n[truncated: {max} of {total} chars; use --max]")?;
            }
            if j {
                println!("{}", json!({"text": text, "chars": total}));
            } else {
                println!("{text}");
            }
        }
        Cmd::Screenshot { path } => {
            std::fs::write(&path, page()?.screenshot()?)?;
            let path = path.canonicalize()?;
            print(j, path.display(), json!({"path": path}));
        }
        Cmd::Console { max, clear } => {
            let (text, value) = debug::console(&mut page()?, max, clear)?;
            print(j, text, value);
        }
        Cmd::Network { max, clear } => {
            let (text, value) = debug::network(&mut page()?, max, clear)?;
            print(j, text, value);
        }
        Cmd::Back | Cmd::Reload => {
            let mut page = page()?;
            let v = if matches!(command, Cmd::Back) { page.back()? } else { page.reload()? };
            print(j, title_url(&v), v);
        }
        Cmd::Send { kind } => {
            agent::send_from_browser(kind, &owner, &cfg)?;
            print(j, format_args!("sent to {owner}"), json!({"agent": owner}));
        }
        _ => unreachable!("administrative commands handled in run"),
    }
    Ok(())
}

fn main() {
    use std::io::IsTerminal;
    let cli = Cli::parse();
    let hold_errors = matches!(&cli.command, Cmd::View { standalone: true, sign_in: true, .. })
        && std::io::stdin().is_terminal();
    if let Err(error) = run(cli) {
        eprintln!("herdr-browser: {error}");
        if hold_errors {
            eprintln!("Press Enter to close this terminal.");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn sign_in_requires_standalone_view() {
        assert!(Cli::try_parse_from(["herdr-browser", "view", "--sign-in"]).is_err());
        let cli = Cli::try_parse_from([
            "herdr-browser", "--profile", "work", "view", "--standalone",
            "--sign-in", "--url", "https://example.com",
        ]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("work"));
        assert!(matches!(cli.command, Cmd::View {
            standalone: true, sign_in: true, url: Some(url)
        } if url == "https://example.com"));
    }
}

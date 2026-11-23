//! Exact local herdr-core ownership and desktop split placement.

use crate::{R, state};
use serde_json::Value;
use std::process::Command;

#[derive(Debug)]
struct TerminalNotFound(String);

impl std::fmt::Display for TerminalNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TerminalNotFound {}

/// Invoke an external CLI without a shell, retaining its refusal message.
fn run(bin: &str, args: &[&str]) -> R<Value> {
    let execute = |program: &std::ffi::OsStr| Command::new(program).args(args).output();
    let output = match execute(std::ffi::OsStr::new(bin)) {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && matches!(bin, "hc" | "herdr-app") => {
            let home = std::env::var_os("HOME").ok_or_else(|| format!("cannot run {bin}: {e}"))?;
            let fallback = std::path::PathBuf::from(home).join(".local/bin").join(bin);
            execute(fallback.as_os_str()).map_err(|err| format!("cannot run {bin} or {}: {err}", fallback.display()))?
        }
        Err(e) => return Err(format!("cannot run {bin}: {e}").into()),
    };
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        if bin == "hc" && args.first() == Some(&"get")
            && serde_json::from_str::<Value>(message).is_ok_and(|v| v.pointer("/error/kind").and_then(Value::as_str) == Some("not_found"))
        {
            return Err(Box::new(TerminalNotFound(format!("hc get: {message}"))));
        }
        return Err(format!("{bin} {}: {message}", args.first().unwrap_or(&"")).into());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("{bin} {} returned invalid JSON: {e}", args.first().unwrap_or(&"")).into())
}

fn local_id(id: &str) -> bool {
    id.strip_prefix('t').is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()))
}

fn terminal(id: &str) -> R<Value> {
    if !local_id(id) {
        return Err(format!("owner {id:?} is not a concrete local hc terminal address").into());
    }
    let value = run("hc", &["get", id, "--json"])?;
    if value["address"].as_str() != Some(id) {
        return Err(format!("hc did not resolve {id} to that exact local terminal").into());
    }
    if value["state"] != "live" {
        return Err(format!("hc terminal {id} is not live").into());
    }
    Ok(value)
}

/// Resolve only an explicit local terminal address or HC_TERMINAL_ID.
pub fn owner(explicit: Option<&str>) -> R<String> {
    let id = match explicit {
        Some(id) => id.to_string(),
        None => std::env::var("HC_TERMINAL_ID").map_err(|_| "no owner: pass --owner <local-hc-address> or run in an hc terminal")?,
    };
    terminal(&id)?;
    Ok(id)
}

/// Require an actual live agent, not a shell or a blocked agent.
pub fn recipient(id: &str) -> R<()> {
    let value = terminal(id)?;
    let status = &value["status"];
    let app = status["app"].as_str().filter(|s| !s.trim().is_empty()).ok_or("owner is not a live agent (missing status.app)")?;
    let state = status["state"].as_str().filter(|s| !s.trim().is_empty()).ok_or("owner is not a live agent (missing status.state)")?;
    if matches!(app, "sh" | "bash" | "zsh" | "fish" | "nu" | "shell") {
        return Err(format!("owner {id} is a shell, not an agent").into());
    }
    if state == "blocked" {
        return Err(format!("agent {id} is blocked; delivery refused").into());
    }
    Ok(())
}

/// Revalidate immediately before delivery; never use force or a shell command.
pub fn send(id: &str, message: &str) -> R<()> {
    recipient(id)?;
    let result = run("hc", &["send", id, message])?;
    if result["sent"] != true {
        return Err(format!("hc send refused delivery: {result}").into());
    }
    Ok(())
}

struct Placement {
    tab: String,
    pane: String,
}

/// Only unambiguous, locally displayed terminals are eligible for a split.
fn placement(ls: &Value, terminal: &str) -> R<Option<Placement>> {
    let spaces = ls["spaces"].as_array().ok_or("herdr-app ls returned no spaces")?;
    let mut matches = Vec::new();
    for space in spaces {
        if !space["machine"].is_null() {
            continue;
        }
        for tab in space["tabs"].as_array().into_iter().flatten() {
            for item in tab["terminals"].as_array().into_iter().flatten() {
                if item["terminal"].as_str() == Some(terminal) {
                    let tab_id = tab["id"].as_str().filter(|s| !s.is_empty()).ok_or("desktop placement has no tab id")?;
                    let pane = item["pane"].as_str().filter(|s| !s.is_empty()).ok_or("desktop placement has no pane id")?;
                    matches.push(Placement { tab: tab_id.into(), pane: pane.into() });
                }
            }
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches.remove(0))),
        _ => Err(format!("terminal {terminal} has ambiguous desktop placement").into()),
    }
}

/// Open or reuse the exact recorded split, serialized per owner.
pub fn show(id: &str) -> R<()> {
    let _guard = state::lock_named(&format!("show-{id}"))?;
    terminal(id)?;
    let ls = run("herdr-app", &["ls"]).map_err(|e| format!("desktop display unavailable: {e}"))?;
    let owner_place = placement(&ls, id)?.ok_or_else(|| format!("owner {id} has no visible local desktop pane"))?;
    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    let exe = exe.to_str().ok_or("helper path is not UTF-8")?;
    let mut argv = vec![exe, "--owner", id];
    if let Some(profile) = state::selected_profile() {
        argv.extend(["--profile", profile]);
    }
    argv.push("view");

    if let Some(view) = state::load()?.views.get(id).cloned() {
        if let Some(place) = placement(&ls, &view.terminal)? {
            if place.tab == owner_place.tab && place.pane == view.pane {
                let current = match run("hc", &["get", &view.terminal, "--json"]) {
                    Ok(value) => Some(value),
                    Err(e) if e.downcast_ref::<TerminalNotFound>().is_some() => None,
                    Err(e) => return Err(e),
                };
                if let Some(current) = current {
                    if current["address"].as_str() != Some(view.terminal.as_str()) {
                        return Err("hc returned a different recorded view address".into());
                    }
                    match current["state"].as_str() {
                        Some("live") => {
                            let actual = current["argv"].as_array().ok_or("hc returned no recorded view argv")?;
                            if actual.len() == argv.len() && actual.iter().zip(&argv).all(|(v, expected)| v.as_str() == Some(*expected)) {
                                return Ok(());
                            }
                        }
                        Some("finished") => {}
                        _ => return Err("hc returned invalid recorded view state".into()),
                    }
                }
            }
        }
        state::update(|s| s.views.remove(id))?;
    }

    let encoded = serde_json::to_string(&argv)?;
    let result = run("herdr-app", &["do", "pane.split", "--direction", "right", "--pane", &owner_place.pane, "--argv", &encoded])
        .map_err(|e| format!("desktop display failed: {e}"))?;
    let view_terminal = result["terminal"].as_str().filter(|s| local_id(s)).ok_or("pane.split returned no concrete local terminal")?;
    let view_pane = if let Some(pane) = result["pane"].as_str().filter(|s| !s.is_empty()) {
        pane.to_owned()
    } else {
        let updated = run("herdr-app", &["ls"])?;
        let place = placement(&updated, view_terminal)?.ok_or("pane.split returned no pane and its terminal is not displayed")?;
        if place.tab != owner_place.tab {
            return Err("pane.split returned a terminal outside the owner's tab".into());
        }
        place.pane
    };
    state::update(|s| {
        s.views.insert(id.to_owned(), state::ViewRecord { pane: view_pane.to_owned(), terminal: view_terminal.to_owned() });
    })?;
    Ok(())
}

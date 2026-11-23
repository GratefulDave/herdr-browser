//! File transfers on a dedicated CDP connection. Download configuration is
//! shared by Chrome's profile; events are filtered to this view's page frames.
use crate::{cdp::Cdp, chrome, state, R};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const PICKER: &str = r#"
ObjC.import('AppKit');
function run(argv) {
  const app = $.NSApplication.sharedApplication;
  app.activateIgnoringOtherApps(true);
  const panel = $.NSOpenPanel.openPanel;
  const folder = argv[0] === 'folder';
  panel.canChooseFiles = !folder;
  panel.canChooseDirectories = folder;
  panel.allowsMultipleSelection = argv[0] === 'multiple';
  if (panel.runModal !== $.NSModalResponseOK) return '[]';
  const urls = panel.URLs;
  const paths = [];
  for (let i = 0; i < urls.count; i++) paths.push(ObjC.unwrap(urls.objectAtIndex(i).path));
  return JSON.stringify(paths);
}
"#;

fn picker(mode: &str) -> R<Vec<String>> {
    let output = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", PICKER, "--", mode]).output()?;
    if !output.status.success() {
        return Err(format!("native file picker failed: {}", String::from_utf8_lossy(&output.stderr).trim()).into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn frame_ids(tree: &Value, frames: &mut HashSet<String>) {
    if let Some(id) = tree.pointer("/frame/id").and_then(Value::as_str) { frames.insert(id.to_owned()); }
    if let Some(children) = tree.get("childFrames").and_then(Value::as_array) {
        for child in children { frame_ids(child, frames); }
    }
}

fn safe_label(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(160).collect()
}

struct Session {
    id: String,
    frames: HashSet<String>,
}

struct Download {
    name: String,
}

/// One instance per interactive view; no messages are read from Page's CDP.
pub struct Files {
    cdp: Cdp,
    pending: VecDeque<Value>,
    sessions: HashMap<String, Session>,
    active: Option<String>,
    downloads: HashMap<String, Download>,
    history: VecDeque<String>,
    gesture: Option<Instant>,
    endpoint: String,
}

impl Files {
    pub fn new() -> R<Self> {
        let endpoint = chrome::endpoint().ok_or("Chrome is not running")?;
        let mut files = Self {
            cdp: Cdp::connect(&endpoint)?, pending: VecDeque::new(), sessions: HashMap::new(),
            active: None, downloads: HashMap::new(), history: VecDeque::new(), gesture: None, endpoint,
        };
        files.configure_downloads(None)?;
        Ok(files)
    }

    // Unlike Cdp::call, retain asynchronous events while waiting for a reply.
    fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> R<Value> {
        let id = self.cdp.send(method, params, session)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() { return Err(format!("timed out waiting for {method}").into()); }
            let Some(message) = self.cdp.next_message(left)? else { continue };
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(format!("{method}: {}", error.get("message").and_then(Value::as_str).unwrap_or("CDP error")).into());
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if message.get("method").is_some() { self.pending.push_back(message); }
        }
    }

    fn configure_downloads(&mut self, chosen: Option<PathBuf>) -> R<PathBuf> {
        // The record is valid only for this browser endpoint, so selections do
        // not survive a Chrome restart. The lock coordinates separate views.
        let _lock = state::lock_named("downloads")?;
        let record = state::state_dir().join("downloads-session.json");
        let previous = std::fs::read(&record).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let directory = chosen.or_else(|| previous.as_ref().filter(|v| v["endpoint"] == self.endpoint)
            .and_then(|v| v["directory"].as_str()).map(PathBuf::from))
            .unwrap_or_else(|| state::profile_dir().join("downloads"));
        std::fs::create_dir_all(&directory)?;
        let directory = directory.canonicalize()?;
        self.call("Browser.setDownloadBehavior", json!({"behavior":"allow", "downloadPath": directory, "eventsEnabled":true}), None)?;
        let data = serde_json::to_vec(&json!({"endpoint":self.endpoint, "directory":directory}))?;
        let temporary = record.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, data)?;
        std::fs::rename(temporary, record)?;
        Ok(directory)
    }

    /// Select the active target and arm interception once for each visited page.
    pub fn attach(&mut self, target_id: &str) -> R<()> {
        if self.active.as_deref() != Some(target_id) { self.gesture = None; }
        if !self.sessions.contains_key(target_id) {
            let result = self.call("Target.attachToTarget", json!({"targetId":target_id,"flatten":true}), None)?;
            let session = result["sessionId"].as_str().ok_or("attachToTarget returned no session")?.to_owned();
            self.call("Page.enable", json!({}), Some(&session))?;
            let tree = self.call("Page.getFrameTree", json!({}), Some(&session))?;
            let mut frames = HashSet::new();
            frame_ids(&tree["frameTree"], &mut frames);
            self.call("Page.setInterceptFileChooserDialog", json!({"enabled":true}), Some(&session))?;
            self.sessions.insert(target_id.to_owned(), Session { id: session, frames });
        }
        self.active = Some(target_id.to_owned());
        Ok(())
    }

    /// Call immediately before forwarding an actual user click or key to the page.
    pub fn note_user_input(&mut self) { self.gesture = Some(Instant::now()); }

    /// Explicitly choose a destination for all downloads in this Chrome profile.
    pub fn choose_download_directory(&mut self) -> R<Option<PathBuf>> {
        let Some(path) = picker("folder")?.into_iter().next() else { return Ok(None) };
        self.configure_downloads(Some(PathBuf::from(path))).map(Some)
    }

    /// Recent notices for this view, oldest first.
    pub fn history(&self) -> &VecDeque<String> { &self.history }

    /// Current profile-wide destination, including changes from other views.
    pub fn download_directory(&self) -> R<PathBuf> {
        let _lock = state::lock_named("downloads")?;
        let data: Value = serde_json::from_slice(&std::fs::read(state::state_dir().join("downloads-session.json"))?)?;
        if data["endpoint"] != self.endpoint { return Err("download destination belongs to a different browser session".into()); }
        Ok(PathBuf::from(data["directory"].as_str().ok_or("download destination is missing")?))
    }

    /// Drain transfer events without reading or consuming another client's events.
    pub fn poll(&mut self) -> R<Vec<String>> {
        let mut notices = Vec::new();
        loop {
            let event = match self.pending.pop_front() {
                Some(event) => event,
                None => match self.cdp.next_message(Duration::from_millis(1))? { Some(event) => event, None => break },
            };
            let method = event["method"].as_str().unwrap_or("");
            let params = &event["params"];
            let sid = event["sessionId"].as_str().unwrap_or("");
            match method {
                "Page.frameAttached" | "Page.frameNavigated" => {
                    let frame = params.get("frameId").or_else(|| params.pointer("/frame/id")).and_then(Value::as_str);
                    if let (Some(frame), Some(session)) = (frame, self.sessions.values_mut().find(|s| s.id == sid)) {
                        session.frames.insert(frame.to_owned());
                    }
                }
                "Page.frameDetached" => {
                    if let (Some(frame), Some(session)) = (params["frameId"].as_str(), self.sessions.values_mut().find(|s| s.id == sid)) {
                        session.frames.remove(frame);
                    }
                }
                "Browser.downloadWillBegin" => {
                    let frame = params["frameId"].as_str().unwrap_or("");
                    if self.sessions.values().any(|s| s.frames.contains(frame)) {
                        if let Some(guid) = params["guid"].as_str() {
                            let name = safe_label(params["suggestedFilename"].as_str().unwrap_or("download"));
                            notices.push(format!("Downloading {name}"));
                            self.downloads.insert(guid.to_owned(), Download { name });
                        }
                    }
                }
                "Browser.downloadProgress" => {
                    if let Some(download) = params["guid"].as_str().and_then(|id| self.downloads.get(id)) {
                        let state = params["state"].as_str().unwrap_or("inProgress");
                        let received = params["receivedBytes"].as_f64().unwrap_or(0.0) as u64;
                        let total = params["totalBytes"].as_f64().unwrap_or(0.0) as u64;
                        let mut notice = format!("{}: {state} ({received}/{total} bytes)", download.name);
                        if state == "completed" {
                            if let Some(path) = params["filePath"].as_str() { notice.push_str(&format!(" — {}", safe_label(path))); }
                        }
                        notices.push(notice);
                        if state == "completed" || state == "canceled" { self.downloads.remove(params["guid"].as_str().unwrap_or("")); }
                    }
                }
                "Page.fileChooserOpened" => {
                    let active = self.active.as_ref().and_then(|id| self.sessions.get(id)).is_some_and(|s| s.id == sid);
                    let authorized = self.gesture.take().is_some_and(|t| t.elapsed() <= Duration::from_secs(2));
                    if !active || !authorized { notices.push("File chooser ignored: no explicit page input".into()); continue; }
                    let Some(node) = params["backendNodeId"].as_u64() else { notices.push("File chooser has no input node".into()); continue };
                    let desc = self.call("DOM.describeNode", json!({"backendNodeId":node}), Some(sid))?;
                    let directory = desc.pointer("/node/attributes").and_then(Value::as_array)
                        .is_some_and(|attrs| attrs.chunks(2).any(|a| a.first().and_then(Value::as_str) == Some("webkitdirectory")));
                    let mode = if directory { "folder" } else if params["mode"] == "selectMultiple" { "multiple" } else { "single" };
                    let paths = picker(mode)?;
                    if paths.is_empty() { notices.push("File selection canceled".into()); continue; }
                    self.call("DOM.setFileInputFiles", json!({"backendNodeId":node,"files":paths}), Some(sid))?;
                    notices.push("Selected file(s) supplied to page".into());
                }
                _ => {}
            }
        }
        for notice in &notices {
            if self.history.len() == 100 { self.history.pop_front(); }
            self.history.push_back(notice.clone());
        }
        Ok(notices)
    }
}

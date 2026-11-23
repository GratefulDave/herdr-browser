//! Tab resolution and page operations over CDP.

use crate::cdp::{Cdp, DEFAULT_TIMEOUT};
use crate::state::{self, Config};
use crate::{R, chrome};
use serde_json::{Value, json};
use std::thread::sleep;
use std::time::{Duration, Instant};

pub const HOME_URL: &str = "https://www.google.com";

const COMMON_JS: &str = include_str!("js/common.js");
const SNAPSHOT_JS: &str = include_str!("js/snapshot.js");

/// Wrap a script so the shared helpers are scoped to this one evaluation.
fn script(prelude: &str, body: &str) -> String {
    format!("(() => {{ {prelude}\n{COMMON_JS}\nreturn (\n{body}\n); }})()")
}

#[derive(Debug, Clone)]
pub struct Tab {
    pub id: String,
    title: String,
    opener: Option<String>,
}

pub fn page_targets(c: &mut Cdp) -> R<Vec<Tab>> {
    let mut r = c.call("Target.getTargets", json!({}), None)?;
    let Value::Array(infos) = r["targetInfos"].take() else { return Err("Target.getTargets returned no targetInfos".into()) };
    Ok(infos
        .into_iter()
        .filter(|t| t["type"] == "page" && !t["url"].as_str().unwrap_or_default().starts_with("devtools://"))
        .filter_map(|mut t| match t["targetId"].take() {
            Value::String(id) => Some(Tab {
                id,
                title: t["title"].as_str().unwrap_or_default().to_owned(),
                opener: t["openerId"].as_str().map(str::to_owned),
            }),
            _ => None,
        })
        .collect())
}

/// Public tab description, limited to the current page's own group.
#[derive(Debug, Clone)]
pub struct BrowserTab {
    pub id: String,
    pub title: String,
    pub active: bool,
}

// Caller holds the group lock. Record descendants before pruning closed
// members, so a surviving popup remains owned after its opener closes.
fn sync_group(root: &str, targets: &[Tab]) -> R<state::TabGroup> {
    if !targets.iter().any(|t| t.id == root) {
        return Err("browser root target is closed".into());
    }
    let previous = state::load()?.tab_groups.get(root).cloned();
    let mut group = previous.clone().unwrap_or_else(|| state::TabGroup {
        selected: root.to_owned(), members: vec![root.to_owned()], ..Default::default()
    });
    update_group(root, targets, &mut group);
    if previous.as_ref() != Some(&group) {
        state::update(|s| { s.tab_groups.insert(root.to_owned(), group.clone()); })?;
    }
    Ok(group)
}

fn closing_fallback<'a>(group: &'a state::TabGroup, id: &str) -> Option<&'a str> {
    group.openers.get(id).filter(|opener| *opener != id && group.members.contains(opener))
        .map(String::as_str)
}

fn update_group(root: &str, targets: &[Tab], group: &mut state::TabGroup) {
    if !group.members.iter().any(|id| id == root) {
        group.members.push(root.to_owned());
    }
    loop {
        let mut added = false;
        for tab in targets {
            if !group.members.contains(&tab.id)
                && tab.opener.as_ref().is_some_and(|id| group.members.contains(id))
            {
                if tab.opener.as_deref() == Some(group.selected.as_str()) {
                    group.selected = tab.id.clone();
                }
                group.members.push(tab.id.clone());
                added = true;
            }
        }
        if !added { break; }
    }
    for tab in targets.iter().filter(|tab| group.members.contains(&tab.id)) {
        if let Some(opener) = tab.opener.as_ref().filter(|opener| group.members.contains(opener)) {
            group.openers.insert(tab.id.clone(), opener.clone());
        }
    }
    group.members.retain(|id| targets.iter().any(|t| &t.id == id));
    if !group.members.contains(&group.selected) {
        group.selected = closing_fallback(group, &group.selected).unwrap_or(root).to_owned();
    }
    group.openers.retain(|id, _| group.members.contains(id));
}

fn switch_to(page: &mut Page, tab: Tab) -> R<bool> {
    if page.tab.id == tab.id { return Ok(false); }
    let r = page.cdp.call("Target.attachToTarget", json!({ "targetId": tab.id, "flatten": true }), None)?;
    let session = r["sessionId"].as_str().ok_or("attachToTarget returned no session")?.to_owned();
    let old_session = std::mem::replace(&mut page.session, session);
    page.tab = tab;
    page.zoom_script = None;
    let _ = page.cdp.call("Target.detachFromTarget", json!({ "sessionId": old_session }), None);
    Ok(true)
}

pub fn create_tab(c: &mut Cdp, url: &str) -> R<String> {
    // Each headless target gets its own window so it continues to paint.
    let r = c.call("Target.createTarget", json!({ "url": url, "newWindow": true }), None)?;
    r["targetId"].as_str().map(String::from).ok_or_else(|| "createTarget returned no id".into())
}


/// The tab a caller should act on; see `resolve`.
pub struct Page {
    pub cdp: Cdp,
    pub tab: Tab,
    pub session: String,
    pub(crate) zoom_script: Option<String>,
    root: String,
}

/// Resolve only the target assigned to this owner; create it at `initial_url` if provided.
pub fn resolve(cfg: &Config, owner: &str, initial_url: Option<&str>) -> R<Page> {
    // The caller has already validated owner. The same lock covers lookup and creation.
    let _guard = state::lock_named("tab-groups")?;
    let owned = state::load()?.tabs.get(owner).cloned();
    if owned.is_none() && initial_url.is_none() {
        return Err(format!("no browser page for {owner}; run `herdr-browser open <url>` first").into());
    }
    let url = if initial_url.is_some() {
        chrome::ensure(cfg)?
    } else {
        chrome::endpoint().ok_or_else(|| format!("no browser page for {owner}; run `herdr-browser open <url>` first"))?
    };
    let mut c = Cdp::connect(&url)?;
    let tabs = page_targets(&mut c)?;
    let root = match owned.filter(|id| tabs.iter().any(|tab| &tab.id == id)) {
        Some(id) => id,
        None if initial_url.is_some() => {
            let id = create_tab(&mut c, initial_url.expect("creation URL checked"))?;
            if let Err(e) = state::update(|s| s.tabs.insert(owner.to_owned(), id.clone())) {
                let _ = c.call("Target.closeTarget", json!({ "targetId": id }), None);
                return Err(e);
            }
            id
        }
        None => return Err(format!("no browser page for {owner}; run `herdr-browser open <url>` first").into()),
    };
    let targets = page_targets(&mut c)?;
    let group = sync_group(&root, &targets)?;
    let chosen = targets.into_iter().find(|tab| tab.id == group.selected).ok_or("selected tab is closed")?;
    attach_tab(c, chosen, root)
}

fn attach_tab(mut cdp: Cdp, tab: Tab, root: String) -> R<Page> {
    let r = cdp.call("Target.attachToTarget", json!({ "targetId": tab.id, "flatten": true }), None)?;
    let session = r["sessionId"].as_str().ok_or("attachToTarget returned no session")?.to_string();
    Ok(Page { cdp, tab, session, root, zoom_script: None })
}

/// Create an independent target for a standalone terminal view.
pub fn standalone(cfg: &Config, initial_url: &str) -> R<Page> {
    let mut c = Cdp::connect(&chrome::ensure(cfg)?)?;
    let id = create_tab(&mut c, &normalize_url(initial_url))?;
    let tab = page_targets(&mut c)?.into_iter().find(|tab| tab.id == id).ok_or("new browser target is closed")?;
    attach_tab(c, tab, id)
}

/// Reattach a standalone view only to the target it already created.
pub fn attach(target: &str) -> R<Page> {
    let url = chrome::endpoint().ok_or("browser is not running")?;
    let _guard = state::lock_named("tab-groups")?;
    let mut c = Cdp::connect(&url)?;
    let targets = page_targets(&mut c)?;
    let group = sync_group(target, &targets)?;
    let tab = targets.into_iter().find(|tab| tab.id == group.selected).ok_or("selected tab is closed")?;
    attach_tab(c, tab, target.to_owned())
}

/// Synchronize the selected target and discover new opener descendants.
/// The root argument is retained for existing view callers, but may not name
/// another group.
pub fn refresh_target(page: &mut Page, root: &str) -> R<bool> {
    if page.root != root { return Err("browser tab group does not match view root".into()); }
    let _guard = state::lock_named("tab-groups")?;
    let targets = page_targets(&mut page.cdp)?;
    let group = sync_group(root, &targets)?;
    let tab = targets.into_iter().find(|tab| tab.id == group.selected).ok_or("selected tab is closed")?;
    switch_to(page, tab)
}

pub fn tabs(page: &mut Page) -> R<Vec<BrowserTab>> {
    let _guard = state::lock_named("tab-groups")?;
    let targets = page_targets(&mut page.cdp)?;
    let group = sync_group(&page.root, &targets)?;
    let selected = targets.iter().find(|tab| tab.id == group.selected).ok_or("selected tab is closed")?.clone();
    switch_to(page, selected)?;
    Ok(group.members.iter().filter_map(|id| targets.iter().find(|t| &t.id == id))
        .map(|t| BrowserTab { id: t.id.clone(), title: t.title.clone(), active: t.id == group.selected })
        .collect())
}

pub fn new_tab(page: &mut Page, url: Option<&str>) -> R<()> {
    let _guard = state::lock_named("tab-groups")?;
    let targets = page_targets(&mut page.cdp)?;
    sync_group(&page.root, &targets)?;
    let url = url.map(normalize_url).unwrap_or_else(|| HOME_URL.to_owned());
    let id = create_tab(&mut page.cdp, &url)?;
    if let Err(e) = state::update(|s| {
        let group = s.tab_groups.get_mut(&page.root).expect("group just synchronized");
        group.members.push(id.clone());
        group.selected = id.clone();
    }) {
        let _ = page.cdp.call("Target.closeTarget", json!({ "targetId": id }), None);
        return Err(e);
    }
    let tab = page_targets(&mut page.cdp)?.into_iter().find(|t| t.id == id).ok_or("new browser target is closed")?;
    switch_to(page, tab)?;
    Ok(())
}

pub fn select_tab(page: &mut Page, id: &str) -> R<()> {
    let _guard = state::lock_named("tab-groups")?;
    let targets = page_targets(&mut page.cdp)?;
    let group = sync_group(&page.root, &targets)?;
    if !group.members.iter().any(|member| member == id) {
        return Err("tab does not belong to this browser view".into());
    }
    let tab = targets.into_iter().find(|tab| tab.id == id).ok_or("tab is closed")?;
    switch_to(page, tab)?;
    state::update(|s| s.tab_groups.get_mut(&page.root).expect("group just synchronized").selected = id.to_owned())?;
    Ok(())
}

/// Closing the last tab resets it to the home page. Closing the root with other tabs
/// migrates the group and every owner mapping to a surviving tab.
pub fn close_tab(page: &mut Page) -> R<()> {
    let _guard = state::lock_named("tab-groups")?;
    let targets = page_targets(&mut page.cdp)?;
    let group = sync_group(&page.root, &targets)?;
    let id = group.selected.clone();
    if group.members.len() == 1 {
        let tab = targets.into_iter().find(|tab| tab.id == id).ok_or("selected tab is closed")?;
        switch_to(page, tab)?;
        page.navigate(HOME_URL)?;
        return Ok(());
    }
    let next_id = closing_fallback(&group, &id)
        .or_else(|| group.members.iter().find(|member| **member != id).map(String::as_str))
        .ok_or("no surviving tab")?.to_owned();
    let next = targets.into_iter().find(|tab| tab.id == next_id).ok_or("surviving tab is closed")?;
    switch_to(page, next)?;
    page.cdp.call("Target.closeTarget", json!({ "targetId": id }), None)?;
    let old_root = page.root.clone();
    let new_root = if id == old_root { next_id.clone() } else { old_root.clone() };
    state::update(|s| {
        let mut group = s.tab_groups.remove(&old_root).expect("group just synchronized");
        group.members.retain(|member| member != &id);
        group.openers.remove(&id);
        group.selected = next_id;
        s.tab_groups.insert(new_root.clone(), group);
        if old_root != new_root {
            for root in s.tabs.values_mut().filter(|root| **root == old_root) {
                *root = new_root.clone();
            }
        }
    })?;
    page.root = new_root;
    Ok(())
}

/// Close only the standalone root and its opener descendants, children first.
pub fn close_target(page: &mut Page, root: &str) -> R<()> {
    if page.root != root { return Err("browser tab group does not match view root".into()); }
    let _guard = state::lock_named("tab-groups")?;
    let targets = page_targets(&mut page.cdp)?;
    let group = sync_group(root, &targets)?;
    for id in group.members.iter().rev() {
        page.cdp.call("Target.closeTarget", json!({ "targetId": id }), None)?;
    }
    state::update(|s| { s.tab_groups.remove(root); })?;
    Ok(())
}

pub fn normalize_url(url: &str) -> String {
    let has_scheme = url.split_once("://").is_some_and(|(scheme, _)| {
        let mut chars = scheme.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    }) || ["about:", "data:", "file:", "chrome:", "javascript:"].iter().any(|p| {
        url.get(..p.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(p))
    });
    if has_scheme {
        return url.to_string();
    }
    // Local dev servers rarely have TLS: localhost, loopback, LAN and *.local get http.
    let host = url.split(['/', '?', '#']).next().unwrap_or("");
    let name = host.rsplit_once(':').filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit())).map_or(host, |(h, _)| h);
    let local = name.eq_ignore_ascii_case("localhost")
        || name.rsplit_once('.').is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("localhost") || suffix.eq_ignore_ascii_case("local"))
        || name == "[::1]"
        || name.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback() || ip.is_private() || ip.is_unspecified());
    format!("{}://{url}", if local { "http" } else { "https" })
}

impl Page {
    /// Stable group key, except when closing the root migrates the group.
    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn eval(&mut self, expr: &str) -> R<Value> {
        self.cdp.eval(&self.session, expr, DEFAULT_TIMEOUT)
    }

    pub(crate) fn call(&mut self, method: &str, params: Value) -> R<Value> {
        let sid = self.session.clone();
        self.cdp.call(method, params, Some(&sid))
    }

    // A release/submit can close the target before CDP replies. Only treat that
    // as dispatched when the original target actually disappeared.
    fn input_event(&mut self, method: &str, params: Value, may_close: bool) -> R<bool> {
        match self.call(method, params) {
            Ok(_) => Ok(false),
            Err(e) => {
                let tabs = page_targets(&mut self.cdp)?;
                if !may_close || tabs.iter().any(|tab| tab.id == self.tab.id) {
                    return Err(e);
                }
                let root = self.root.clone();
                refresh_target(self, &root)?;
                Ok(true)
            }
        }
    }

    fn wait_after_input(&mut self) -> R<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let root = self.root.clone();
            refresh_target(self, &root)?;
            if self.cdp.eval(&self.session, "document.readyState", Duration::from_secs(2)).is_ok_and(|v| v == "complete") {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for page after input".into());
            }
            sleep(Duration::from_millis(100));
        }
    }

    /// Wait for `document.readyState == "complete"`, tolerating context swaps mid-navigation.
    pub fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(v) = self.cdp.eval(&self.session, "document.readyState", Duration::from_secs(2))
                && v == "complete"
            {
                return;
            }
            sleep(Duration::from_millis(100));
        }
    }

    pub fn info(&mut self) -> R<Value> {
        self.eval("({ url: location.href, title: document.title })")
    }

    pub fn navigate(&mut self, url: &str) -> R<Value> {
        let r = self.call("Page.navigate", json!({ "url": normalize_url(url) }))?;
        if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(format!("navigation failed: {e}").into());
        }
        self.wait_ready();
        self.info()
    }

    pub fn back(&mut self) -> R<Value> {
        self.eval("history.back()")?;
        sleep(Duration::from_millis(300));
        self.wait_ready();
        self.info()
    }

    pub fn forward(&mut self) -> R<Value> {
        self.eval("history.forward()")?;
        sleep(Duration::from_millis(300));
        self.wait_ready();
        self.info()
    }

    pub fn reload(&mut self) -> R<Value> {
        self.call("Page.reload", json!({}))?;
        sleep(Duration::from_millis(300));
        self.wait_ready();
        self.info()
    }

    pub fn activate(&mut self) -> R<()> {
        let id = self.tab.id.clone();
        self.cdp.call("Target.activateTarget", json!({ "targetId": id }), None)?;
        self.call("Page.bringToFront", json!({}))?;
        Ok(())
    }

    pub fn text(&mut self) -> R<String> {
        Ok(self.eval("document.body ? document.body.innerText : ''")?.as_str().unwrap_or_default().to_string())
    }

    pub fn selection(&mut self) -> R<String> {
        Ok(self.eval("String(getSelection())")?.as_str().unwrap_or_default().to_string())
    }

    pub fn snapshot(&mut self) -> R<Value> {
        self.eval(&script("", SNAPSHOT_JS))
    }

    /// Scroll the element into view and return its centre in viewport coordinates.
    fn locate(&mut self, target: &str) -> R<(f64, f64)> {
        let t = serde_json::to_string(target)?;
        let v = self.eval(&script(
            "",
            &format!(
                "(() => {{ const el = __hb.resolve({t}); el.scrollIntoView({{ block: 'center', inline: 'center' }});\
                 const r = el.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; }})()"
            ),
        ))?;
        Ok((v[0].as_f64().unwrap_or(0.0), v[1].as_f64().unwrap_or(0.0)))
    }

    /// Real (trusted) mouse click at the element's centre.
    pub fn click(&mut self, target: &str) -> R<()> {
        let (x, y) = self.locate(target)?;
        for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
            if self.input_event("Input.dispatchMouseEvent", json!({ "type": kind, "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1 }), kind == "mouseReleased")? {
                break;
            }
        }
        sleep(Duration::from_millis(200));
        self.wait_after_input()?;
        Ok(())
    }

    /// Replace the field's contents with `text` (fill semantics), optionally pressing Enter.
    pub fn type_text(&mut self, target: &str, text: &str, submit: bool) -> R<()> {
        let t = serde_json::to_string(target)?;
        self.eval(&script(
            "",
            &format!(
                "(() => {{ const el = __hb.resolve({t}); el.scrollIntoView({{ block: 'center' }}); el.focus();\
                 if ('value' in el) el.select ? el.select() : (el.value = '');\
                 else if (el.isContentEditable) document.execCommand('selectAll'); return true; }})()"
            ),
        ))?;
        self.call("Input.insertText", json!({ "text": text }))?;
        if submit {
            for kind in ["keyDown", "keyUp"] {
                if self.input_event(
                    "Input.dispatchKeyEvent",
                    json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r" }),
                    true,
                )? {
                    break;
                }
            }
            sleep(Duration::from_millis(300));
            self.wait_after_input()?;
        }
        Ok(())
    }

    pub fn screenshot(&mut self) -> R<Vec<u8>> {
        use base64::Engine;
        let r = self.call("Page.captureScreenshot", json!({ "format": "png" }))?;
        let data = r["data"].as_str().ok_or("captureScreenshot returned no data")?;
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }


}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: &str, opener: Option<&str>) -> Tab {
        Tab { id: id.into(), title: String::new(), opener: opener.map(str::to_owned) }
    }

    #[test]
    fn closed_popup_returns_to_live_non_root_opener() {
        let mut group = state::TabGroup {
            selected: "other".into(),
            members: vec!["root".into(), "other".into()],
            ..Default::default()
        };
        let open = vec![tab("root", None), tab("other", None), tab("popup", Some("other"))];
        update_group("root", &open, &mut group);
        assert_eq!(group.selected, "popup");
        assert_eq!(closing_fallback(&group, "popup"), Some("other"));

        update_group("root", &open[..2], &mut group);
        assert_eq!(group.selected, "other");
        assert!(!group.openers.contains_key("popup"));
    }

    #[test]
    fn missing_or_foreign_opener_does_not_escape_group() {
        let mut group = state::TabGroup {
            selected: "popup".into(),
            members: vec!["root".into(), "popup".into()],
            openers: [("popup".into(), "foreign".into())].into(),
        };
        let targets = vec![tab("root", None), tab("foreign", None)];
        update_group("root", &targets, &mut group);
        assert_eq!(group.selected, "root");
    }

    #[test]
    fn closed_opener_and_regular_tabs_keep_existing_fallback() {
        let mut group = state::TabGroup {
            selected: "popup".into(),
            members: vec!["root".into(), "opener".into(), "popup".into(), "regular".into()],
            openers: [("popup".into(), "opener".into())].into(),
        };
        update_group("root", &[tab("root", None), tab("regular", None)], &mut group);
        assert_eq!(group.selected, "root");
        assert_eq!(closing_fallback(&group, "regular"), None);
    }

    #[test]
    fn legacy_groups_deserialize_without_opener_metadata() {
        let group: state::TabGroup = serde_json::from_str(
            r#"{"selected":"root","members":["root"]}"#,
        ).unwrap();
        assert!(group.openers.is_empty());
    }

    #[test]
    fn bare_hosts_get_https() {
        assert_eq!(normalize_url("example.com/x"), "https://example.com/x");
    }

    #[test]
    fn local_dev_servers_get_http() {
        for u in ["localhost:3000", "127.0.0.1:8080/app", "192.168.1.20:5173", "myapp.local", "[::1]:4000"] {
            assert_eq!(normalize_url(u), format!("http://{u}"));
        }
        assert_eq!(normalize_url("example.com:8443/x"), "https://example.com:8443/x");
    }

    #[test]
    fn bare_urls_preserve_encoded_path_and_nested_redirect() {
        assert_eq!(
            normalize_url("localhost:3000/a%2Fb?next=https://example.com/x%3Fy#part"),
            "http://localhost:3000/a%2Fb?next=https://example.com/x%3Fy#part",
        );
        assert_eq!(
            normalize_url("example.com/a%2Fb?next=https://other.example/x#part"),
            "https://example.com/a%2Fb?next=https://other.example/x#part",
        );
        assert_eq!(normalize_url("[::1]/x?next=https://example.com"), "http://[::1]/x?next=https://example.com");
        assert_eq!(normalize_url("LOCALHOST:3000/x"), "http://LOCALHOST:3000/x");
    }

    #[test]
    fn explicit_schemes_are_kept() {
        for u in ["http://a.b", "about:blank", "data:text/html,hi", "file:///tmp/x", "chrome:settings", "JAVASCRIPT:alert(1)"] {
            assert_eq!(normalize_url(u), u);
        }
    }
}

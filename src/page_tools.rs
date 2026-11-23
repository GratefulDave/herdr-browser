//! Page-local zoom and find tools for an attached CDP target.

use crate::{R, page::Page};
use serde_json::json;

const ZOOM_STEPS: &[u16] = &[25, 33, 50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200, 250, 300, 400, 500];
const FIND_JS: &str = include_str!("js/find.js");

/// Move to the next supported zoom level, clamping at either end.
pub fn step_zoom(current: u16, direction: i8) -> u16 {
    if direction > 0 {
        ZOOM_STEPS.iter().copied().find(|&n| n > current).unwrap_or(*ZOOM_STEPS.last().unwrap())
    } else if direction < 0 {
        ZOOM_STEPS.iter().rev().copied().find(|&n| n < current).unwrap_or(ZOOM_STEPS[0])
    } else {
        100
    }
}

/// Apply layout zoom now and to subsequent documents in this CDP session.
/// Call again when a view attaches to a different target/session. 100 restores
/// the document root's original inline zoom rather than forcing `zoom: 1`.
pub fn apply_zoom(page: &mut Page, percent: u16) -> R<()> {
    if !(25..=500).contains(&percent) {
        return Err("zoom must be between 25% and 500%".into());
    }
    // The callback also handles document-start, before documentElement exists.
    let source = format!(r#"(() => {{
        if (window !== window.top) return;
        const desired = Symbol.for('herdr-browser.desired-root-zoom');
        window[desired] = {percent};
        const apply = () => {{
            const root = document.documentElement;
            if (!root) return;
            const key = Symbol.for('herdr-browser.original-root-zoom');
            const percent = window[desired];
            if (percent === 100) {{
                if (root[key]) {{
                    const [value, priority] = root[key];
                    if (value) root.style.setProperty('zoom', value, priority);
                    else root.style.removeProperty('zoom');
                    delete root[key];
                }}
            }} else {{
                if (!root[key]) root[key] = [root.style.getPropertyValue('zoom'), root.style.getPropertyPriority('zoom')];
                root.style.setProperty('zoom', `${{percent}}%`, 'important');
            }}
        }};
        if (document.documentElement) apply();
        else document.addEventListener('DOMContentLoaded', apply, {{ once: true }});
    }})()"#);
    let session = &page.session;
    let result = page.cdp.call("Page.addScriptToEvaluateOnNewDocument", json!({"source": source}), Some(session))?;
    let id = result["identifier"].as_str().ok_or("CDP did not return a zoom script identifier")?.to_owned();
    if let Some(old) = &page.zoom_script {
        if let Err(err) = page.cdp.call("Page.removeScriptToEvaluateOnNewDocument", json!({"identifier": old}), Some(session)) {
            let _ = page.cdp.call("Page.removeScriptToEvaluateOnNewDocument", json!({"identifier": id}), Some(session));
            return Err(err);
        }
    }
    page.zoom_script = Some(id);
    page.eval(&source)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindResult {
    /// One-based active match index, or zero if there are no matches.
    pub current: usize,
    pub total: usize,
}

/// Search rendered top-document text. Matches do not span text nodes; frames,
/// form-control values and closed shadow roots are not searched.
/// A zero step starts a search; positive/negative steps wrap through matches.
pub fn find(page: &mut Page, query: &str, step: i8) -> R<FindResult> {
    let query = serde_json::to_string(query)?;
    let expression = format!("({FIND_JS})({query}, {})", step.signum());
    let result = page.eval(&expression)?;
    let current = result["current"].as_u64().ok_or("find returned no current match")?;
    let total = result["total"].as_u64().ok_or("find returned no match count")?;
    Ok(FindResult { current: current as usize, total: total as usize })
}

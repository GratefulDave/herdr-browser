//! Profile-local address history, bookmarks and search preferences.

use crate::{state, R};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DEFAULT_SEARCH: &str = "https://duckduckgo.com/?q={query}";
const HISTORY_LIMIT: usize = 500;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub url: String,
    pub title: String,
    pub bookmarked: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    url: String,
    title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Data {
    history: Vec<Entry>,
    bookmarks: Vec<Entry>,
    search_template: String,
}

impl Default for Data {
    fn default() -> Self {
        Self { history: Vec::new(), bookmarks: Vec::new(), search_template: DEFAULT_SEARCH.into() }
    }
}

pub struct AddressBook {
    dir: PathBuf,
    data: Data,
}

impl AddressBook {
    pub fn load() -> R<Self> {
        Self::load_in(state::state_dir())
    }

    fn load_in(dir: PathBuf) -> R<Self> {
        let _lock = lock(&dir)?;
        let data = read(&dir)?;
        Ok(Self { dir, data })
    }

    // Re-read while locked so a view never overwrites another process's updates.
    fn update<T>(&mut self, change: impl FnOnce(&mut Data) -> T) -> R<T> {
        let _lock = lock(&self.dir)?;
        let mut data = read(&self.dir)?;
        let before = data.clone();
        let result = change(&mut data);
        if data != before {
            write(&self.dir, &data)?;
        }
        self.data = data;
        Ok(result)
    }

    pub fn record(&mut self, url: &str, title: &str) -> R<()> {
        if !safe_url(url) { return Ok(()); }
        self.update(|data| {
            let entry = Entry { url: url.into(), title: title.into() };
            if data.history.first() != Some(&entry) {
                data.history.retain(|e| e.url != url);
                data.history.insert(0, entry);
                data.history.truncate(HISTORY_LIMIT);
            }
        })
    }

    pub fn toggle_bookmark(&mut self, url: &str, title: &str) -> R<Option<bool>> {
        if !safe_url(url) { return Ok(None); }
        self.update(|data| {
            if let Some(index) = data.bookmarks.iter().position(|e| e.url == url) {
                data.bookmarks.remove(index);
                Some(false)
            } else {
                data.bookmarks.insert(0, Entry { url: url.into(), title: title.into() });
                Some(true)
            }
        })
    }

    pub fn suggest(&self, input: &str) -> Vec<Suggestion> {
        let query = input.trim().to_lowercase();
        let mut suggestions = Vec::new();
        for entry in self.data.bookmarks.iter().chain(self.data.history.iter()) {
            if suggestions.iter().any(|s: &Suggestion| s.url == entry.url) { continue; }
            let url = entry.url.to_lowercase();
            let title = entry.title.to_lowercase();
            if !url.contains(&query) && !title.contains(&query) { continue; }
            suggestions.push(Suggestion {
                url: entry.url.clone(), title: entry.title.clone(),
                bookmarked: self.data.bookmarks.iter().any(|b| b.url == entry.url),
            });
        }
        // Exact and prefix matches precede substring matches; stable sort preserves MRU.
        suggestions.sort_by_key(|s| {
            let url = s.url.to_lowercase();
            let title = s.title.to_lowercase();
            let rank = if url == query || title == query { 0 } else if url.starts_with(&query) || title.starts_with(&query) { 1 } else { 2 };
            (rank, !s.bookmarked)
        });
        suggestions
    }

    pub fn search_template(&self) -> &str { &self.data.search_template }

    pub fn set_search_template(&mut self, template: &str) -> R<()> {
        let authority = template.strip_prefix("https://").unwrap_or("").split(['/', '?', '#']).next().unwrap_or("");
        if template.matches("{query}").count() != 1 || authority.contains("{query}") || template.replace("{query}", "").contains(['{', '}']) || !template.starts_with("https://") || !safe_url(&template.replace("{query}", "search")) {
            return Err("search template must be an HTTPS URL containing exactly one {query} and no credentials".into());
        }
        self.update(|data| data.search_template = template.into())
    }

    pub fn destination(&self, input: &str) -> R<String> {
        let input = input.trim();
        if input.is_empty() { return Err("address is empty".into()); }
        if ["http://", "https://"].iter().any(|prefix| input.get(..prefix.len()).is_some_and(|s| s.eq_ignore_ascii_case(prefix))) {
            if !valid_http_url(input) { return Err("invalid HTTP(S) address".into()); }
            return Ok(input.into());
        }
        if ["about:", "file:", "data:", "chrome:", "javascript:"].iter().any(|prefix| input.get(..prefix.len()).is_some_and(|s| s.eq_ignore_ascii_case(prefix))) {
            return Ok(input.into());
        }
        if !input.chars().any(char::is_whitespace) && looks_like_host(input) {
            let url = crate::page::normalize_url(input);
            if !valid_http_url(&url) { return Err("invalid address".into()); }
            return Ok(url);
        }
        Ok(self.data.search_template.replace("{query}", &encode_query(input)))
    }
}

fn valid_http_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else { return false; };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") { return false; }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    !authority.is_empty() && !url.chars().any(|c| c.is_control() || c.is_whitespace())
        && !authority.contains('\\') && !authority.starts_with(':')
}

fn safe_url(url: &str) -> bool {
    valid_http_url(url) && !url.split_once("://").unwrap().1.split(['/', '?', '#']).next().unwrap().contains('@')
}

fn looks_like_host(input: &str) -> bool {
    let authority = input.split(['/', '?', '#']).next().unwrap_or("");
    let (host, has_port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, suffix)) = rest.split_once(']') else { return false; };
        if host.parse::<std::net::Ipv6Addr>().is_err() { return false; }
        if suffix.is_empty() { return true; }
        return suffix.strip_prefix(':').is_some_and(|port| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()));
    } else if let Some((host, port)) = authority.split_once(':') {
        if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) { return false; }
        (host, true)
    } else {
        (authority, false)
    };
    if host.is_empty() || !host.split('.').all(|label| {
        !label.is_empty() && !label.starts_with('-') && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) { return false; }
    host.eq_ignore_ascii_case("localhost") || host.contains('.') || has_port
}

fn encode_query(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { out.push(b as char); }
        else { use std::fmt::Write; let _ = write!(out, "%{b:02X}"); }
    }
    out
}

fn lock(dir: &Path) -> R<File> {
    fs::create_dir_all(dir)?;
    let file = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join("address.lock"))?;
    file.lock()?;
    Ok(file)
}

fn read(dir: &Path) -> R<Data> {
    match fs::read(dir.join("address.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Data::default()),
        Err(e) => Err(e.into()),
    }
}

fn write(dir: &Path, data: &Data) -> R<()> {
    let bytes = serde_json::to_vec(data)?;
    let tmp = dir.join(format!(".address-{}-{}.tmp", std::process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed)));
    let result = (|| -> R<()> {
        let mut file = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, dir.join("address.json"))?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn book() -> (PathBuf, AddressBook) {
        let dir = std::env::temp_dir().join(format!("herdr-address-test-{}-{}", std::process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed)));
        let book = AddressBook::load_in(dir.clone()).unwrap();
        (dir, book)
    }

    #[test]
    fn history_bookmarks_and_concurrent_writers() {
        let (dir, mut a) = book();
        let mut b = AddressBook::load_in(dir.clone()).unwrap();
        a.record("https://example.org/a", "Alpha").unwrap();
        b.record("https://example.org/b", "Beta").unwrap();
        a.toggle_bookmark("https://example.org/a", "Alpha").unwrap();
        let suggestions = a.suggest("example");
        assert_eq!(suggestions.len(), 2);
        assert!(suggestions[0].bookmarked);
        assert_eq!(fs::metadata(dir.join("address.json")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(a.toggle_bookmark("https://example.org/a", "Alpha").unwrap(), Some(false));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn address_entry_preserves_navigation_and_distinguishes_searches() {
        let (dir, book) = book();
        for url in [
            "about:blank", "file:///tmp/a", "data:text/html,hello",
            "chrome://settings", "JAVASCRIPT:alert(1)", "HTTP://example.org/path",
            "HTTPS://example.org/path",
        ] {
            assert_eq!(book.destination(url).unwrap(), url);
        }
        for (input, expected) in [
            ("app.local", "http://app.local"),
            ("app.localhost:3000/a%2Fb?next=https://example.com/x%3Fy#part", "http://app.localhost:3000/a%2Fb?next=https://example.com/x%3Fy#part"),
            ("192.168.1.20:5173", "http://192.168.1.20:5173"),
            ("8.8.8.8", "https://8.8.8.8"),
            ("[::1]:4000", "http://[::1]:4000"),
            ("example.org/a%2Fb?q=x%26y#part", "https://example.org/a%2Fb?q=x%26y#part"),
            ("why?", "https://duckduckgo.com/?q=why%3F"),
            ("hello,world", "https://duckduckgo.com/?q=hello%2Cworld"),
            ("rust:language", "https://duckduckgo.com/?q=rust%3Alanguage"),
            ("hello.world!", "https://duckduckgo.com/?q=hello.world%21"),
            ("ftp://example.org", "https://duckduckgo.com/?q=ftp%3A%2F%2Fexample.org"),
        ] {
            assert_eq!(book.destination(input).unwrap(), expected, "{input}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn destinations_and_private_history() {
        let (dir, mut book) = book();
        assert_eq!(book.destination("localhost:3000/a?q=x#top").unwrap(), "http://localhost:3000/a?q=x#top");
        assert_eq!(book.destination("hello world").unwrap(), "https://duckduckgo.com/?q=hello%20world");
        book.set_search_template("https://example.org/search?q={query}").unwrap();
        assert_eq!(book.destination("a & b").unwrap(), "https://example.org/search?q=a%20%26%20b");
        assert!(book.set_search_template("http://example.org/{query}").is_err());
        assert!(book.set_search_template("https://user:password@example.org/{query}").is_err());
        book.record("https://user:password@example.org/", "secret").unwrap();
        book.record("chrome://settings", "settings").unwrap();
        book.record("HTTP://user:password@example.org/", "secret").unwrap();
        book.record("javascript:alert(1)", "script").unwrap();
        assert_eq!(book.toggle_bookmark("file:///tmp/a", "file").unwrap(), None);
        assert!(book.suggest("").is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}

//! Small shared helpers.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A cancel flag that can be shared with a running job.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    /// Resolves once `cancel` has been called.
    pub async fn cancelled(&self) {
        while !self.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// A short id that is unique for this install.
pub fn new_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{}{:x}{:x}", prefix, nanos, COUNTER.fetch_add(1, Ordering::SeqCst))
}

/// Case-insensitive match of `name` against a simple pattern with `*` and `?`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ni;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The last path component as text, or the whole path when there is none.
pub fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// Normalizes a Koofr path: forward slashes, no leading or trailing slash.
pub fn clean_remote(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect::<Vec<_>>()
        .join("/")
}

/// Normalizes a local path for comparisons: no trailing separator, and
/// lower case on Windows where paths are case-insensitive.
pub fn norm_local(path: &str) -> String {
    let mut p = path.replace('/', std::path::MAIN_SEPARATOR_STR);
    if cfg!(windows) {
        p = p.replace('/', "\\").to_lowercase();
    }
    while p.len() > 1 && (p.ends_with('\\') || p.ends_with('/')) {
        p.pop();
    }
    p
}

/// True when one path is the same as, or inside, the other.
pub fn paths_overlap(a: &str, b: &str, sep: char) -> bool {
    if a == b {
        return true;
    }
    let inside = |inner: &str, outer: &str| {
        outer.is_empty() || (inner.starts_with(outer) && inner[outer.len()..].starts_with(sep))
    };
    inside(a, b) || inside(b, a)
}

/// Removes ANSI color codes that can show up in rclone messages.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("~$*", "~$Budget.xlsx"));
        assert!(glob_match("*.tmp", "a.TMP"));
        assert!(glob_match("Thumbs.db", "thumbs.db"));
        assert!(!glob_match("*.tmp", "a.tmpx"));
        assert!(glob_match("a?c*", "abcdef"));
        assert!(!glob_match("a?c", "ac"));
    }

    #[test]
    fn remote_paths() {
        assert_eq!(clean_remote("/Docs//Work/"), "Docs/Work");
        assert_eq!(clean_remote("\\Docs\\..\\x"), "Docs/x");
        assert_eq!(clean_remote("/"), "");
    }

    #[test]
    fn overlap() {
        assert!(paths_overlap("docs", "docs", '/'));
        assert!(paths_overlap("docs/work", "docs", '/'));
        assert!(paths_overlap("docs", "docs/work", '/'));
        assert!(!paths_overlap("docs2", "docs", '/'));
        assert!(paths_overlap("", "docs", '/'));
    }

    #[test]
    fn ansi() {
        assert_eq!(strip_ansi("\u{1b}[31mBisync aborted.\u{1b}[0m"), "Bisync aborted.");
    }
}

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

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    // Days since 1970-01-01, after Howard Hinnant's algorithm.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Reads a timestamp like "2026-11-13T15:10:34.650367249+0000" or
/// "2026-11-13T10:10:34.65-05:00" or "...Z" into nanoseconds since 1970.
pub fn parse_time_ns(text: &str) -> Option<i64> {
    let t = text.trim();
    let num = |a: usize, b: usize| t.get(a..b)?.parse::<i64>().ok();
    if t.len() < 19 || t.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = &t[19..];
    let mut nanos: i64 = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        rest = &frac[digits.len()..];
        let mut padded = digits.chars().take(9).collect::<String>();
        while padded.len() < 9 {
            padded.push('0');
        }
        nanos = padded.parse().ok()?;
    }
    let offset_secs: i64 = if rest.is_empty() || rest == "Z" || rest == "z" {
        0
    } else {
        let sign = match rest.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let digits: String = rest[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() != 4 {
            return None;
        }
        sign * (digits[0..2].parse::<i64>().ok()? * 3600 + digits[2..4].parse::<i64>().ok()? * 60)
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset_secs;
    secs.checked_mul(1_000_000_000)?.checked_add(nanos)
}

/// Undoes Go-style string quoting as used in rclone's sync records, for
/// example "sub dir/we\"ird.txt". The text passed in excludes the outer quotes.
pub fn go_unquote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(e) = chars.next() else { break };
        let hex = |n: usize, chars: &mut std::str::Chars| -> Option<char> {
            let h: String = chars.by_ref().take(n).collect();
            u32::from_str_radix(&h, 16).ok().and_then(char::from_u32)
        };
        match e {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\u{07}'),
            'b' => out.push('\u{08}'),
            'f' => out.push('\u{0C}'),
            'v' => out.push('\u{0B}'),
            'x' => {
                if let Some(ch) = hex(2, &mut chars) {
                    out.push(ch)
                }
            }
            'u' => {
                if let Some(ch) = hex(4, &mut chars) {
                    out.push(ch)
                }
            }
            'U' => {
                if let Some(ch) = hex(8, &mut chars) {
                    out.push(ch)
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Why a folder must not be synced, or None when it is fine. Covers Windows'
/// own folders, program folders and the folders that hold program settings.
/// `windows` and `env` are passed in so the rules can be tested anywhere.
pub fn system_folder_reason(path: &str, windows: bool, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let sep = if windows { '\\' } else { '/' };
    let norm = |p: &str| {
        let mut p = if windows { p.replace('/', "\\").to_lowercase() } else { p.to_string() };
        while p.len() > 1 && p.ends_with(sep) {
            p.pop();
        }
        p
    };
    let path = norm(path);
    let inside = |outer: &str| path == outer || (path.starts_with(outer) && path[outer.len()..].starts_with(sep));
    const SYSTEM: &str = "That's a Windows system folder. Syncing it could break programs or Windows itself. Pick a folder of your own.";
    const PROGRAMS: &str = "That folder holds installed programs. Syncing it could break them. Pick a folder of your own.";
    const SETTINGS: &str = "That folder holds settings files that programs keep open all the time. Pick a folder of your own instead.";
    const PROFILE: &str = "That's your whole user folder, which includes settings files Windows keeps locked. Pick a folder inside it instead, like Documents.";
    const USERS: &str = "That folder holds every user account on this PC. Pick a folder of your own.";
    if windows {
        let var = |name: &str| env(name).map(|v| norm(&v)).filter(|v| v.len() > 3);
        if let Some(dir) = var("SystemRoot").or_else(|| var("windir")) {
            if inside(&dir) {
                return Some(SYSTEM.into());
            }
        }
        for name in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData"] {
            if let Some(dir) = var(name) {
                if inside(&dir) {
                    return Some(PROGRAMS.into());
                }
            }
        }
        if let Some(home) = var("USERPROFILE") {
            if path == home {
                return Some(PROFILE.into());
            }
            let app_data = format!("{home}\\appdata");
            for exact in [app_data.clone(), format!("{app_data}\\local"), format!("{app_data}\\locallow"), format!("{app_data}\\roaming")] {
                if path == exact {
                    return Some(SETTINGS.into());
                }
            }
            if let Some(users) = home.rfind('\\').map(|i| home[..i].to_string()).filter(|u| u.len() > 3) {
                if path == users {
                    return Some(USERS.into());
                }
            }
        }
        for name in ["APPDATA", "LOCALAPPDATA"] {
            if var(name).is_some_and(|dir| path == dir) {
                return Some(SETTINGS.into());
            }
        }
    } else {
        for dir in ["/bin", "/boot", "/dev", "/etc", "/lib", "/lib64", "/proc", "/sbin", "/sys", "/usr", "/var", "/System", "/Library", "/Applications"] {
            if inside(dir) {
                return Some("That's a system folder. Syncing it could break programs. Pick a folder of your own.".into());
            }
        }
        if let Some(home) = env("HOME").map(|h| norm(&h)).filter(|h| h.len() > 1) {
            if path == home {
                return Some("That's your whole home folder, which includes settings files programs keep open. Pick a folder inside it instead, like Documents.".into());
            }
        }
    }
    None
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
    fn times() {
        let a = parse_time_ns("2026-11-13T15:10:34.650367249+0000").unwrap();
        let b = parse_time_ns("2026-11-13T10:10:34.650367249-05:00").unwrap();
        assert_eq!(a, b);
        assert_eq!(parse_time_ns("1970-01-01T00:00:01Z"), Some(1_000_000_000));
        assert_eq!(parse_time_ns("2000-02-29T00:00:00.5Z"), Some(951_782_400_500_000_000));
        assert!(parse_time_ns("yesterday").is_none());
    }

    #[test]
    fn unquote() {
        assert_eq!(go_unquote(r#"sub dir/we\"ird [x].txt"#), "sub dir/we\"ird [x].txt");
        assert_eq!(go_unquote(r"back\\slash"), "back\\slash");
        assert_eq!(go_unquote(r"tab\there"), "tab\there");
        assert_eq!(go_unquote(r"\u00e9t\xe9"), "\u{e9}t\u{e9}");
        assert_eq!(go_unquote("plain émoji 😀"), "plain émoji 😀");
    }

    #[test]
    fn ansi() {
        assert_eq!(strip_ansi("\u{1b}[31mBisync aborted.\u{1b}[0m"), "Bisync aborted.");
    }

    #[test]
    fn system_folders() {
        let env = |name: &str| match name {
            "SystemRoot" => Some(r"C:\Windows".to_string()),
            "ProgramFiles" => Some(r"C:\Program Files".to_string()),
            "ProgramFiles(x86)" => Some(r"C:\Program Files (x86)".to_string()),
            "ProgramData" => Some(r"C:\ProgramData".to_string()),
            "USERPROFILE" => Some(r"C:\Users\Robert".to_string()),
            "APPDATA" => Some(r"C:\Users\Robert\AppData\Roaming".to_string()),
            "LOCALAPPDATA" => Some(r"C:\Users\Robert\AppData\Local".to_string()),
            _ => None,
        };
        let why = |p: &str| system_folder_reason(p, true, &env);
        assert!(why(r"C:\Windows").is_some());
        assert!(why(r"c:\windows\System32\").is_some());
        assert!(why(r"C:\Program Files\SyncHopper").is_some());
        assert!(why(r"C:\Program Files (x86)").is_some());
        assert!(why(r"C:\ProgramData\Thing").is_some());
        assert!(why(r"C:\Users\Robert").unwrap().contains("whole user folder"));
        assert!(why(r"C:\Users").is_some());
        assert!(why(r"C:\Users\Robert\AppData").is_some());
        assert!(why(r"C:\Users\Robert\AppData\Roaming").is_some());
        assert!(why(r"C:\Users\Robert\AppData\Local\").is_some());
        // Ordinary folders, including ones that only start with the same letters.
        assert!(why(r"C:\Users\Robert\Documents").is_none());
        assert!(why(r"C:\Users\Robert\AppData\Roaming\SomeGame\Saves").is_none());
        assert!(why(r"C:\WindowsStuff").is_none());
        assert!(why(r"D:\Program Files backup").is_none());
        assert!(why(r"C:\Users\Robert\Koofr\Taxes").is_none());
        let unix = |name: &str| if name == "HOME" { Some("/home/robert".to_string()) } else { None };
        assert!(system_folder_reason("/etc/ssh", false, &unix).is_some());
        assert!(system_folder_reason("/home/robert/", false, &unix).is_some());
        assert!(system_folder_reason("/home/robert/Documents", false, &unix).is_none());
    }
}

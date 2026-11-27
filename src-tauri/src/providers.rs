//! What SyncHopper knows about each cloud service it works with: its name,
//! how an account signs in, and the rules and limits that are particular to
//! it. Everything else in the app is the same for every service.

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Provider {
    /// Saved with each account, so it never changes.
    pub id: &'static str,
    /// The service's name as people know it.
    pub name: &'static str,
    /// What the sync engine calls this kind of account.
    pub kind: &'static str,
    /// How an account signs in: "password" (an email and a password made for
    /// apps), "browser" (the service's own sign-in page) or "api_key" (an
    /// email, a password and a key).
    pub sign_in: &'static str,
    /// Whether new accounts talk HTTP/2. Koofr uploads are much slower with
    /// it, so Koofr starts with it off.
    pub http2_default: bool,
    /// The HTTP/2 choice is only offered where it matters.
    pub http2_choice: bool,
    /// The request rate the service asks apps to stay under, where it has
    /// one. Offered as a setting, off by default.
    pub request_limit: Option<u32>,
    /// The longest path the service accepts, in characters.
    pub max_path: Option<usize>,
    /// The service refuses emoji and some other symbols in names.
    pub refuses_emoji: bool,
    /// The drive letter suggested first.
    pub letter: &'static str,
}

pub const KOOFR: Provider = Provider {
    id: "koofr",
    name: "Koofr",
    kind: "koofr",
    sign_in: "password",
    http2_default: false,
    http2_choice: true,
    request_limit: Some(40),
    max_path: Some(1023),
    refuses_emoji: true,
    letter: "K",
};

pub const PCLOUD: Provider = Provider {
    id: "pcloud",
    name: "pCloud",
    kind: "pcloud",
    sign_in: "browser",
    http2_default: true,
    http2_choice: false,
    request_limit: None,
    max_path: None,
    refuses_emoji: false,
    letter: "P",
};

pub const FILEN: Provider = Provider {
    id: "filen",
    name: "Filen",
    kind: "filen",
    sign_in: "api_key",
    http2_default: true,
    http2_choice: false,
    request_limit: None,
    max_path: None,
    refuses_emoji: false,
    letter: "F",
};

/// In the order the Add cloud provider screen lists them.
pub static ALL: [&Provider; 3] = [&KOOFR, &PCLOUD, &FILEN];

pub fn find(id: &str) -> Option<&'static Provider> {
    ALL.iter().copied().find(|p| p.id == id)
}

/// The provider for a saved account. An id this version doesn't know (a
/// file from a newer version) is treated as Koofr rather than failing.
pub fn get(id: &str) -> &'static Provider {
    find(id).unwrap_or(&KOOFR)
}

pub const KOOFR_PASSWORD_URL: &str = "https://app.koofr.net/app/admin/preferences/password";
pub const FILEN_CLI_URL: &str = "https://github.com/FilenCloudDienste/filen-cli/releases/latest";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_are_found_by_id() {
        assert_eq!(get("pcloud").name, "pCloud");
        assert_eq!(get("filen").sign_in, "api_key");
        assert_eq!(get("something-new").id, "koofr");
        assert!(find("something-new").is_none());
        let mut ids: Vec<&str> = ALL.iter().map(|p| p.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), ALL.len());
    }
}

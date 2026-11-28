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
    /// apps), "browser" (the service's own sign-in page) or "keys" (an email
    /// and the account password, used once to sign in and unlock the
    /// account's keys; only the keys are saved).
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
    /// Signing in also needs to know which of the user's drives to use
    /// (kDrive: the number in the kDrive's web address).
    pub asks_drive: bool,
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
    asks_drive: false,
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
    asks_drive: false,
};

pub const FILEN: Provider = Provider {
    id: "filen",
    name: "Filen",
    kind: "filen",
    sign_in: "keys",
    http2_default: true,
    http2_choice: false,
    request_limit: None,
    max_path: None,
    refuses_emoji: false,
    letter: "F",
    asks_drive: false,
};

/// Infomaniak's kDrive. Signs in on Infomaniak's own page, and needs to be
/// told which kDrive to use, since one sign-in can have several.
pub const KDRIVE: Provider = Provider {
    id: "kdrive",
    name: "kDrive",
    kind: "kdrive",
    sign_in: "browser",
    http2_default: true,
    http2_choice: false,
    request_limit: None,
    max_path: None,
    refuses_emoji: false,
    letter: "I",
    asks_drive: true,
};

/// In the order the Add cloud provider screen lists them.
pub static ALL: [&Provider; 4] = [&KOOFR, &PCLOUD, &FILEN, &KDRIVE];

/// The kDrive's number, from what someone typed or pasted: the number
/// itself, or the address of the kDrive in a browser, which has the number
/// after "/drive/".
pub fn kdrive_id(typed: &str) -> Option<String> {
    let typed = typed.trim();
    let digits = |s: &str| -> String { s.chars().take_while(|c| c.is_ascii_digit()).collect() };
    let id = if !typed.is_empty() && typed.chars().all(|c| c.is_ascii_digit()) {
        typed.to_string()
    } else {
        let lower = typed.to_ascii_lowercase();
        let at = lower.find("/drive/")?;
        digits(&typed[at + "/drive/".len()..])
    };
    (!id.is_empty() && id.len() <= 12 && id.chars().any(|c| c != '0')).then_some(id)
}

pub fn find(id: &str) -> Option<&'static Provider> {
    ALL.iter().copied().find(|p| p.id == id)
}

/// The provider for a saved account. An id this version doesn't know (a
/// file from a newer version) is treated as Koofr rather than failing.
pub fn get(id: &str) -> &'static Provider {
    find(id).unwrap_or(&KOOFR)
}

pub const KOOFR_PASSWORD_URL: &str = "https://app.koofr.net/app/admin/preferences/password";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_are_found_by_id() {
        assert_eq!(get("pcloud").name, "pCloud");
        assert_eq!(get("filen").sign_in, "keys");
        assert_eq!((get("kdrive").name, get("kdrive").sign_in, get("kdrive").asks_drive), ("kDrive", "browser", true));
        assert!(ALL.iter().filter(|p| p.asks_drive).count() == 1);
    }

    #[test]
    fn the_kdrive_number_is_found_in_what_people_paste() {
        // The address forms rclone's own help and kDrive's web app show.
        for typed in [
            "654321",
            "  654321\n",
            "https://ksuite.infomaniak.com/1234567/kdrive/app/drive/654321/files/5",
            "https://ksuite.infomaniak.com/kdrive/app/drive/654321/files",
            "https://kdrive.infomaniak.com/app/drive/654321/files/12/preview",
            "kdrive.infomaniak.com/app/Drive/654321",
            "https://kdrive.infomaniak.com/app/drive/654321?x=1",
        ] {
            assert_eq!(kdrive_id(typed).as_deref(), Some("654321"), "{typed:?}");
        }
        // A number with a zero in front is kept as typed.
        assert_eq!(kdrive_id("0654321").as_deref(), Some("0654321"));
        for typed in ["", "   ", "my drive", "https://ksuite.infomaniak.com/1234567/kdrive/app/", "https://kdrive.infomaniak.com/app/drive/", "/drive/abc", "12a34", "0", "1234567890123"] {
            assert_eq!(kdrive_id(typed), None, "{typed:?}");
        }
        assert_eq!(get("something-new").id, "koofr");
        assert!(find("something-new").is_none());
        let mut ids: Vec<&str> = ALL.iter().map(|p| p.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), ALL.len());
    }
}

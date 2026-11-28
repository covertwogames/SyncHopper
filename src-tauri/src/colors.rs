//! The colors an account can have. Each account's icon, in File Explorer's
//! sidebar and in SyncHopper's own window, is the SyncHopper mark on its
//! color, so several accounts can be told apart at a glance.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize)]
pub struct Color {
    /// Saved with the account.
    pub id: &'static str,
    pub name: &'static str,
    /// For the screens.
    pub hex: &'static str,
}

/// In the order new accounts get them. The first is the color of the app's
/// own icon, so a single account looks as it always has. The order keeps the
/// first six apart for people with the common kinds of color blindness.
pub static ALL: [Color; 8] = [
    Color { id: "teal", name: "Teal", hex: "#0b7a75" },
    Color { id: "blue", name: "Blue", hex: "#2d78e1" },
    Color { id: "orange", name: "Orange", hex: "#d6600a" },
    Color { id: "purple", name: "Purple", hex: "#652db4" },
    Color { id: "pink", name: "Pink", hex: "#d94f9f" },
    Color { id: "red", name: "Red", hex: "#b3262f" },
    Color { id: "slate", name: "Slate", hex: "#4d5663" },
    Color { id: "gold", name: "Gold", hex: "#7d5e00" },
];

pub fn find(id: &str) -> Option<&'static Color> {
    ALL.iter().find(|c| c.id == id)
}

/// The color for a new account: the first one no account has yet, or, once
/// every color is taken, the one the fewest accounts have.
pub fn next_free(used: &[&str]) -> &'static str {
    let count = |id: &str| used.iter().filter(|u| **u == id).count();
    ALL.iter().min_by_key(|c| count(c.id)).map(|c| c.id).unwrap_or("teal")
}

/// The icon files, made from the app's own icon with the teal swapped for
/// each color. Teal has none: it is the app's icon itself.
fn icon_bytes(id: &str) -> Option<&'static [u8]> {
    Some(match id {
        "blue" => include_bytes!("../icons/accounts/blue.ico"),
        "orange" => include_bytes!("../icons/accounts/orange.ico"),
        "purple" => include_bytes!("../icons/accounts/purple.ico"),
        "pink" => include_bytes!("../icons/accounts/pink.ico"),
        "red" => include_bytes!("../icons/accounts/red.ico"),
        "slate" => include_bytes!("../icons/accounts/slate.ico"),
        "gold" => include_bytes!("../icons/accounts/gold.ico"),
        _ => return None,
    })
}

fn icon_file(data: &Path, id: &str) -> PathBuf {
    data.join("icons").join(format!("{id}.ico"))
}

/// What File Explorer is told to show for an account of this color: the
/// app's own icon for teal, and otherwise that color's icon file, which is
/// put in SyncHopper's data folder if it is not there already. Falls back
/// to the app's icon when the file can't be written.
pub fn explorer_icon(exe: &Path, data: &Path, id: &str) -> String {
    let own = format!("{},0", exe.to_string_lossy());
    let Some(bytes) = icon_bytes(id) else { return own };
    let file = icon_file(data, id);
    if std::fs::read(&file).map(|have| have != bytes).unwrap_or(true) {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&file, bytes).is_err() {
            return own;
        }
    }
    file.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accounts_take_the_colors_in_order() {
        assert_eq!(next_free(&[]), "teal");
        assert_eq!(next_free(&["teal"]), "blue");
        assert_eq!(next_free(&["teal", "blue"]), "orange");
        // A color given up (or changed by hand) is offered again.
        assert_eq!(next_free(&["blue", "orange"]), "teal");
        assert_eq!(next_free(&["teal", "orange", "orange"]), "blue");
        // All eight taken: the least used, earliest first.
        let all: Vec<&str> = ALL.iter().map(|c| c.id).collect();
        assert_eq!(next_free(&all), "teal");
        let mut nine = all.clone();
        nine.push("teal");
        assert_eq!(next_free(&nine), "blue");
        assert!(find("blue").is_some() && find("mauve").is_none() && find("").is_none());
    }

    #[test]
    fn every_color_but_teal_has_an_icon_file_and_teal_is_the_apps_own() {
        let dir = tempfile::tempdir().unwrap();
        let exe = Path::new("/apps/SyncHopper/synchopper.exe");
        assert_eq!(explorer_icon(exe, dir.path(), "teal"), "/apps/SyncHopper/synchopper.exe,0");
        assert_eq!(explorer_icon(exe, dir.path(), "not a color"), "/apps/SyncHopper/synchopper.exe,0");
        assert!(!dir.path().join("icons").exists(), "nothing is written for teal");
        let mut seen = std::collections::HashSet::new();
        for c in ALL.iter().filter(|c| c.id != "teal") {
            let shown = explorer_icon(exe, dir.path(), c.id);
            assert_eq!(shown, dir.path().join("icons").join(format!("{}.ico", c.id)).to_string_lossy());
            let bytes = std::fs::read(&shown).unwrap();
            // A Windows icon file with the same six sizes as the app's icon.
            assert_eq!(&bytes[..6], &[0, 0, 1, 0, 6, 0], "{}", c.id);
            assert!(seen.insert(bytes), "{} is a copy of another color's file", c.id);
        }
        // A damaged file is put right the next time.
        let blue = dir.path().join("icons/blue.ico");
        std::fs::write(&blue, b"junk").unwrap();
        explorer_icon(exe, dir.path(), "blue");
        assert!(std::fs::read(&blue).unwrap().len() > 1000);
        // Each color's hex is a color.
        for c in ALL.iter() {
            assert!(c.hex.len() == 7 && c.hex.starts_with('#') && u32::from_str_radix(&c.hex[1..], 16).is_ok(), "{}", c.id);
        }
    }
}

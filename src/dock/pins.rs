//! Apps pinned to the dock, kept in `%APPDATA%\wisp\dock-pins.toml` in dock order.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::indexer::AppEntry;
use crate::util;

#[derive(Serialize, Deserialize)]
struct Pin {
    id: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    pin: Vec<Pin>,
}

fn path() -> PathBuf {
    util::data_dir().join("dock-pins.toml")
}

fn parse(text: &str) -> Vec<AppEntry> {
    toml::from_str::<File>(text)
        .map(|file| {
            file.pin
                .into_iter()
                .filter(|pin| !pin.id.is_empty())
                .map(|pin| AppEntry {
                    name: pin.name,
                    id: pin.id,
                    path: pin.path,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn render<'a>(apps: impl IntoIterator<Item = &'a AppEntry>) -> Option<String> {
    let file = File {
        pin: apps
            .into_iter()
            .map(|app| Pin {
                id: app.id.clone(),
                name: app.name.clone(),
                path: app.path.clone(),
            })
            .collect(),
    };
    toml::to_string(&file).ok()
}

/// The pinned apps, in dock order. A missing or unreadable file pins nothing.
pub fn load() -> Vec<AppEntry> {
    std::fs::read_to_string(path())
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// Saves the pins. The file is written aside first so a crash keeps the old one.
pub fn save<'a>(apps: impl IntoIterator<Item = &'a AppEntry>) {
    let Some(text) = render(apps) else { return };
    let path = path();
    let tmp = path.with_extension("toml.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_keep_their_order_through_the_file() {
        let apps = [
            AppEntry {
                name: "Notepad".into(),
                id: r"C:\Windows\notepad.exe".into(),
                path: Some(r"C:\Windows\notepad.exe".into()),
            },
            AppEntry {
                name: "Store app".into(),
                id: "Package!App".into(),
                path: None,
            },
        ];
        let loaded = parse(&render(&apps).unwrap());
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            (loaded[0].id.as_str(), loaded[0].path.as_deref()),
            (apps[0].id.as_str(), apps[0].path.as_deref())
        );
        assert_eq!(
            (loaded[1].name.as_str(), loaded[1].path.as_deref()),
            ("Store app", None)
        );
    }

    #[test]
    fn a_broken_file_pins_nothing() {
        assert!(parse("pin = 3").is_empty());
        assert!(parse("").is_empty());
    }
}

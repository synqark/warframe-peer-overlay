//! The players we have shared a squad with, as they were when it came apart.
//!
//! A member is written down the moment they drop out of the squad, which is the moment
//! everything about them is known: what they were called, their mastery rank, the platform
//! they played on, where they connected from, the loadout they brought, and the mission they
//! played with it (see `mission`). That moment stands as the match's own time. Only those who
//! played a mission with us, and whose loadout was captured, are kept: what the history is for
//! is the loadouts players bring to missions, and one who left before any started brought
//! theirs to none.
//!
//! The newest `KEPT` of them live, newest first, in `history.json` beside the loadouts. What
//! a card shows is kept; the loadout's whole JSON is not, since that would run to tens of
//! megabytes (the copy saved under `loadouts` still has it). Each entry names that copy, which
//! is how a clear-out of the folder knows which files are still worth keeping and which have
//! fallen off the end of the history along with the player they belong to.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};

use crate::{mission::Mission, monitor::LoadoutView};

/// How many players are kept; the oldest fall off the end.
const KEPT: usize = 5000;

/// One player, as they were when the squad they were in came apart.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// The moment of it, in seconds since the epoch: what the list is ordered by.
    pub at: u64,
    /// The same moment as the wall clock had it, written down there and then, so that showing
    /// it later needs no calendar of our own.
    pub when: String,
    /// What their loadout was saved as under `loadouts`: what a clear-out of the folder spares
    /// for as long as this entry stands. Empty for an entry written down before that was noted,
    /// or one whose capture was never saved.
    #[serde(default)]
    pub file: String,
    /// The mission they were last tied to (see `mission`): the node it was played at,
    /// `SolNode228`, and its type, `MT_LANDSCAPE`, by the ids EE.log gives them (named by
    /// `names::node_name` and `names::mission_type_name`). Both empty only for an entry
    /// written down before missions were, when players who played none were kept as well.
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub mission_type: String,
    /// Everything a card shows of them.
    pub view: LoadoutView,
}

/// Everyone written down so far, newest first.
#[derive(Default)]
pub struct History {
    entries: Vec<HistoryEntry>,
}

impl History {
    /// Whatever earlier runs wrote down.
    pub fn load() -> Self {
        let entries = file().and_then(|file| read(&file)).unwrap_or_default();
        Self { entries }
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Writes a player down as they left, along with the file their loadout was saved as and
    /// the mission they were tied to. `save` puts the lot away once the last of a squad is in,
    /// rather than once for each of them.
    pub fn record(&mut self, view: LoadoutView, file: String, mission: &Mission) {
        let entry = HistoryEntry {
            at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            when: on_the_wall(),
            file,
            location: mission.location.clone(),
            mission_type: mission.mission_type.clone(),
            view,
        };
        remember(&mut self.entries, entry);
    }

    /// The loadout files the entries were made from: what a clear-out of `loadouts` must spare.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .map(|entry| entry.file.as_str())
            .filter(|file| !file.is_empty())
    }

    /// When the last departure was written down. Nothing captured since can be let go: its
    /// squad has yet to come apart.
    pub fn horizon(&self) -> u64 {
        self.entries.first().map_or(0, |entry| entry.at)
    }

    /// Keeps what has been written down for the runs to come.
    pub fn save(&self) {
        if let Some(file) = file() {
            write(&file, &self.entries);
        }
    }
}

/// The newest first, and no more of them than we keep.
fn remember(entries: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    entries.insert(0, entry);
    entries.truncate(KEPT);
}

fn file() -> Option<PathBuf> {
    ProjectDirs::from("com", "synqark", "WarframePeerOverlay")
        .map(|dirs| dirs.data_local_dir().join("history.json"))
}

fn read(file: &Path) -> Option<Vec<HistoryEntry>> {
    serde_json::from_slice(&fs::read(file).ok()?).ok()
}

fn write(file: &Path, entries: &[HistoryEntry]) {
    if let Some(directory) = file.parent()
        && fs::create_dir_all(directory).is_ok()
        && let Ok(json) = serde_json::to_vec(entries)
    {
        let _ = fs::write(file, json);
    }
}

/// The wall clock as `2026-09-13 15:32`.
fn on_the_wall() -> String {
    let mut now: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut now) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, at: u64) -> HistoryEntry {
        HistoryEntry {
            at,
            when: "2026-09-13 15:32".to_owned(),
            file: format!("{at}_{name}_PC.json"),
            location: String::new(),
            mission_type: String::new(),
            view: LoadoutView {
                name: name.to_owned(),
                platform: "PC".to_owned(),
                ..LoadoutView::default()
            },
        }
    }

    #[test]
    fn keeps_the_newest_and_lets_the_rest_go() {
        let mut entries = Vec::new();
        for at in 0..KEPT as u64 + 10 {
            remember(&mut entries, entry(&format!("Tenno{at}"), at));
        }

        assert_eq!(entries.len(), KEPT);
        assert_eq!(entries[0].at, KEPT as u64 + 9, "the newest leads");
        assert_eq!(entries[KEPT - 1].at, 10, "the oldest ten are gone");
    }

    #[test]
    fn writes_down_the_mission_a_player_was_tied_to() {
        let mut history = History::default();
        let player = |name: &str| LoadoutView {
            name: name.to_owned(),
            ..LoadoutView::default()
        };
        let mission = Mission {
            location: "SolNode6".to_owned(),
            mission_type: "MT_EXTERMINATION".to_owned(),
            ..Mission::default()
        };
        history.record(player("Tenno"), String::new(), &mission);

        let entry = &history.entries()[0];
        assert_eq!(entry.view.name, "Tenno");
        assert_eq!(
            (entry.location.as_str(), entry.mission_type.as_str()),
            ("SolNode6", "MT_EXTERMINATION")
        );
    }

    #[test]
    fn reads_an_entry_written_down_before_missions_were() {
        let json = r#"[{"at":10,"when":"2026-09-13 15:32","file":"10_Tenno_PC.json","view":{"name":"Tenno","platform":"PC"}}]"#;

        let entries: Vec<HistoryEntry> = serde_json::from_str(json).unwrap();

        assert_eq!(entries[0].view.name, "Tenno");
        assert_eq!(entries[0].location, "");
        assert_eq!(entries[0].mission_type, "");
    }

    #[test]
    fn writes_everyone_down_and_reads_them_back() {
        // The thread name contains `::`, which Windows rejects in a file name.
        let file = std::env::temp_dir().join(format!(
            "warframe-peer-overlay-history-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&file);
        assert!(read(&file).is_none(), "nothing written down yet");

        let entries = vec![entry("Tenno", 20), entry("Lotus", 10)];
        write(&file, &entries);

        let read_back = read(&file).expect("what was written down comes back");
        assert_eq!(
            read_back
                .iter()
                .map(|entry| (entry.view.name.as_str(), entry.at))
                .collect::<Vec<_>>(),
            [("Tenno", 20), ("Lotus", 10)]
        );
        fs::remove_file(file).unwrap();
    }

    #[test]
    fn names_the_loadout_files_still_worth_keeping() {
        let history = History {
            entries: vec![
                entry("Tenno", 20),
                entry("Lotus", 10),
                // Written down before the file was noted: it names nothing to spare.
                HistoryEntry {
                    file: String::new(),
                    ..entry("Ordis", 5)
                },
            ],
        };

        assert_eq!(history.horizon(), 20, "the newest leads");
        let mut files = history.files().collect::<Vec<_>>();
        files.sort();
        assert_eq!(files, ["10_Lotus_PC.json", "20_Tenno_PC.json"]);
        assert_eq!(History::default().horizon(), 0, "nothing written down yet");
    }

    #[test]
    fn tells_the_time_as_the_wall_clock_has_it() {
        let now = on_the_wall();
        assert_eq!(now.len(), 16, "{now} reads as 2026-09-13 15:32");
        assert!(now.starts_with("20"));
    }
}

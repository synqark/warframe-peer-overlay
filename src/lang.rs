//! The language every window is written in, and the words themselves.
//!
//! Japanese is what the windows were laid out and sized against, so it is the original of
//! each pair below; English is the translation, kept beside it line for line so a wording is
//! never changed in one language and forgotten in the other. English is nonetheless what the
//! overlay opens in (`Language::DEFAULT`) until the user picks otherwise, since it is the
//! language more of the people this is handed to read.
//!
//! The choice lives in a process-wide atomic rather than being threaded through every
//! function: the words are wanted deep inside layout code, on the UI thread and on the
//! monitor thread alike, and passing a language down to each of them would touch every
//! signature for nothing. It is written down in `settings.json` beside the loadouts and read
//! back at start-up, and the tray hands a change to `main`, which saves it and starts the
//! overlay again — the windows measure their columns against the text they hold, so a
//! language is picked up cleanly at launch rather than mid-layout.
//!
//! Only what the project writes itself is translated. Names the game gives — items, nodes,
//! mission types — come from the export's `en` dictionary in either language (see `names`),
//! and so do `HOST`, `MR`, platform names and the windows' titles.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU8, Ordering},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Which language the windows are written in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// What the windows were laid out and sized against, and the original of every pair.
    Japanese,
    /// What the overlay opens in until a language is chosen.
    #[default]
    English,
}

impl Language {
    /// Both of them, in the order the tray menu offers them.
    pub const ALL: [Language; 2] = [Language::Japanese, Language::English];

    /// What the overlay opens in when nothing has been chosen. Kept beside `#[default]`
    /// above so `CURRENT` can start from it, which a `Default::default()` call cannot do
    /// in a `static`.
    pub const DEFAULT: Language = Language::English;

    /// How the tray menu names it — in the language itself, so either is recognisable
    /// whichever one the menu happens to be showing.
    pub fn label(self) -> &'static str {
        match self {
            Language::Japanese => "日本語",
            Language::English => "English",
        }
    }

    /// Whichever of the two is this one. Both sides are worked out before the choice is
    /// made, so this is for words already to hand rather than anything costly.
    pub fn pick<T>(self, japanese: T, english: T) -> T {
        match self {
            Language::Japanese => japanese,
            Language::English => english,
        }
    }

    const fn code(self) -> u8 {
        match self {
            Language::Japanese => 0,
            Language::English => 1,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Language::English,
            _ => Language::Japanese,
        }
    }
}

/// The language in force, as `Language::code` gives it. Read on every line of text laid out,
/// so it is an atomic rather than a lock.
static CURRENT: AtomicU8 = AtomicU8::new(Language::DEFAULT.code());

pub fn current() -> Language {
    Language::from_code(CURRENT.load(Ordering::Relaxed))
}

/// Puts a language in force for the rest of the process. `main` calls it once at start-up
/// with what `load` read; a change made from the tray is saved and the overlay started
/// again, so nothing has to cope with the language moving under it.
pub fn set(language: Language) {
    CURRENT.store(language.code(), Ordering::Relaxed);
}

/// Whichever of the two the language in force calls for.
pub fn pick<T>(japanese: T, english: T) -> T {
    current().pick(japanese, english)
}

/// What is written down in `settings.json`. A struct rather than the bare language, so that
/// another setting can join it later without the file having to be read another way.
#[derive(Default, Deserialize, Serialize)]
struct Settings {
    #[serde(default)]
    language: Language,
}

/// Beside the loadouts, the history and the windows' placements.
fn settings_path() -> Option<PathBuf> {
    ProjectDirs::from("com", "synqark", "WarframePeerOverlay")
        .map(|dirs| dirs.data_local_dir().join("settings.json"))
}

/// The language last chosen, or `Language::DEFAULT` when none has been chosen yet — or when
/// the file cannot be read, which is no reason to refuse to start.
pub fn load() -> Language {
    settings_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|json| serde_json::from_str::<Settings>(&json).ok())
        .unwrap_or_default()
        .language
}

/// Writes the choice down for the next run. Failure is silent: the language still takes
/// effect for this run, and the tray thread has nowhere to report it to.
pub fn save(language: Language) {
    let Some(path) = settings_path() else {
        return;
    };
    if let Some(folder) = path.parent() {
        let _ = fs::create_dir_all(folder);
    }
    if let Ok(json) = serde_json::to_string_pretty(&Settings { language }) {
        let _ = fs::write(path, json);
    }
}

/// Declares a word as `name = (japanese, english)`, as a function giving whichever of the
/// two the language in force calls for.
macro_rules! words {
    ($($(#[$doc:meta])* $name:ident = ($japanese:literal, $english:literal);)*) => {
        $(
            $(#[$doc])*
            pub fn $name() -> &'static str {
                super::pick($japanese, $english)
            }
        )*
    };
}

/// Every word the project writes itself, each in both languages.
pub mod text {
    words! {
        /// The toast a second launch is refused with.
        already_running = (
            "すでに起動しています。終了するにはタスクトレイのアイコンからExitを選択してください。",
            "Already running. To quit, choose Exit from the tray icon."
        );
        /// The toast at start-up, while the game is not up yet and the overlay has no
        /// window to show itself beside.
        waiting_for_warframe = ("Warframeの起動を待機中", "Waiting for Warframe");
        /// The status line before the monitor has reported for the first time.
        starting_up = ("監視を開始しています", "Starting up");
        /// The overlay's panel while the log is watched but the squad is empty.
        waiting_for_peers = ("分隊ピアを待機中", "Waiting for squad peers");
        /// A peer's location line while the lookup is still out.
        resolving_location = ("地域を解決中", "Resolving location");
        /// A peer's location line under `--no-geo`.
        geo_disabled = ("地域取得OFF", "Location lookup off");

        /// The status line, as `monitor::Status` writes it.
        status_waiting_for_warframe = ("Warframeを待機中", "Waiting for Warframe");
        status_log_missing = ("EE.logが見つかりません", "EE.log not found");
        status_monitoring = ("EE.logを監視中", "Monitoring EE.log");

        /// Our own card: the mark beside the name, and the name itself before the log has
        /// said what we are called.
        you = ("自分", "You");
        /// While a player's loadout has not been captured.
        loadout_missing = ("ロードアウト未取得", "Loadout not captured");
        /// On hover over a quest mark.
        quest_done = ("クリア済み", "Completed");
        quest_not_done = ("未クリア", "Not completed");
        /// A place in the grid nobody has taken.
        vacant_place = ("未参加", "Empty");

        /// The session window's headings and notes.
        session_latest = ("最後にロードしたミッション", "Last mission loaded");
        session_no_missions = (
            "まだ SolNode のミッションのロードを見ていません。",
            "No SolNode mission load seen yet."
        );
        session_ties = (
            "分隊メンバーの紐付け（抜けたときに History へ記録）",
            "Squad member ties (written to History when they leave)"
        );
        session_no_members = ("分隊にメンバーはいません。", "No members in the squad.");
        session_recent = (
            "直近のミッション（新しい順）",
            "Recent missions (newest first)"
        );
        session_tie_none = (
            "なし（抜けても記録しない）",
            "None (nothing written on leaving)"
        );
        column_member = ("メンバー", "Member");
        column_loaded = ("ロード (秒)", "Loaded (s)");
        column_role = ("役割", "Role");
        column_node = ("ノード", "Node");
        column_ended = ("終了", "Ended");
        label_loaded = ("ロード", "Loaded");
        label_session_ended = ("セッション終了", "Session ended");
        role_host = ("ホスト", "Host");
        role_client = ("クライアント", "Client");
        /// That a mission's session has not ended yet.
        not_ended = ("未", "No");

        /// The history's list.
        history_empty = (
            "まだ記録がありません。分隊のメンバーが抜けたときに記録します。",
            "Nothing recorded yet. Players are written down as they leave the squad."
        );
        history_no_matches = (
            "条件に合う記録がありません。",
            "No records match the filters."
        );
        clear_filters = ("絞り込み解除", "Clear filters");
        search_by_name = ("名前で検索", "Search by name");
        search_by_node = ("ノードで検索", "Search by node");
        date_from = ("開始日", "From");
        date_to = ("終了日", "To");
        /// Between the two dates a match must fall between.
        date_range = ("〜", "–");
        /// The mission-type menu while no type is picked, and the choice letting it go.
        mission_type_any = ("タイプ", "Type");
        filter_all = ("すべて", "All");

        /// The history's statistics.
        statistic_platforms = ("プラットフォーム", "Platform");
        statistic_countries = ("国", "Country");
        statistic_focus = ("フォーカス", "Focus");
        /// A cell counting nothing.
        no_records = ("記録なし", "No records");
        /// Which of the history the statistics add up.
        scope_label = ("統計表示対象データ：", "Statistics from:");
        scope_all = ("全データ", "All records");
        scope_listed = ("検索データのみ", "Filtered records");

        /// The slots of a loadout, in the order every card shows them.
        slot_warframe = ("フレーム", "Warframe");
        slot_primary = ("プライマリ", "Primary");
        slot_secondary = ("セカンダリ", "Secondary");
        slot_melee = ("近接", "Melee");
        slot_companion = ("コンパニオン", "Companion");
        /// The extras a grid card keeps beside the companion.
        slot_aura = ("オーラ", "Aura");
        drifter = ("漂流者", "Drifter");
        operator = ("オペレーター", "Operator");
    }

    /// The slots of a loadout, in the order every card shows them.
    pub fn gear_slots() -> [&'static str; 5] {
        [
            slot_warframe(),
            slot_primary(),
            slot_secondary(),
            slot_melee(),
            slot_companion(),
        ]
    }

    /// The status line when the log is there but cannot be read.
    pub fn status_read_error(error: &dyn std::fmt::Display) -> String {
        super::pick(
            format!("EE.log読み取りエラー: {error}"),
            format!("EE.log read error: {error}"),
        )
    }

    /// How long after the log's start a mission loaded, and whether we hosted it.
    pub fn loaded_seconds(at: &str, role: &str) -> String {
        super::pick(format!("{at} 秒 ({role})"), format!("{at} s ({role})"))
    }

    /// That a mission's session ended, by what and when.
    pub fn ended_by(by: &str, at: &str) -> String {
        super::pick(format!("済 ({by} {at} 秒)"), format!("Yes ({by} {at} s)"))
    }

    /// How many players the history holds, and how many of them the filters let through.
    pub fn record_count(records: usize) -> String {
        super::pick(format!("{records}件"), format!("{records} records"))
    }

    pub fn record_count_listed(listed: usize, records: usize) -> String {
        super::pick(
            format!("{listed} / {records}件"),
            format!("{listed} / {records} records"),
        )
    }

    /// On the cross beside a date that is set.
    pub fn clear_date(which: &str) -> String {
        super::pick(format!("{which}を外す"), format!("Clear {which}"))
    }

    /// How many players a statistic, or one of its slices, counts.
    pub fn player_count(players: usize) -> String {
        super::pick(format!("{players}人"), format!("{players} players"))
    }

    /// How many countries a ranking has in it, and how many different items.
    pub fn country_count(countries: usize) -> String {
        super::pick(format!("{countries}か国"), format!("{countries} countries"))
    }

    pub fn kind_count(kinds: usize) -> String {
        super::pick(format!("{kinds}種"), format!("{kinds} kinds"))
    }

    /// An item's rank and how many forma have gone into it, on hover.
    pub fn rank(rank: u64) -> String {
        super::pick(format!("ランク {rank}"), format!("Rank {rank}"))
    }

    pub fn forma(forma: u64) -> String {
        super::pick(format!("フォーマ {forma}"), format!("Forma {forma}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_side_the_language_calls_for() {
        assert_eq!(Language::Japanese.pick("あ", "a"), "あ");
        assert_eq!(Language::English.pick("あ", "a"), "a");
    }

    #[test]
    fn opens_in_the_default_language_until_one_is_chosen() {
        // The global is left as it is: the tests share one process, and a language put in
        // force here would follow every other test into its assertions.
        assert_eq!(Language::default(), Language::DEFAULT);
        assert_eq!(current(), Language::DEFAULT);
        assert_eq!(Language::DEFAULT, Language::English);
    }

    #[test]
    fn reads_back_what_was_written_down() {
        let settings = Settings {
            language: Language::English,
        };
        let json = serde_json::to_string(&settings).expect("settings serialize");
        assert_eq!(json, r#"{"language":"english"}"#);
        let read: Settings = serde_json::from_str(&json).expect("settings deserialize");
        assert_eq!(read.language, Language::English);
    }

    #[test]
    fn falls_back_to_the_default_for_a_file_that_says_nothing() {
        let read: Settings = serde_json::from_str("{}").expect("empty settings deserialize");
        assert_eq!(read.language, Language::DEFAULT);
    }
}

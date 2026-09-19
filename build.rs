//! Builds the table of display names the app embeds (see `src/names.rs`) out of the
//! warframe-public-export-plus submodule: one line per named item, sorted by path, as
//! `path \t export \t part type \t name`, the name in `LANGUAGE`.
//!
//! Two kinds of line name something other than an exported item, because a loadout does not
//! name them by an item's path. An aura is named by its dictionary key (`AuraName`), so every
//! aura mod lends its key a line of its own under `Auras`. A focus school is named by a path no
//! export lists (`FocusAbility`, as `/Lotus/Upgrades/Focus/Power/PowerFocusAbility`), so each
//! school the focus export has a folder for gets a line under `FocusSchools`, named as the
//! dictionary names that school's operator ability.
//!
//! Two more name what EE.log gives for the missions the history ties players to (see
//! `src/mission.rs`), by the ids the log uses: every node of the star chart (`SolNode228`)
//! under `Regions`, named with its system as `Plains of Eidolon (Earth)`, and every mission
//! type (`MT_LANDSCAPE`) under `MissionTypes`, as `Free Roam`.
//!
//! Only the exports a loadout draws on are read. Without the submodule checked out the build
//! still succeeds, with a warning and an empty table: items then show their internal paths.

use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, de::DeserializeOwned};

/// The language names come in. English, though the game here runs in Japanese: Japanese names
/// are taller than Latin ones, which would leave the rows of a card standing at heights the
/// cards beside it cannot match. The export ships `dict.<language>.json` for fifteen of them.
const LANGUAGE: &str = "en";
const EXPORT_DIR: &str = "third_party/warframe-public-export-plus";
/// Every export a loadout draws on: warframes (with archwings and necramechs), weapons (with
/// the parts of modular ones), companions, mods, arcanes, cosmetics, glyphs and other flavour,
/// gear, resources (ships among them), railjack armaments, parazon and railjack mods, focus,
/// and operator gear. `src/names.rs` maps each to a `Kind`.
const EXPORTS: [&str; 13] = [
    "Warframes",
    "Weapons",
    "Sentinels",
    "Upgrades",
    "Arcanes",
    "Customs",
    "Flavour",
    "Gear",
    "Resources",
    "RailjackWeapons",
    "Avionics",
    "FocusUpgrades",
    "Virtuals",
];

/// A node of the star chart, in `ExportRegions`.
#[derive(Deserialize)]
struct Region {
    /// Dictionary keys, for the node and the system (planet) it is in.
    name: Option<String>,
    #[serde(rename = "systemName")]
    system_name: Option<String>,
}

#[derive(Deserialize)]
struct ExportEntry {
    /// A key into the language's dictionary.
    name: Option<String>,
    /// Which slot of a modular item a part fills.
    #[serde(rename = "partType")]
    part_type: Option<String>,
    /// What sort of mod an upgrade is: `AURA`, `WARFRAME` and so on.
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Where the focus export keeps each school's upgrades, one folder a school.
const FOCUS_PATH: &str = "/Lotus/Upgrades/Focus/";

fn main() {
    let source =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo")).join(EXPORT_DIR);
    let table = PathBuf::from(env::var_os("OUT_DIR").expect("set by cargo")).join("names.tsv");
    println!("cargo::rerun-if-changed=build.rs");

    let dictionary = source.join(format!("dict.{LANGUAGE}.json"));
    if !dictionary.exists() {
        println!(
            "cargo::warning={EXPORT_DIR} is not checked out (git submodule update --init), so items will show their internal paths"
        );
        // Built again once the submodule turns up.
        println!("cargo::rerun-if-changed={}", source.display());
        fs::write(&table, "").expect("OUT_DIR is writable");
        return;
    }
    println!("cargo::rerun-if-changed={}", dictionary.display());
    let dictionary: HashMap<String, String> = read(&dictionary);

    let mut rows = Vec::new();
    let mut schools = BTreeSet::new();
    for export in EXPORTS {
        let file = source.join(format!("Export{export}.json"));
        println!("cargo::rerun-if-changed={}", file.display());
        let entries: HashMap<String, ExportEntry> = read(&file);
        for (path, entry) in entries {
            if export == "FocusUpgrades"
                && let Some(school) = path
                    .strip_prefix(FOCUS_PATH)
                    .and_then(|rest| rest.split('/').next())
            {
                schools.insert(school.to_owned());
            }
            let Some(key) = entry.name else {
                continue;
            };
            let Some(name) = dictionary
                .get(&key)
                .map(|name| one_line(name))
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            if export == "Upgrades" && entry.kind.as_deref() == Some("AURA") {
                rows.push(format!("{key}\tAuras\t\t{name}"));
            }
            let part = entry.part_type.unwrap_or_default();
            rows.push(format!("{path}\t{export}\t{part}\t{name}"));
        }
    }
    let regions = source.join("ExportRegions.json");
    println!("cargo::rerun-if-changed={}", regions.display());
    let regions: HashMap<String, Region> = read(&regions);
    for (id, region) in regions {
        let name = |key: Option<String>| {
            key.and_then(|key| dictionary.get(&key))
                .map(|name| one_line(name))
                .filter(|name| !name.is_empty())
        };
        let Some(node) = name(region.name) else {
            continue;
        };
        let name = match name(region.system_name) {
            Some(system) => format!("{node} ({system})"),
            None => node,
        };
        rows.push(format!("{id}	Regions		{name}"));
    }
    let mission_types = source.join("ExportMissionTypes.json");
    println!("cargo::rerun-if-changed={}", mission_types.display());
    let mission_types: HashMap<String, ExportEntry> = read(&mission_types);
    for (id, mission_type) in mission_types {
        if let Some(name) = mission_type
            .name
            .and_then(|key| dictionary.get(&key))
            .map(|name| capitalised(&one_line(name)))
            .filter(|name| !name.is_empty())
        {
            rows.push(format!("{id}	MissionTypes		{name}"));
        }
    }
    for school in schools {
        let ability = format!("/Lotus/Language/Items/Operator{school}AbilityName");
        if let Some(name) = dictionary.get(&ability).map(|name| one_line(name)) {
            rows.push(format!(
                "{FOCUS_PATH}{school}/{school}FocusAbility\tFocusSchools\t\t{name}"
            ));
        }
    }
    rows.sort_unstable();
    // An aura mod and its variant (a quest's copy, say) share a key, and so a line.
    rows.dedup();
    fs::write(&table, rows.join("\n")).expect("OUT_DIR is writable");
}

fn read<T: DeserializeOwned>(file: &Path) -> T {
    let bytes =
        fs::read(file).unwrap_or_else(|error| panic!("reading {}: {error}", file.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("parsing {}: {error}", file.display()))
}

/// Some mission types are named in capitals (`FREE ROAM`) and some not (`Alchemy`): the
/// capitals are brought down to how the rest are written.
fn capitalised(name: &str) -> String {
    if name.chars().any(char::is_lowercase) {
        return name.to_owned();
    }
    name.split(' ')
        .map(|word| {
            let mut letters = word.chars();
            letters.next().map_or_else(String::new, |first| {
                first
                    .to_uppercase()
                    .chain(letters.flat_map(char::to_lowercase))
                    .collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A few names hold a line break, but the table keeps each on one line.
fn one_line(name: &str) -> String {
    name.split(['\r', '\n', '\t'])
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

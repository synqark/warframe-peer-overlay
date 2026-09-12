//! Display names for the game's internal paths, as the arsenal shows them.
//!
//! The table comes from the warframe-public-export-plus submodule, which `build.rs` turns into
//! one line per named item (`path \t export \t part type \t name`) for `include_str!`. It
//! covers every export a loadout draws on, in the language `build.rs` picks, and is indexed on
//! first use. Built without the submodule, it is empty and every lookup comes back `None`.

use std::{collections::HashMap, sync::OnceLock};

static TABLE: &str = include_str!(concat!(env!("OUT_DIR"), "/names.tsv"));

/// The parts a modular item goes by: a zaw's strike, a kitgun's chamber, an amp's prism, a
/// MOA's or hound's model, and a K-Drive's board. Their bases have no name of their own.
const NAMING_PARTS: [&str; 6] = [
    "LWPT_BLADE",
    "LWPT_GUN_BARREL",
    "LWPT_AMP_OCULUS",
    "LWPT_MOA_HEAD",
    "LWPT_ZANUKA_HEAD",
    "LWPT_HB_DECK",
];

/// Which export an item comes from. In a `WeaponUpgrades` list, this is what tells the mods
/// from the arcanes and the cosmetics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Warframes, archwings and necramechs.
    Warframe,
    /// Weapons, and the parts modular ones are built from.
    Weapon,
    /// Sentinels and beasts.
    Companion,
    Mod,
    Arcane,
    /// Skins, armour, syandanas and the like.
    Cosmetic,
    /// Glyphs, colour palettes, ship decorations and the like.
    Flavour,
    Gear,
    Resource,
    RailjackWeapon,
    /// Parazon and railjack mods.
    Avionic,
    Focus,
    /// Operator and drifter gear.
    Virtual,
}

impl Kind {
    fn from_export(export: &str) -> Option<Self> {
        Some(match export {
            "Warframes" => Self::Warframe,
            "Weapons" => Self::Weapon,
            "Sentinels" => Self::Companion,
            "Upgrades" => Self::Mod,
            "Arcanes" => Self::Arcane,
            "Customs" => Self::Cosmetic,
            "Flavour" => Self::Flavour,
            "Gear" => Self::Gear,
            "Resources" => Self::Resource,
            "RailjackWeapons" => Self::RailjackWeapon,
            "Avionics" => Self::Avionic,
            "FocusUpgrades" => Self::Focus,
            "Virtuals" => Self::Virtual,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub name: &'static str,
    /// For a part of a modular item, the slot it fills (`LWPT_BLADE` and so on).
    pub part: Option<&'static str>,
}

pub fn lookup(path: &str) -> Option<Entry> {
    table().get(path).copied()
}

/// The name an item goes by, given its `ModularPartTypes`: that of a modular item's defining
/// part, otherwise its own. Beasts bred from parts (antigen and mutagen) keep their own.
pub fn item_name(path: &str, parts: &[String]) -> Option<&'static str> {
    parts
        .iter()
        .filter_map(|part| lookup(part))
        .find(|entry| entry.part.is_some_and(|slot| NAMING_PARTS.contains(&slot)))
        .or_else(|| lookup(path))
        .map(|entry| entry.name)
}

fn table() -> &'static HashMap<&'static str, Entry> {
    static INDEX: OnceLock<HashMap<&'static str, Entry>> = OnceLock::new();
    INDEX.get_or_init(|| TABLE.lines().filter_map(row).collect())
}

fn row(line: &'static str) -> Option<(&'static str, Entry)> {
    let mut fields = line.split('\t');
    let (path, export, part, name) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    let entry = Entry {
        kind: Kind::from_export(export)?,
        name,
        part: (!part.is_empty()).then_some(part),
    };
    Some((path, entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> Entry {
        lookup(path).unwrap_or_else(|| {
            panic!("{path} is not in the table: is third_party/warframe-public-export-plus checked out?")
        })
    }

    fn parts(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn indexes_every_row_of_the_table() {
        let rows = TABLE.lines().count();
        assert!(
            rows > 10_000,
            "only {rows} names: is third_party/warframe-public-export-plus checked out?"
        );
        assert!(
            TABLE.lines().all(|line| row(line).is_some()),
            "every row has four fields and an export `Kind` knows"
        );
        assert_eq!(table().len(), rows, "no path is listed twice");
    }

    #[test]
    fn names_gear_mods_and_cosmetics() {
        assert_eq!(
            entry("/Lotus/Powersuits/Excalibur/Excalibur"),
            Entry {
                kind: Kind::Warframe,
                name: "Excalibur",
                part: None,
            }
        );
        assert_eq!(
            entry("/Lotus/Types/Game/KubrowPet/HunterKubrowPetPowerSuit").name,
            "Sunika Kubrow"
        );

        let serration = entry("/Lotus/Upgrades/Mods/Rifle/WeaponDamageAmountMod");
        assert_eq!((serration.kind, serration.name), (Kind::Mod, "Serration"));
        let merciless = entry("/Lotus/Upgrades/CosmeticEnhancers/Offensive/PrimaryDamageOnKill");
        assert_eq!(
            (merciless.kind, merciless.name),
            (Kind::Arcane, "Primary Merciless")
        );
        assert_eq!(
            entry("/Lotus/Upgrades/Skins/Deluxe/AlchemistDeluxeShotgunSkin").kind,
            Kind::Cosmetic
        );
    }

    #[test]
    fn names_a_modular_item_after_its_defining_part() {
        let zaw = parts(&[
            "/Lotus/Weapons/Ostron/Melee/ModularMelee01/Balance/BalanceSpeedIICritI",
            "/Lotus/Weapons/Ostron/Melee/ModularMelee02/Handle/HandleNine",
            "/Lotus/Weapons/Ostron/Melee/ModularMelee02/Tip/TipEleven",
        ]);
        assert_eq!(
            item_name("/Lotus/Weapons/Ostron/Melee/LotusModularWeapon", &zaw),
            Some("Dokrahm")
        );

        let kitgun = parts(&[
            "/Lotus/Weapons/SolarisUnited/Secondary/SUModularSecondarySet1/Barrel/SUModularSecondaryBarrelAPart",
            "/Lotus/Weapons/SolarisUnited/Secondary/SUModularSecondarySet1/Handle/SUModularSecondaryHandleCPart",
        ]);
        assert_eq!(
            item_name(
                "/Lotus/Weapons/SolarisUnited/Secondary/LotusModularSecondaryShotgun",
                &kitgun
            ),
            Some("Catchmoon")
        );

        // Antigen and mutagen breed the beast but do not name it.
        let vulpaphyla =
            "/Lotus/Types/Friendly/Pets/CreaturePets/ArmoredInfestedCatbrowPetPowerSuit";
        let bred = parts(&[
            "/Lotus/Types/Friendly/Pets/CreaturePets/CreaturePetParts/Deimos/InfestedCritterAntigenC",
            "/Lotus/Types/Friendly/Pets/CreaturePets/CreaturePetParts/Deimos/InfestedCritterMutagenD",
        ]);
        assert_eq!(item_name(vulpaphyla, &bred), Some(entry(vulpaphyla).name));
    }

    #[test]
    fn leaves_what_the_export_lacks_unnamed() {
        assert_eq!(lookup("/Example/Nothing"), None);
        assert_eq!(
            item_name("/Example/Nothing", &parts(&["/Example/Part"])),
            None
        );
    }
}

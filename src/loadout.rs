//! Mission loadouts, squad members' and our own, read out of the game's memory.
//!
//! A loadout lives in the game's heap as a NUL-terminated JSON string starting with
//! `{"PlayerLevel":`: warframe and weapons with their mods, forma and levels, companion,
//! archwing, operator, gear and focus. One pass over the game's memory collects every such
//! string, counting identical copies, and serves three kinds of job:
//!
//! - **Members.** When a player joins the squad, EE.log announces
//!   `ProcessSquadMessage received JOIN message from <name>, loadout: <N> bytes`, and their
//!   loadout is the string of exactly N bytes.
//! - **Members' changes.** A member who changes their gear sends it again
//!   (`HandleSquadMessage from <address> LOADOUT`), naming no size. The new version is found
//!   through the game's own records, which keep a player's name just before the address of
//!   their loadout: the version their records have moved to is theirs.
//! - **Ourselves.** Every `BuildLoadOut for <local player>` (each change in the arsenal, leaving
//!   it, loading into a mission) rebuilds our loadout as a new string within the same second.
//!   A version the previous pass did not meet is the one just built; without one, a version
//!   copied about since is (the arsenal copies ours dozens of times over); without either,
//!   ours has not changed. Copies alone cannot tell: an old loadout can linger in more copies
//!   than the current one for as long as the game runs. Members' versions never count.
//!
//! Besides being saved, every loadout found is handed back to the monitor, summed up as a
//! [`Loadout`], for the loadout windows. Ours starts out as the copy last saved, until the game
//! shows it again.
//!
//! What is saved is kept only as long as something can show it. Of our own, that is the latest
//! and nothing else: an earlier copy is of no use to anything. Of a member's, it is as long as
//! the history still has them — [`discard_unkept`] throws the rest away.
//!
//! Capturing by size, and our own loadout, depend on no struct layouts or pointers, so a game
//! update that moves things around in memory does not break them. Following a member's changes
//! reads the game's records, but assumes no offsets in them beyond how far before a loadout's
//! address the name may lie; should that stop holding, changes simply stop showing. Access is
//! read-only
//! (`PROCESS_VM_READ | PROCESS_QUERY_INFORMATION`): nothing is written, injected or hooked. A
//! pass takes a second or two, so captures run on a worker thread of their own and the monitor
//! loop only hands it jobs.

use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    fmt, fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{
        Diagnostics::Debug::ReadProcessMemory,
        Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_GUARD,
            PAGE_NOACCESS, PAGE_WRITECOMBINE, VirtualQueryEx,
        },
        Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
    },
};

use crate::parser::Platform;

const LOADOUT_HEAD: &[u8] = br#"{"PlayerLevel":"#;
/// No loadout comes near this; it only bounds the search for a string's terminator.
const MAX_LOADOUT: usize = 1024 * 1024;
/// Read granularity. Chunks overlap by the head's length so no head is split unseen.
const CHUNK: usize = 32 * 1024 * 1024;
/// The unit memory is committed and freed in, and so what a chunk that failed to read is read
/// again by (`Process::read_pages`).
const PAGE: usize = 4096;
/// Anything larger is graphics or mapped memory, never the heap strings wanted here.
const MAX_REGION: usize = 512 * 1024 * 1024;
/// How often work that found nothing is retried, and for how long a member is looked for.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);
/// Passes spent on one rebuild of our own loadout before settling for a stand-in for it.
const OWN_PASSES: u32 = 2;
/// Passes spent looking for a member's changed loadout before giving up on that message.
const UPDATE_PASSES: u32 = 2;
/// How far before a loadout's address a record keeps the name of the player it belongs to.
/// The game's record keeps it 88 bytes before; the reach leaves it room to move.
const OWNER_REACH: usize = 160;
/// The longest player name, platform mark included, a record is taken to hold.
const MAX_NAME: usize = 64;

/// One member's loadout, announced by EE.log as `bytes` long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRequest {
    pub pid: u32,
    pub name: String,
    pub platform: Platform,
    pub bytes: usize,
}

/// A member's loadout, sent to the squad again since they joined (`HandleSquadMessage from
/// <address> LOADOUT`): their arsenal changed. The line gives no size, so the new version is
/// found through the records that tie a loadout to its player's name (`record_owner`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateRequest {
    pub pid: u32,
    pub name: String,
    pub platform: Platform,
}

/// Our own loadout, just rebuilt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnRequest {
    pub pid: u32,
    pub name: String,
    pub platform: Platform,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    Member(CaptureRequest),
    Update(UpdateRequest),
    Own(OwnRequest),
    /// Somebody else's loadout was just rebuilt: take note of what is in memory now, so that
    /// their version is never mistaken for a freshly built one of ours.
    Others {
        pid: u32,
    },
}

/// A loadout the worker found, handed back to the monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Captured {
    /// A member's, found for the JOIN that announced it as `bytes` long.
    Member {
        name: String,
        bytes: usize,
        loadout: Loadout,
        json: RawJson,
        /// What it was saved as, so that the history can name it and a clear-out spare it.
        file: Option<String>,
    },
    /// A member's, changed since they joined. It stands for them in place of the one their
    /// JOIN announced, until they leave or join again.
    Update {
        name: String,
        loadout: Loadout,
        json: RawJson,
        file: Option<String>,
    },
    /// Our own, as last rebuilt.
    Own {
        name: String,
        platform: String,
        loadout: Loadout,
        json: RawJson,
    },
}

/// A loadout as the game had it, kept whole so a window can hand it out entire — everything
/// the summary leaves behind included. `Debug` gives a hash of it rather than its content:
/// the monitor's snapshot signature would otherwise carry tens of kilobytes per player,
/// formatted afresh twice a second.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RawJson(Arc<str>);

impl RawJson {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Over several lines, the way a file of it reads; the game keeps it all on one.
    pub fn pretty(&self) -> String {
        serde_json::from_str::<Value>(&self.0)
            .and_then(|loadout| serde_json::to_string_pretty(&loadout))
            .unwrap_or_else(|_| self.0.to_string())
    }
}

impl From<&[u8]> for RawJson {
    fn from(json: &[u8]) -> Self {
        Self(String::from_utf8_lossy(json).into())
    }
}

impl fmt::Debug for RawJson {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "RawJson({:x})", content_hash(self.0.as_bytes()))
    }
}

/// What the loadout window shows of a loadout. Anything missing, or shaped otherwise than
/// expected after a game update, is simply left out.
///
/// The history keeps these, so a field added later must read as its default from an entry
/// written down before it existed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Loadout {
    /// `PlayerLevel`.
    pub mastery_rank: Option<u64>,
    /// Whether the player is past The New War (`PostNewWar`) and The Old Peace
    /// (`PostOldPeace`) quests.
    pub post_new_war: Option<bool>,
    pub post_old_peace: Option<bool>,
    pub warframe: Option<Item>,
    pub primary: Option<Item>,
    pub secondary: Option<Item>,
    pub melee: Option<Item>,
    /// The companion itself, whichever kind it is.
    pub companion: Option<Item>,
    /// The name the player gave their companion (`KubrowName`).
    pub companion_name: Option<String>,
    /// The auras on the warframe, as the dictionary keys their names go by: `AuraName`, then
    /// `ExtraAuraName` for a warframe with a second aura slot, the empty ones left out.
    pub auras: Vec<String>,
    /// Whoever stands behind the warframe, when the loadout brings one.
    pub operator: Option<Operator>,
}

/// The operator (`OPERATOR`) or the drifter (`OPERATOR_ADULT`) a loadout brings, and the
/// focus school they have on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operator {
    /// The drifter rather than the operator.
    pub drifter: bool,
    /// The focus school's path (`FocusAbility`).
    pub focus: Option<String>,
}

/// One equipped item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The game's internal path (`ItemType`), not a display name (see `names`).
    pub path: String,
    /// `Level`.
    pub rank: Option<u64>,
    /// Forma used on it (`Polarized`); absent when there are none.
    pub forma: Option<u64>,
    /// The parts a modular item is built from (`ModularPartTypes`), as internal paths; empty
    /// for any other item.
    pub parts: Vec<String>,
    /// What is installed on it (`WeaponUpgrades`), as internal paths in the order the game
    /// lists them, with the empty slots left out. Cosmetics, mods and arcanes share the list;
    /// `names::Kind` tells them apart.
    pub upgrades: Vec<String>,
}

/// `NORMAL` slots, as observed: warframe, secondary, primary, melee, then the archgun and
/// exalted weapons, with `{}` for an empty one. `SENTINEL[0]` holds the companion; its weapon
/// comes further along.
const WARFRAME_SLOT: usize = 0;
const SECONDARY_SLOT: usize = 1;
const PRIMARY_SLOT: usize = 2;
const MELEE_SLOT: usize = 3;
const COMPANION_SLOT: usize = 0;

impl Loadout {
    pub fn from_json(loadout: &Value) -> Self {
        let item = |group: &str, slot: usize| Item::from_json(&loadout[group][slot]);
        Self {
            mastery_rank: loadout["PlayerLevel"].as_u64(),
            post_new_war: loadout["PostNewWar"].as_bool(),
            post_old_peace: loadout["PostOldPeace"].as_bool(),
            warframe: item("NORMAL", WARFRAME_SLOT),
            primary: item("NORMAL", PRIMARY_SLOT),
            secondary: item("NORMAL", SECONDARY_SLOT),
            melee: item("NORMAL", MELEE_SLOT),
            companion: item("SENTINEL", COMPANION_SLOT),
            companion_name: text(&loadout["KubrowName"]),
            auras: ["AuraName", "ExtraAuraName"]
                .into_iter()
                .filter_map(|key| text(&loadout[key]))
                .collect(),
            operator: Operator::from_json(loadout),
        }
    }
}

impl Operator {
    fn from_json(loadout: &Value) -> Option<Self> {
        // Where a loadout has both, the drifter's is the later of the two.
        let drifter = loadout.get("OPERATOR_ADULT").is_some();
        (drifter || loadout.get("OPERATOR").is_some()).then(|| Self {
            drifter,
            focus: text(&loadout["FocusAbility"]),
        })
    }
}

/// A string that says something: an empty one is as good as none.
fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

impl Item {
    fn from_json(entry: &Value) -> Option<Self> {
        let path = entry["ItemType"].as_str().filter(|path| !path.is_empty())?;
        let parts = entry["ModularPartTypes"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let upgrades = entry["WeaponUpgrades"]
            .as_array()
            .map(|upgrades| {
                upgrades
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|path| !path.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            path: path.to_owned(),
            rank: entry["Level"].as_u64(),
            forma: entry["Polarized"].as_u64(),
            parts,
            upgrades,
        })
    }
}

/// Starts the worker: it takes jobs, and hands back every loadout it finds.
pub fn spawn() -> (Sender<Job>, Receiver<Captured>) {
    let (jobs, job_receiver) = mpsc::channel();
    let (captured_sender, captured) = mpsc::channel();
    thread::spawn(move || run(job_receiver, captured_sender));
    (jobs, captured)
}

/// One distinct loadout string, and how many copies of it a pass met.
struct Found {
    json: Vec<u8>,
    copies: usize,
    /// Where each copy lies, for finding what points at them (`referrers`).
    addresses: Vec<usize>,
}

/// What the worker knows of one squad member's loadout.
struct MemberState {
    /// The version last taken for theirs.
    version: u64,
    /// Every record that named them, by where it lies, with the version it pointed at when
    /// last looked at.
    records: HashMap<usize, u64>,
}

/// What the worker remembers between passes.
#[derive(Default)]
struct History {
    /// Every version the previous pass met, with its copies, so that one built since stands
    /// out, and so does one copied about since. `None` until a pass has run.
    previous: Option<HashMap<u64, usize>>,
    /// Versions saved for squad members; never taken for our own.
    members: HashSet<u64>,
    /// Each squad member's loadout as last taken, by name, with the records naming them.
    member_states: HashMap<String, MemberState>,
    /// The version last saved as our own.
    own: Option<u64>,
    /// Our own loadout as `self_latest.json` has it, looked for in memory until some version
    /// has been taken for ours (`recognise_saved_own`).
    saved_own: Option<Value>,
    /// The mastery rank of the version last taken for ours. Rank never drops, so anything
    /// under it belongs to somebody else. `pick_own` reaches past the floor when nothing else
    /// clears it, so a wrong capture cannot lock ours out for good.
    own_mastery: Option<u64>,
}

fn run(receiver: Receiver<Job>, captured: Sender<Captured>) {
    let directory = loadout_directory();
    let mut history = History::default();
    // Until the game shows ours again, the copy last saved stands in for it, and tells us the
    // mastery rank ours cannot be below.
    if let Some(own) = directory.as_deref().and_then(latest_own) {
        if let Captured::Own { loadout, json, .. } = &own {
            history.own_mastery = loadout.mastery_rank;
            history.saved_own = serde_json::from_str(&json.0).ok();
        }
        let _ = captured.send(own);
    }
    let mut members: Vec<(CaptureRequest, Instant)> = Vec::new();
    let mut updates: Vec<(UpdateRequest, u32)> = Vec::new();
    let mut own: Option<(OwnRequest, u32)> = None;
    // A pass asked for to take note of what is in memory, after somebody else's rebuild.
    let mut note: Option<u32> = None;
    loop {
        // Idle until asked; with work outstanding, wake up again to retry it.
        let received = if members.is_empty() && updates.is_empty() && own.is_none() {
            receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(RETRY_INTERVAL)
        };
        let mut jobs = match received {
            Ok(job) => vec![job],
            Err(RecvTimeoutError::Timeout) => Vec::new(),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Jobs arriving together share one pass over the game's memory.
        jobs.extend(receiver.try_iter());
        for job in jobs {
            match job {
                Job::Member(request) => members.push((request, Instant::now())),
                // A later message from the same member starts the looking afresh.
                Job::Update(request) => {
                    updates.retain(|(pending, _)| pending.name != request.name);
                    updates.push((request, 0));
                }
                // A newer rebuild supersedes one still being looked for.
                Job::Own(request) => own = Some((request, 0)),
                Job::Others { pid } => note = Some(pid),
            }
        }

        let mut pids: Vec<u32> = members
            .iter()
            .map(|(request, _)| request.pid)
            .chain(updates.iter().map(|(request, _)| request.pid))
            .chain(own.iter().map(|(request, _)| request.pid))
            .chain(note)
            .collect();
        pids.sort_unstable();
        pids.dedup();
        for pid in pids {
            let process = Process::open(pid);
            let found = process.as_ref().map(census).unwrap_or_default();
            // Members first, so a version just given to one is never taken for our own.
            let mut joined = Vec::new();
            members.retain(|(request, since)| {
                if request.pid != pid {
                    return true;
                }
                if let Some(hash) = pick_member(&found, request.bytes, &history.members) {
                    history.members.insert(hash);
                    joined.push((request.name.clone(), hash));
                    let json = &found[&hash].json;
                    let file = directory.as_deref().and_then(|directory| {
                        save_member(directory, &request.name, request.platform, json)
                    });
                    if let Some(loadout) = summarize(json) {
                        let _ = captured.send(Captured::Member {
                            name: request.name.clone(),
                            bytes: request.bytes,
                            loadout,
                            json: RawJson::from(json.as_slice()),
                            file,
                        });
                    }
                    return false;
                }
                since.elapsed() < CAPTURE_TIMEOUT
            });

            // Records are looked through only when there is somebody to look for: a member
            // just captured, whose records are where their changes will show, or a member
            // whose loadout changed.
            let looking =
                !joined.is_empty() || updates.iter().any(|(request, _)| request.pid == pid);
            let records = process.as_ref().filter(|_| looking).map(|process| {
                referrers(process, &found)
                    .into_iter()
                    .filter_map(|(place, version)| {
                        Some((record_owner(process, place)?, place, version))
                    })
                    .collect::<Vec<_>>()
            });
            let records_of = |name: &str| {
                records
                    .as_deref()
                    .map_or_else(HashMap::new, |records| records_naming(records, name))
            };
            for (name, version) in joined {
                // A change already on its way is weighed against no records at all: the
                // JOIN it follows may be long gone, as when the log is replayed at start.
                let records = if updates.iter().any(|(request, _)| request.name == name) {
                    HashMap::new()
                } else {
                    records_of(&name)
                };
                history
                    .member_states
                    .insert(name, MemberState { version, records });
            }
            let mut updated = Vec::new();
            updates.retain_mut(|(request, passes)| {
                if request.pid != pid {
                    return true;
                }
                *passes += 1;
                let records = records_of(&request.name);
                let state = history.member_states.get(&request.name);
                let before = state.map(|state| &state.records);
                let picked = pick_update(&records, before.unwrap_or(&HashMap::new()), &found);
                let current = state.map(|state| state.version);
                if let Some(version) = picked
                    && Some(version) != current
                {
                    history.members.insert(version);
                    updated.push(request.name.clone());
                    let json = &found[&version].json;
                    let file = directory.as_deref().and_then(|directory| {
                        save_member(directory, &request.name, request.platform, json)
                    });
                    if let Some(loadout) = summarize(json) {
                        let _ = captured.send(Captured::Update {
                            name: request.name.clone(),
                            loadout,
                            json: RawJson::from(json.as_slice()),
                            file,
                        });
                    }
                }
                if let Some(version) = picked.or(current) {
                    history
                        .member_states
                        .insert(request.name.clone(), MemberState { version, records });
                }
                // Nothing moved yet: look once more in case the change is still arriving.
                picked.is_none() && *passes < UPDATE_PASSES
            });
            // A change found is newer than the JOIN a member might still be looked for by.
            members.retain(|(request, _)| !updated.contains(&request.name));
            let own_done = match &mut own {
                Some((request, passes)) if request.pid == pid => {
                    *passes += 1;
                    recognise_saved_own(&found, &mut history);
                    let pick = pick_own(&found, &history);
                    if let Some((hash, _)) = pick
                        && history.own != Some(hash)
                    {
                        history.own = Some(hash);
                        let json = &found[&hash].json;
                        history.own_mastery = mastery_rank(json).or(history.own_mastery);
                        if let Some(directory) = &directory {
                            save_own(directory, request, json);
                        }
                        if let Some(loadout) = summarize(json) {
                            let _ = captured.send(Captured::Own {
                                name: request.name.clone(),
                                platform: request.platform.label().to_owned(),
                                loadout,
                                json: RawJson::from(json.as_slice()),
                            });
                        }
                    }
                    // A version built or copied about since the previous pass is the rebuild
                    // itself; without one, look once more in case it was still being written.
                    pick.is_some_and(|(_, reason)| reason.is_the_rebuild()) || *passes >= OWN_PASSES
                }
                _ => false,
            };
            if own_done {
                own = None;
            }
            history.previous = Some(
                found
                    .iter()
                    .map(|(hash, entry)| (*hash, entry.copies))
                    .collect(),
            );
        }
        // Whatever a note asked about has been taken in by the pass above.
        note = None;
    }
}

/// Every distinct loadout string in the game's memory, keyed by content hash.
fn census(process: &Process) -> HashMap<u64, Found> {
    let mut found = HashMap::new();
    for (base, size) in process.regions() {
        let mut offset = 0;
        while offset < size {
            let want = CHUNK.min(size - offset);
            let last = offset + want >= size;
            let chunk_base = base + offset;
            if let Some(data) = process.read_pages(chunk_base, want) {
                // A head in the overlap is met again at the start of the next chunk.
                let accept_before = if last {
                    data.len()
                } else {
                    CHUNK - LOADOUT_HEAD.len()
                };
                collect(
                    &data,
                    chunk_base,
                    accept_before,
                    |start| process.read(chunk_base + start, MAX_LOADOUT),
                    &mut found,
                );
            }
            if last {
                break;
            }
            offset += CHUNK - LOADOUT_HEAD.len();
        }
    }
    found
}

/// Adds every complete loadout string in `data` whose head starts before `accept_before`,
/// counting identical copies and noting where each lies (`data` starts at `base`). One that
/// runs past the end of the chunk is fetched with `read_past(start)` instead. A string counts
/// only if it ends in `}` right before its NUL terminator and parses as JSON.
fn collect(
    data: &[u8],
    base: usize,
    accept_before: usize,
    mut read_past: impl FnMut(usize) -> Option<Vec<u8>>,
    found: &mut HashMap<u64, Found>,
) {
    for start in memchr::memmem::find_iter(data, LOADOUT_HEAD) {
        if start >= accept_before {
            break;
        }
        let rest = &data[start..];
        let fetched;
        let json = match memchr::memchr(0, &rest[..rest.len().min(MAX_LOADOUT)]) {
            Some(end) => &rest[..end],
            None if rest.len() < MAX_LOADOUT => {
                fetched = read_past(start);
                let Some(window) = fetched.as_deref() else {
                    continue;
                };
                let Some(end) = memchr::memchr(0, window) else {
                    continue;
                };
                &window[..end]
            }
            None => continue,
        };
        if json.last() != Some(&b'}') {
            continue;
        }
        let hash = content_hash(json);
        if let Some(entry) = found.get_mut(&hash) {
            entry.copies += 1;
            entry.addresses.push(base + start);
        } else if serde_json::from_slice::<serde::de::IgnoredAny>(json).is_ok() {
            found.insert(
                hash,
                Found {
                    json: json.to_vec(),
                    copies: 1,
                    addresses: vec![base + start],
                },
            );
        }
    }
}

/// Every place in the game's memory that holds the address of a loadout string, with the
/// version that string is. The records tying a loadout to its player are among them.
fn referrers(process: &Process, found: &HashMap<u64, Found>) -> Vec<(usize, u64)> {
    let mut strings = found
        .iter()
        .flat_map(|(hash, entry)| entry.addresses.iter().map(move |address| (*address, *hash)))
        .collect::<Vec<_>>();
    strings.sort_unstable();
    let (Some(&(low, _)), Some(&(high, _))) = (strings.first(), strings.last()) else {
        return Vec::new();
    };
    let mut places = Vec::new();
    for (base, size) in process.regions() {
        let mut offset = 0;
        while offset < size {
            let want = CHUNK.min(size - offset);
            if let Some(data) = process.read_pages(base + offset, want) {
                // Regions start on a page, so every eighth byte starts an aligned word.
                for (index, word) in data.as_chunks::<8>().0.iter().enumerate() {
                    let value = usize::from_le_bytes(*word);
                    if (low..=high).contains(&value)
                        && let Ok(at) =
                            strings.binary_search_by_key(&value, |(address, _)| *address)
                    {
                        places.push((base + offset + index * 8, strings[at].1));
                    }
                }
            }
            offset += want;
        }
    }
    places
}

/// The player a record holding a loadout's address at `place` belongs to.
fn record_owner(process: &Process, place: usize) -> Option<String> {
    // One word more than the reach, to tell whether a name at its far end starts there.
    let len = OWNER_REACH + 8;
    let window = process.read(place.checked_sub(len)?, len)?;
    (window.len() == len)
        .then(|| nearest_name(&window, |address, len| process.read(address, len)))
        .flatten()
}

/// The player name a record keeps nearest the end of `window`, the stretch of it just before a
/// loadout's address; its first word is only there to be looked back at. A short name is kept
/// in place; a longer one as an address followed by a length word, whose top four bits are set,
/// read with `read`. Names end in the platform mark, which is what tells them from the rest of
/// a record, and the nearest is taken so that a record in a row of them is not put to the
/// player of the record before it.
fn nearest_name(window: &[u8], read: impl Fn(usize, usize) -> Option<Vec<u8>>) -> Option<String> {
    let words = window.len() / 8;
    let word = |index: usize| {
        u64::from_le_bytes(
            window[index * 8..index * 8 + 8]
                .try_into()
                .expect("eight bytes"),
        )
    };
    for index in (1..words).rev() {
        // In place, running from this word to its NUL — unless the word before is plain text
        // too, in which case this is the tail of a name that starts earlier. Only plain text
        // counts: a number that happens to have no small bytes in it is no part of a name.
        let rest = &window[index * 8..];
        let text = &rest[..memchr::memchr(0, rest).unwrap_or(rest.len())];
        let continued = window[index * 8 - 8..index * 8]
            .iter()
            .all(|b| (0x20..=0x7e).contains(b));
        if !continued && let Some(name) = player_name(text) {
            return Some(name.to_owned());
        }
        // Elsewhere, by address and length.
        if index + 1 < words {
            let length = word(index + 1) as u32;
            let len = (length & 0x0fff_ffff) as usize;
            if length & 0xf000_0000 == 0xf000_0000
                && (4..=MAX_NAME).contains(&len)
                && let Some(bytes) = read(word(index) as usize, len)
                && let Some(name) = player_name(&bytes)
            {
                return Some(name.to_owned());
            }
        }
    }
    None
}

/// The records of `name` among `records` (owner, where it lies, the version it points at), by
/// where they lie — on versions most of whose named records name them, and no others. The game
/// fills a record in while it may be being read, and one caught holding a player's name and
/// somebody else's loadout would otherwise hand that loadout over; the loadout's own records,
/// of which there are far more, outvote it.
fn records_naming(records: &[(String, usize, u64)], name: &str) -> HashMap<usize, u64> {
    // For each version: how many records name `name`, and how many name anybody.
    let mut claims = HashMap::<u64, (usize, usize)>::new();
    for (owner, _, version) in records {
        let claim = claims.entry(*version).or_default();
        claim.1 += 1;
        if owner == name {
            claim.0 += 1;
        }
    }
    records
        .iter()
        .filter(|(owner, _, version)| {
            let (theirs, everybody) = claims[version];
            owner == name && theirs * 2 > everybody
        })
        .map(|(_, place, version)| (*place, *version))
        .collect()
}

/// The name in `bytes`, when they hold a player name as the game keeps one: the name, then the
/// mark of the platform it plays on, a character from the private use area (`\u{e000}` for
/// PC and so on, as `parser` reads them).
fn player_name(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mark = text.chars().last()?;
    let name = text.strip_suffix(mark)?;
    let fits = ('\u{e000}'..='\u{f8ff}').contains(&mark)
        && !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .chars()
            .all(|c| !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&c));
    fits.then_some(name)
}

/// Which version a member's records say is theirs now. The game keeps one record per player
/// that it updates in place, makes a new one for every loadout a player sends, and leaves old
/// records pointing at old versions; so the versions that count are those pointed at by
/// records that point somewhere else than `before` has them, or were not there then. The one
/// most of those point at wins, the most-copied where they tie. `None` when nothing moved.
fn pick_update(
    records: &HashMap<usize, u64>,
    before: &HashMap<usize, u64>,
    found: &HashMap<u64, Found>,
) -> Option<u64> {
    let mut votes = HashMap::<u64, usize>::new();
    for (place, version) in records {
        if before.get(place) != Some(version) {
            *votes.entry(*version).or_default() += 1;
        }
    }
    votes
        .into_iter()
        .max_by_key(|&(version, votes)| {
            let copies = found.get(&version).map_or(0, |entry| entry.copies);
            (votes, copies, version)
        })
        .map(|(version, _)| version)
}

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// A member's loadout: the string of exactly the announced length not already given to anyone.
fn pick_member(found: &HashMap<u64, Found>, bytes: usize, taken: &HashSet<u64>) -> Option<u64> {
    found
        .iter()
        .find(|(hash, entry)| entry.json.len() == bytes && !taken.contains(*hash))
        .map(|(hash, _)| *hash)
}

/// How `pick_own` came to the version it took for ours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    /// Not in memory at the previous pass: the rebuild itself. That includes a version met
    /// long before, freed, and built again, as when a weapon is changed back.
    Built,
    /// In memory at the previous pass, but in more copies now: an unchanged loadout rebuilt
    /// and copied about, as the arsenal copies ours dozens of times over on the way in.
    Copied,
    /// Nothing built or copied about: the version already taken for ours, still in memory.
    /// Leaving the arsenal rebuilds ours unchanged and lets its copies go, while an old
    /// loadout can linger in more copies than the current one for as long as the game runs.
    Kept,
    /// Nothing to go on, as on the first pass: the most-copied version.
    MostCopied,
}

impl Reason {
    /// Whether the version is the rebuild a job asked about, rather than a stand-in for it.
    fn is_the_rebuild(self) -> bool {
        matches!(self, Self::Built | Self::Copied)
    }
}

/// Our own current loadout, and how it was told (see `Reason`, in the order tried). Versions
/// saved for members, and any ranked below us, are never ours — unless nothing clears that
/// floor, in which case it was not ours that set it, and every version is tried again to let
/// one set the floor anew.
fn pick_own(found: &HashMap<u64, Found>, history: &History) -> Option<(u64, Reason)> {
    pick_own_among(found, history, true).or_else(|| pick_own_among(found, history, false))
}

fn pick_own_among(
    found: &HashMap<u64, Found>,
    history: &History,
    by_rank: bool,
) -> Option<(u64, Reason)> {
    let candidates = found
        .iter()
        .filter(|(hash, entry)| {
            !history.members.contains(*hash)
                && (!by_rank || ours_by_rank(&entry.json, history.own_mastery))
        })
        .map(|(hash, entry)| (*hash, entry.copies))
        .collect::<Vec<_>>();
    // The one ahead by `key`, and by copies where two are level.
    let ahead = |versions: &mut dyn Iterator<Item = (u64, usize, usize)>| {
        versions
            .max_by_key(|&(_, key, copies)| (key, copies))
            .map(|(hash, _, _)| hash)
    };

    if let Some(previous) = &history.previous {
        let built = ahead(
            &mut candidates
                .iter()
                .filter(|(hash, _)| !previous.contains_key(hash))
                .map(|&(hash, copies)| (hash, copies, copies)),
        );
        if let Some(hash) = built {
            return Some((hash, Reason::Built));
        }
        let copied = ahead(&mut candidates.iter().filter_map(|&(hash, copies)| {
            let grown = copies
                .checked_sub(*previous.get(&hash)?)
                .filter(|&grown| grown > 0)?;
            Some((hash, grown, copies))
        }));
        if let Some(hash) = copied {
            return Some((hash, Reason::Copied));
        }
    }
    if let Some(own) = history.own
        && candidates.iter().any(|&(hash, _)| hash == own)
    {
        return Some((own, Reason::Kept));
    }
    ahead(
        &mut candidates
            .iter()
            .map(|&(hash, copies)| (hash, copies, copies)),
    )
    .map(|hash| (hash, Reason::MostCopied))
}

/// Until some version has been taken for ours, looks for the one `self_latest.json` holds and
/// takes that: a restart carries on from the loadout we had, rather than from whichever version
/// lingers in the most copies. The saved copy was laid out afresh when it was written, so it is
/// compared as JSON rather than byte for byte.
fn recognise_saved_own(found: &HashMap<u64, Found>, history: &mut History) {
    if history.own.is_some() {
        history.saved_own = None;
        return;
    }
    let Some(saved) = &history.saved_own else {
        return;
    };
    let recognised = found
        .iter()
        .find(|(_, entry)| {
            serde_json::from_slice::<Value>(&entry.json).is_ok_and(|json| &json == saved)
        })
        .map(|(hash, _)| *hash);
    if recognised.is_some() {
        history.own = recognised;
        history.saved_own = None;
    }
}

/// Whether a version's mastery rank leaves it ours to claim. A rank that cannot be read leaves
/// the version in: only a changed loadout format could hide it, and then nothing would match.
fn ours_by_rank(json: &[u8], floor: Option<u64>) -> bool {
    match (mastery_rank(json), floor) {
        (Some(rank), Some(floor)) => rank >= floor,
        _ => true,
    }
}

/// The `PlayerLevel` a loadout opens with: `LOADOUT_HEAD` ends right before the rank.
fn mastery_rank(json: &[u8]) -> Option<u64> {
    let digits = json.get(LOADOUT_HEAD.len()..)?;
    let end = digits.iter().position(|byte| !byte.is_ascii_digit())?;
    std::str::from_utf8(digits.get(..end)?).ok()?.parse().ok()
}

/// A read-only handle to another process, closed on drop.
struct Process(HANDLE);

impl Process {
    fn open(pid: u32) -> Option<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
        (!handle.is_null()).then_some(Self(handle))
    }

    /// `(base, size)` of every region worth scanning, in address order.
    fn regions(&self) -> Vec<(usize, usize)> {
        let mut regions = Vec::new();
        let mut address = 0x10000_usize;
        loop {
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let written = unsafe {
                VirtualQueryEx(
                    self.0,
                    address as *const c_void,
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            let base = info.BaseAddress as usize;
            let end = base.saturating_add(info.RegionSize);
            // Nothing more to query, or a region that would not advance the walk.
            if written == 0 || end <= address {
                return regions;
            }
            address = end;
            if is_scannable(info.State, info.Protect, info.RegionSize) {
                regions.push((base, info.RegionSize));
            }
        }
    }

    fn read(&self, address: usize, len: usize) -> Option<Vec<u8>> {
        let mut buffer = vec![0_u8; len];
        let mut read = 0;
        let ok = unsafe {
            ReadProcessMemory(
                self.0,
                address as *const c_void,
                buffer.as_mut_ptr().cast(),
                len,
                &mut read,
            )
        };
        if ok == 0 || read == 0 {
            return None;
        }
        buffer.truncate(read);
        Some(buffer)
    }

    /// Reads a chunk of a region as `read` does, standing up to the region having changed
    /// since it was listed. The target frees and commits pages as it runs, and a read that
    /// meets a single page gone fails as a whole (`ERROR_PARTIAL_COPY`, nothing copied), which
    /// would cost everything else in the chunk. So a chunk that fails is read again a page at
    /// a time, the pages that are gone left as zeros: offsets stay where they were, and a zero
    /// ends whatever string runs into one. `None` only when no page of it can be read.
    fn read_pages(&self, address: usize, len: usize) -> Option<Vec<u8>> {
        if let Some(data) = self.read(address, len) {
            return Some(data);
        }
        let mut data = vec![0_u8; len];
        let mut any = false;
        let mut offset = 0;
        while offset < len {
            // Up to the next page boundary: a chunk need not start on one.
            let page = (PAGE - (address + offset) % PAGE).min(len - offset);
            if let Some(bytes) = self.read(address + offset, page) {
                data[offset..offset + bytes.len()].copy_from_slice(&bytes);
                any = true;
            }
            offset += page;
        }
        any.then_some(data)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Heap-like memory only: committed and readable, but not code, guard pages, or
/// write-combined GPU staging buffers (huge, uncached, slow to read, never holding strings).
fn is_scannable(state: u32, protect: u32, size: usize) -> bool {
    state == MEM_COMMIT
        && size <= MAX_REGION
        && protect & (PAGE_NOACCESS | PAGE_GUARD | PAGE_WRITECOMBINE) == 0
        && protect != PAGE_EXECUTE
        && protect != PAGE_EXECUTE_READ
}

fn loadout_directory() -> Option<PathBuf> {
    ProjectDirs::from("com", "synqark", "WarframePeerOverlay")
        .map(|dirs| dirs.data_local_dir().join("loadouts"))
}

fn since_epoch() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// The saved form: the loadout itself, wrapped with whose it is and when it was captured.
fn record(name: &str, platform: Platform, captured: u64, json: &[u8]) -> Option<Vec<u8>> {
    let loadout = serde_json::from_slice::<Value>(json).ok()?;
    serde_json::to_vec_pretty(&json!({
        "name": name,
        "platform": platform.label(),
        "loadout_bytes": json.len(),
        "captured_unix": captured,
        "loadout": loadout,
    }))
    .ok()
}

/// Writes a member's loadout down, and says what it ended up called.
fn save_member(directory: &Path, name: &str, platform: Platform, json: &[u8]) -> Option<String> {
    let captured = since_epoch().as_secs();
    let body = record(name, platform, captured, json)?;
    fs::create_dir_all(directory).ok()?;
    let file = file_name(captured, name, platform);
    fs::write(directory.join(&file), body).ok()?;
    Some(file)
}

/// Replaces `self_latest.json`, the only copy of our own that is kept: it is written aside and
/// renamed over, so something reading it never sees half a file.
fn save_own(directory: &Path, request: &OwnRequest, json: &[u8]) {
    let now = since_epoch().as_secs();
    let Some(body) = record(&request.name, request.platform, now, json) else {
        return;
    };
    if fs::create_dir_all(directory).is_err() {
        return;
    }
    let staged = directory.join("self_latest.json.tmp");
    if fs::write(&staged, &body).is_ok() {
        let _ = fs::rename(&staged, directory.join("self_latest.json"));
    }
}

/// Clears out what nothing can show any more: every member loadout `kept` does not name, and
/// `self/`, the archive of our own rebuilds that earlier versions kept beside
/// `self_latest.json` and nothing ever read.
///
/// A member file captured after `horizon` — the last departure the history wrote down — is
/// spared whether it is named or not: it belongs to a squad still together, whose members are
/// written down only once they leave.
pub fn discard_unkept(kept: &HashSet<&str>, horizon: u64) {
    if let Some(directory) = loadout_directory() {
        discard(&directory, kept, horizon);
    }
}

fn discard(directory: &Path, kept: &HashSet<&str>, horizon: u64) {
    let _ = fs::remove_dir_all(directory.join("self"));
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        // Only what `file_name` wrote is ours to throw away: anything else in the folder — the
        // latest copy of our own among it — opens with no capture time and is left alone.
        let Some(captured) = file
            .to_str()
            .and_then(|file| file.split('_').next()?.parse::<u64>().ok())
        else {
            continue;
        };
        if captured <= horizon && !kept.contains(file.to_string_lossy().as_ref()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// A loadout string found in memory, summed up for the window.
fn summarize(json: &[u8]) -> Option<Loadout> {
    serde_json::from_slice::<Value>(json)
        .ok()
        .map(|loadout| Loadout::from_json(&loadout))
}

/// Our own loadout as `save_own` last left it in `self_latest.json`.
fn latest_own(directory: &Path) -> Option<Captured> {
    let record: Value =
        serde_json::from_slice(&fs::read(directory.join("self_latest.json")).ok()?).ok()?;
    Some(Captured::Own {
        name: record["name"].as_str()?.to_owned(),
        platform: record["platform"].as_str().unwrap_or_default().to_owned(),
        loadout: Loadout::from_json(&record["loadout"]),
        json: serde_json::to_vec(&record["loadout"])
            .map(|json| RawJson::from(json.as_slice()))
            .unwrap_or_default(),
    })
}

/// `<unix time>_<name>_<platform>.json`, with anything Windows would reject replaced.
fn file_name(captured: u64, name: &str, platform: Platform) -> String {
    let safe = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("{captured}_{}_{}.json", safe(name), safe(platform.label()))
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_READWRITE};

    use super::*;

    fn loadout(level: u32, item: &str) -> Vec<u8> {
        format!(r#"{{"PlayerLevel":{level},"NORMAL":[{{"ItemType":"{item}"}}]}}"#).into_bytes()
    }

    fn census_of(entries: &[(&[u8], usize)]) -> HashMap<u64, Found> {
        entries
            .iter()
            .map(|(json, copies)| {
                (
                    content_hash(json),
                    Found {
                        json: json.to_vec(),
                        copies: *copies,
                        addresses: Vec::new(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn counts_complete_loadout_strings_and_skips_the_rest() {
        let suit = loadout(30, "/Example/Suit");
        let other = loadout(5, "/Example/OtherSuit");
        let mut memory = Vec::new();
        for json in [&suit, &other, &suit] {
            memory.extend_from_slice(b"noise");
            memory.extend_from_slice(json);
            memory.push(0);
        }
        // Same head, but the string stops before its object closes.
        memory.extend_from_slice(&suit[..suit.len() - 3]);
        memory.push(0);

        let mut found = HashMap::new();
        collect(&memory, 0x1000, memory.len(), |_| None, &mut found);

        assert_eq!(found.len(), 2);
        assert_eq!(found[&content_hash(&suit)].copies, 2);
        assert_eq!(found[&content_hash(&other)].copies, 1);
        // Where each copy lies, counted from where the chunk starts.
        let second = 5 + suit.len() + 1 + 5 + other.len() + 1 + 5;
        assert_eq!(
            found[&content_hash(&suit)].addresses,
            [0x1000 + 5, 0x1000 + second]
        );
    }

    #[test]
    fn leaves_a_head_in_the_overlap_to_the_next_chunk() {
        let mut memory = b"noise".to_vec();
        memory.extend_from_slice(&loadout(30, "/Example/Suit"));
        memory.push(0);

        let mut found = HashMap::new();
        collect(&memory, 0, 5, |_| None, &mut found);

        assert!(found.is_empty());
    }

    #[test]
    fn fetches_a_loadout_that_runs_past_the_chunk() {
        let suit = loadout(30, "/Example/Suit");
        let mut whole = suit.clone();
        whole.push(0);
        // The chunk holds the whole head but ends partway through the string.
        let chunk = &whole[..LOADOUT_HEAD.len() + 4];

        let mut found = HashMap::new();
        collect(
            chunk,
            0,
            chunk.len(),
            |start| whole.get(start..).map(<[u8]>::to_vec),
            &mut found,
        );

        assert_eq!(found[&content_hash(&suit)].json, suit);
    }

    #[test]
    fn reads_around_a_page_that_has_gone() {
        use windows_sys::Win32::System::Memory::{
            MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        let size = 3 * PAGE;
        // SAFETY: a region of this test's own, written and freed only here.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                size,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        }
        .cast::<u8>();
        assert!(!base.is_null());
        unsafe {
            std::ptr::write_bytes(base, 7, size);
            // The middle page goes, as the game's do while a pass reads its memory.
            VirtualFree(base.add(PAGE).cast(), PAGE, MEM_DECOMMIT);
        }
        let process = Process::open(std::process::id()).expect("a process may read itself");
        let address = base as usize;

        assert_eq!(
            process.read(address, size),
            None,
            "one read fails as a whole"
        );
        let data = process
            .read_pages(address, size)
            .expect("two pages are left");
        assert!(data[..PAGE].iter().all(|&byte| byte == 7));
        assert!(data[PAGE..2 * PAGE].iter().all(|&byte| byte == 0));
        assert!(data[2 * PAGE..].iter().all(|&byte| byte == 7));
        // Not starting on a page, as every chunk but a region's first does not.
        let data = process
            .read_pages(address + 100, size - 100)
            .expect("two pages are left");
        assert_eq!(data.len(), size - 100);
        assert!(data[..PAGE - 100].iter().all(|&byte| byte == 7));
        assert!(
            data[PAGE - 100..2 * PAGE - 100]
                .iter()
                .all(|&byte| byte == 0)
        );
        assert!(data[2 * PAGE - 100..].iter().all(|&byte| byte == 7));

        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
        assert_eq!(
            process.read_pages(address, size),
            None,
            "nothing left to read"
        );
    }

    #[test]
    fn reads_a_loadout_back_out_of_a_live_process() {
        // This test process stands in for the game: the string lives in its heap.
        let suit = loadout(7, "/Example/ReadBackSuit");
        let mut heap = suit.clone();
        heap.push(0);
        let heap = std::hint::black_box(heap);
        // And a record of the test's own making holds its address, as the game's do.
        let record = std::hint::black_box(Box::new(heap.as_ptr() as usize));

        let process = Process::open(std::process::id()).expect("a process may read itself");
        let found = census(&process);
        let hash = content_hash(&suit);
        assert!(found[&hash].addresses.contains(&(heap.as_ptr() as usize)));
        let place = std::ptr::from_ref::<usize>(&record) as usize;
        assert!(referrers(&process, &found).contains(&(place, hash)));
        drop((heap, record));
    }

    /// A record as the game lays one out, in words: the player's name (by address and length,
    /// or in place) some way before the address of their loadout, which ends the window.
    fn record_window(fields: &[u64]) -> Vec<u8> {
        fields.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    /// Up to eight bytes of text as the word that holds them in place.
    fn in_place(text: &[u8]) -> u64 {
        let mut word = [0; 8];
        word[..text.len()].copy_from_slice(text);
        u64::from_le_bytes(word)
    }

    #[test]
    fn reads_the_name_of_the_player_a_record_belongs_to() {
        // A member's record: name kept elsewhere, then ids, then the loadout's address (which
        // lies just past the window). Our own name follows the loadout, out of the window.
        let name = "Tenno\u{e000}".as_bytes().to_vec();
        let id = b"0123456789abcdef01234567".to_vec();
        let window = record_window(&[
            0x7ff6_0000_0000,
            0x2000,                                    // name, by address…
            0xff00_0001_f000_0000 | name.len() as u64, // …and length
            0x3000,                                    // an id by address…
            0xff00_0001_f000_0018,                     // …and length
            0,
            0x3000,
            0xff00_0001_f000_0018,
        ]);
        let read = |address: usize, len: usize| match address {
            0x2000 => Some(name[..len.min(name.len())].to_vec()),
            0x3000 => Some(id[..len.min(id.len())].to_vec()),
            _ => None,
        };
        assert_eq!(nearest_name(&window, read).as_deref(), Some("Tenno"));

        // A row of records keeping short names in place: the nearest is the owner, and the
        // tail of a longer name that runs into a second word is not a name of its own.
        let window = record_window(&[
            in_place(b"LongName"),
            in_place("12\u{e000}".as_bytes()),
            0x4000,
            0xff00_0001_f000_0100,
            in_place("Lotus\u{e000}".as_bytes()),
            0,
        ]);
        assert_eq!(nearest_name(&window, |_, _| None).as_deref(), Some("Lotus"));
        let window = record_window(&[0, in_place(b"LongName"), in_place("12\u{e000}".as_bytes())]);
        assert_eq!(
            nearest_name(&window, |_, _| None).as_deref(),
            Some("LongName12")
        );
        // A number before a name, with no small bytes in it, does not make the name a tail.
        let window = record_window(&[
            0,
            0xa2d2_969e_7683_51c8,
            in_place(b"TennoAbc"),
            in_place("d\u{e000}".as_bytes()),
        ]);
        assert_eq!(
            nearest_name(&window, |_, _| None).as_deref(),
            Some("TennoAbcd")
        );
        // The first word is only looked back at: a name's tail there names nobody.
        let window = record_window(&[in_place("d\u{e000}".as_bytes()), 0]);
        assert_eq!(nearest_name(&window, |_, _| None), None);

        // Ids and other text carry no platform mark, so they name nobody.
        let window = record_window(&[0, 0x3000, 0xff00_0001_f000_0018]);
        assert_eq!(nearest_name(&window, read), None);
    }

    #[test]
    fn leaves_out_a_record_caught_naming_somebody_else_on_a_loadout() {
        let (ours, theirs) = (1_u64, 2_u64);
        let mut records = vec![
            ("Tenno".to_owned(), 0x100, theirs),
            ("Tenno".to_owned(), 0x200, theirs),
            // Caught while the game filled it in: their name, our loadout.
            ("Tenno".to_owned(), 0x300, ours),
        ];
        records.extend((0..5).map(|i| ("Lotus".to_owned(), 0x1000 + i, ours)));

        assert_eq!(
            records_naming(&records, "Tenno"),
            HashMap::from([(0x100, theirs), (0x200, theirs)])
        );
        assert_eq!(records_naming(&records, "Lotus").len(), 5);
        assert!(records_naming(&records, "Ordis").is_empty());
    }

    #[test]
    fn tells_a_player_name_by_its_platform_mark() {
        assert_eq!(player_name("Tenno\u{e000}".as_bytes()), Some("Tenno"));
        assert_eq!(
            player_name("Some Gamertag\u{e001}".as_bytes()),
            Some("Some Gamertag")
        );
        assert_eq!(player_name(b"Tenno"), None);
        assert_eq!(player_name("\u{e000}".as_bytes()), None);
        assert_eq!(player_name(b"0123456789abcdef01234567"), None);
    }

    #[test]
    fn takes_the_version_a_members_records_have_moved_to() {
        let (joined, changed, older) = (
            loadout(16, "/Example/Joined"),
            loadout(16, "/Example/Changed"),
            loadout(16, "/Example/Older"),
        );
        let found = census_of(&[(&joined, 8), (&changed, 4), (&older, 2)]);
        let (joined, changed, older) = (
            content_hash(&joined),
            content_hash(&changed),
            content_hash(&older),
        );
        // The record kept per player, the one made for the JOIN, and one left from before.
        let before = HashMap::from([(0x100, joined), (0x200, joined), (0x300, older)]);

        // Changed in place, and a new record for the message: the lingering one is outvoted.
        let after = HashMap::from([
            (0x100, changed),
            (0x200, joined),
            (0x300, older),
            (0x400, changed),
        ]);
        assert_eq!(pick_update(&after, &before, &found), Some(changed));

        // Nothing moved: nothing to take.
        assert_eq!(pick_update(&before, &before, &found), None);

        // Nothing to weigh against, as after a restart: most records win.
        assert_eq!(pick_update(&after, &HashMap::new(), &found), Some(changed));
    }

    #[test]
    fn takes_a_members_loadout_by_its_announced_length() {
        let (short, long) = (loadout(1, "/Example/A"), loadout(22, "/Example/B"));
        let found = census_of(&[(&short, 1), (&long, 1)]);

        assert_eq!(
            pick_member(&found, long.len(), &HashSet::new()),
            Some(content_hash(&long))
        );
        let taken = HashSet::from([content_hash(&long)]);
        assert_eq!(pick_member(&found, long.len(), &taken), None);
    }

    /// The copies each version had at the previous pass.
    fn previous(versions: &[(&[u8], usize)]) -> Option<HashMap<u64, usize>> {
        Some(
            versions
                .iter()
                .map(|(json, copies)| (content_hash(json), *copies))
                .collect(),
        )
    }

    #[test]
    fn our_own_loadout_is_the_version_just_built() {
        let old = loadout(36, "/Example/Old");
        let preset = loadout(36, "/Example/Preset");
        let rebuilt = loadout(36, "/Example/Rebuilt");
        let member = loadout(10, "/Example/Member");
        let found = census_of(&[(&old, 40), (&preset, 19), (&rebuilt, 3), (&member, 50)]);
        let members = HashSet::from([content_hash(&member)]);

        // Freshly built wins even with few copies yet, and a member's loadout never counts.
        let history = History {
            previous: previous(&[(&old, 40), (&preset, 19)]),
            members: members.clone(),
            ..History::default()
        };
        assert_eq!(
            pick_own(&found, &history),
            Some((content_hash(&rebuilt), Reason::Built))
        );

        // No pass before, and nothing taken for ours yet: nothing to go on but copies.
        let history = History {
            members,
            ..History::default()
        };
        assert_eq!(
            pick_own(&found, &history),
            Some((content_hash(&old), Reason::MostCopied))
        );
    }

    // The next three follow a recording of the game's memory through two visits to the
    // arsenal: an old loadout lingered in 18 copies the whole time, while the current one sat
    // in 4 outside the arsenal and ran to a hundred inside it.

    #[test]
    fn keeps_ours_when_leaving_the_arsenal_rebuilds_it_unchanged() {
        let lingering = loadout(36, "/Example/Volt");
        let current = loadout(36, "/Example/Soma");
        let found = census_of(&[(&lingering, 18), (&current, 4)]);
        let history = History {
            previous: previous(&[(&lingering, 18), (&current, 49)]),
            own: Some(content_hash(&current)),
            ..History::default()
        };

        assert_eq!(
            pick_own(&found, &history),
            Some((content_hash(&current), Reason::Kept)),
            "not the old loadout, for all its copies"
        );
    }

    #[test]
    fn takes_the_version_the_arsenal_copies_about() {
        // Ours was taken wrongly before: entering the arsenal puts it right.
        let lingering = loadout(36, "/Example/Volt");
        let current = loadout(36, "/Example/Rhino");
        let found = census_of(&[(&lingering, 18), (&current, 48)]);
        let history = History {
            previous: previous(&[(&lingering, 18), (&current, 4)]),
            own: Some(content_hash(&lingering)),
            ..History::default()
        };

        assert_eq!(
            pick_own(&found, &history),
            Some((content_hash(&current), Reason::Copied))
        );
    }

    #[test]
    fn takes_a_version_built_again_after_it_was_let_go() {
        // Boar Prime, then Soma Prime, then Boar Prime again: the first version was met long
        // before, but freed in between, so its return is the rebuild.
        let lingering = loadout(36, "/Example/Volt");
        let soma = loadout(36, "/Example/Soma");
        let boar = loadout(36, "/Example/Boar");
        let found = census_of(&[(&lingering, 18), (&soma, 11), (&boar, 56)]);
        let history = History {
            previous: previous(&[(&lingering, 18), (&soma, 63)]),
            own: Some(content_hash(&soma)),
            ..History::default()
        };

        assert_eq!(
            pick_own(&found, &history),
            Some((content_hash(&boar), Reason::Built))
        );
    }

    #[test]
    fn carries_on_from_the_saved_copy_of_ours_after_a_restart() {
        let lingering = loadout(36, "/Example/Volt");
        let current = loadout(36, "/Example/Rhino");
        let found = census_of(&[(&lingering, 18), (&current, 4)]);
        let mut history = History {
            saved_own: serde_json::from_slice(&current).ok(),
            ..History::default()
        };

        recognise_saved_own(&found, &mut history);
        assert_eq!(history.own, Some(content_hash(&current)));
        assert_eq!(history.saved_own, None, "looked for no longer");
        assert_eq!(
            pick_own(&found, &history),
            Some((content_hash(&current), Reason::Kept))
        );
    }

    #[test]
    fn leaves_another_players_freshly_built_loadout_out_of_ours() {
        // The game rebuilds members' loadouts locally too, so a version no pass has met is not
        // ours by that alone, and one ranked below us never is.
        let ours = loadout(36, "/Example/Ours");
        let theirs = loadout(31, "/Example/Theirs");
        let found = census_of(&[(&ours, 40), (&theirs, 2)]);
        let history = History {
            previous: previous(&[(&ours, 40)]),
            own: Some(content_hash(&ours)),
            own_mastery: Some(36),
            ..History::default()
        };

        assert_eq!(
            pick_own(&found, &history),
            Some((content_hash(&ours), Reason::Kept)),
            "ours stands, unchanged since the pass that met it"
        );
    }

    #[test]
    fn reaches_past_a_rank_floor_that_leaves_nothing() {
        // A floor a wrong capture left behind must not lock ours out for good.
        let ours = loadout(36, "/Example/Ours");
        let found = census_of(&[(&ours, 40)]);
        let history = History {
            own_mastery: Some(40),
            ..History::default()
        };

        assert_eq!(
            pick_own(&found, &history),
            Some((content_hash(&ours), Reason::MostCopied))
        );
    }

    #[test]
    fn reads_the_mastery_rank_a_loadout_opens_with() {
        assert_eq!(mastery_rank(&loadout(36, "/Example/Suit")), Some(36));
        assert_eq!(mastery_rank(br#"{"PlayerLevel":}"#), None);
    }

    #[test]
    fn replaces_the_latest_copy_of_our_own_and_keeps_no_other() {
        // The thread name contains `::`, which Windows rejects in a file name.
        let directory =
            std::env::temp_dir().join(format!("warframe-peer-overlay-own-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let request = OwnRequest {
            pid: 1,
            name: "LocalTenno".to_owned(),
            platform: Platform::Pc,
        };

        save_own(&directory, &request, &loadout(36, "/Example/First"));
        save_own(&directory, &request, &loadout(36, "/Example/Second"));

        let latest: Value =
            serde_json::from_slice(&fs::read(directory.join("self_latest.json")).unwrap()).unwrap();
        assert_eq!(latest["name"], "LocalTenno");
        assert_eq!(
            latest["loadout"]["NORMAL"][0]["ItemType"],
            "/Example/Second"
        );
        // The one before it is nowhere: only the latest is of any use.
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn clears_out_the_loadouts_nothing_can_show_any_more() {
        // The thread name contains `::`, which Windows rejects in a file name.
        let directory = std::env::temp_dir().join(format!(
            "warframe-peer-overlay-discard-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(directory.join("self")).unwrap();
        fs::write(directory.join("self").join("100.json"), "{}").unwrap();
        for file in [
            "10_WrittenDown_PC.json",
            "20_Forgotten_PC.json",
            "40_StillHere_PC.json",
            "self_latest.json",
        ] {
            fs::write(directory.join(file), "{}").unwrap();
        }

        discard(&directory, &HashSet::from(["10_WrittenDown_PC.json"]), 30);

        let mut left = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        left.sort();
        // The one the history names stays, and so does one captured since the last departure it
        // wrote down; our own archive goes whatever is in it, and so does the rest.
        assert_eq!(
            left,
            [
                "10_WrittenDown_PC.json",
                "40_StillHere_PC.json",
                "self_latest.json"
            ]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reads_our_last_saved_loadout_back() {
        // The thread name contains `::`, which Windows rejects in a file name.
        let directory = std::env::temp_dir().join(format!(
            "warframe-peer-overlay-latest-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        assert_eq!(latest_own(&directory), None, "nothing saved yet");

        let request = OwnRequest {
            pid: 1,
            name: "LocalTenno".to_owned(),
            platform: Platform::Pc,
        };
        save_own(&directory, &request, &loadout(36, "/Example/Suit"));

        let Some(Captured::Own {
            name,
            platform,
            loadout,
            json,
        }) = latest_own(&directory)
        else {
            panic!("our loadout was just saved");
        };
        assert_eq!((name.as_str(), platform.as_str()), ("LocalTenno", "PC"));
        assert_eq!(loadout.mastery_rank, Some(36));
        // The whole of it comes back too, for a window to hand out.
        assert!(json.pretty().contains("\"PlayerLevel\": 36"));
        assert_eq!(
            loadout.warframe.map(|item| item.path).as_deref(),
            Some("/Example/Suit")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn sums_up_what_the_window_shows() {
        let loadout = Loadout::from_json(&json!({
            "PlayerLevel": 12,
            "PostNewWar": true,
            "PostOldPeace": false,
            "KubrowName": "Pup",
            "AuraName": "/Example/AuraName",
            "ExtraAuraName": "",
            "FocusAbility": "/Example/Focus/SchoolFocusAbility",
            "OPERATOR_ADULT": [],
            "NORMAL": [
                {
                    "ItemType": "/Example/Suit",
                    "Level": 30,
                    "Polarized": 2,
                    "WeaponUpgrades": ["/Example/Skin", "", "/Example/Mod"],
                },
                {"ItemType": "/Example/Pistol", "Level": 25},
                {"ItemType": "/Example/Rifle"},
                {},
            ],
            "SENTINEL": [{
                "ItemType": "/Example/Moa",
                "Level": 5,
                "ModularPartTypes": ["/Example/MoaHead", "/Example/MoaCore"],
            }],
        }));
        let item = |path: &str, rank, forma| {
            Some(Item {
                path: path.to_owned(),
                rank,
                forma,
                parts: Vec::new(),
                upgrades: Vec::new(),
            })
        };
        let suit = Item {
            // The empty slot between them is left out.
            upgrades: vec!["/Example/Skin".to_owned(), "/Example/Mod".to_owned()],
            ..item("/Example/Suit", Some(30), Some(2)).unwrap()
        };
        let moa = Item {
            parts: vec!["/Example/MoaHead".to_owned(), "/Example/MoaCore".to_owned()],
            ..item("/Example/Moa", Some(5), None).unwrap()
        };

        assert_eq!(
            loadout,
            Loadout {
                mastery_rank: Some(12),
                post_new_war: Some(true),
                post_old_peace: Some(false),
                warframe: Some(suit),
                primary: item("/Example/Rifle", None, None),
                secondary: item("/Example/Pistol", Some(25), None),
                melee: None,
                companion: Some(moa),
                companion_name: Some("Pup".to_owned()),
                // A second aura slot left empty is no aura.
                auras: vec!["/Example/AuraName".to_owned()],
                operator: Some(Operator {
                    drifter: true,
                    focus: Some("/Example/Focus/SchoolFocusAbility".to_owned()),
                }),
            }
        );
    }

    #[test]
    fn tells_the_operator_from_the_drifter() {
        let operator = |loadout| Loadout::from_json(&loadout).operator;
        assert_eq!(
            operator(json!({"OPERATOR": [], "FocusAbility": ""})),
            Some(Operator {
                drifter: false,
                focus: None
            })
        );
        assert!(operator(json!({"OPERATOR": [], "OPERATOR_ADULT": []})).is_some_and(|o| o.drifter));
        // Without either, nobody stands behind the warframe, whatever focus is listed.
        assert_eq!(operator(json!({"FocusAbility": "/Example/Focus"})), None);
    }

    #[test]
    fn reads_a_summary_written_down_before_auras_and_operators_were_kept() {
        let earlier = json!({
            "mastery_rank": 30,
            "post_new_war": true,
            "post_old_peace": null,
            "warframe": null,
            "primary": null,
            "secondary": null,
            "melee": null,
            "companion": null,
            "companion_name": null,
        });
        let loadout: Loadout =
            serde_json::from_value(earlier).expect("an older summary still reads");
        assert_eq!(loadout.mastery_rank, Some(30));
        assert!(loadout.auras.is_empty());
        assert_eq!(loadout.operator, None);
    }

    #[test]
    fn lays_a_loadout_out_over_several_lines_to_be_handed_out() {
        let json = RawJson::from(loadout(36, "/Example/Suit").as_slice());
        assert!(json.pretty().starts_with("{\n  \"PlayerLevel\": 36,"));

        // Whatever cannot be read stands as it came.
        let broken = RawJson::from(b"{\"PlayerLevel\":".as_slice());
        assert_eq!(broken.pretty(), "{\"PlayerLevel\":");
        assert!(RawJson::default().is_empty());
    }

    #[test]
    fn leaves_out_whatever_a_loadout_lacks() {
        assert_eq!(
            Loadout::from_json(&json!({"NORMAL": "not a list", "KubrowName": ""})),
            Loadout::default()
        );
    }

    #[test]
    fn scans_only_heap_like_regions() {
        assert!(is_scannable(MEM_COMMIT, PAGE_READWRITE, 4096));
        assert!(!is_scannable(MEM_COMMIT, PAGE_EXECUTE_READ, 4096));
        assert!(!is_scannable(MEM_COMMIT, PAGE_READWRITE | PAGE_GUARD, 4096));
        assert!(!is_scannable(
            MEM_COMMIT,
            PAGE_READWRITE | PAGE_WRITECOMBINE,
            4096
        ));
        assert!(!is_scannable(MEM_RESERVE, PAGE_READWRITE, 4096));
        assert!(!is_scannable(MEM_COMMIT, PAGE_READWRITE, MAX_REGION + 1));
    }

    #[test]
    fn names_files_safely_whatever_the_player_is_called() {
        assert_eq!(
            file_name(1789127427, "Tenno", Platform::Pc),
            "1789127427_Tenno_PC.json"
        );
        assert_eq!(
            file_name(1, "Mr Tenno", Platform::Unknown),
            "1_Mr_Tenno____.json"
        );
    }
}

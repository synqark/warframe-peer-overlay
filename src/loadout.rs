//! Mission loadouts, squad members' and our own, read out of the game's memory.
//!
//! A loadout lives in the game's heap as a NUL-terminated JSON string starting with
//! `{"PlayerLevel":`: warframe and weapons with their mods, forma and levels, companion,
//! archwing, operator, gear and focus. One pass over the game's memory collects every such
//! string, counting identical copies, and serves two kinds of job:
//!
//! - **Members.** When a player joins the squad, EE.log announces
//!   `ProcessSquadMessage received JOIN message from <name>, loadout: <N> bytes`, and their
//!   loadout is the string of exactly N bytes.
//! - **Ourselves.** Every `BuildLoadOut for <local player>` (each change in the arsenal, leaving
//!   it, loading into a mission) rebuilds our loadout as a new string within the same second.
//!   The version no earlier pass has met is the one just built; without one, the most-copied
//!   version is current, since superseded ones are freed within minutes. Members' versions
//!   never count.
//!
//! Besides being saved, every loadout found is handed back to the monitor, summed up as a
//! [`Loadout`], for the loadout window. Ours starts out as the copy last saved, until the game
//! shows it again.
//!
//! Nothing here depends on struct layouts or pointers, so a game update that moves things
//! around in memory does not break it. Access is read-only
//! (`PROCESS_VM_READ | PROCESS_QUERY_INFORMATION`): nothing is written, injected or hooked. A
//! pass takes a second or two, so captures run on a worker thread of their own and the monitor
//! loop only hands it jobs.

use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
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
/// Anything larger is graphics or mapped memory, never the heap strings wanted here.
const MAX_REGION: usize = 512 * 1024 * 1024;
/// How often work that found nothing is retried, and for how long a member is looked for.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);
/// Passes spent on one rebuild of our own loadout before settling for the most-copied version.
const OWN_PASSES: u32 = 2;

/// One member's loadout, announced by EE.log as `bytes` long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRequest {
    pub pid: u32,
    pub name: String,
    pub platform: Platform,
    pub bytes: usize,
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
    Own(OwnRequest),
}

/// A loadout the worker found, handed back to the monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Captured {
    /// A member's, found for the JOIN that announced it as `bytes` long.
    Member {
        name: String,
        bytes: usize,
        loadout: Loadout,
    },
    /// Our own, as last rebuilt.
    Own {
        name: String,
        platform: String,
        loadout: Loadout,
    },
}

/// What the loadout window shows of a loadout. Anything missing, or shaped otherwise than
/// expected after a game update, is simply left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
}

/// One equipped item. Its `WeaponUpgrades` are not kept yet: a flat list of paths mixing
/// cosmetics, mods and arcanes, with `""` for an empty slot, which `names::Kind` tells apart.
#[derive(Clone, Debug, PartialEq, Eq)]
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
            companion_name: loadout["KubrowName"]
                .as_str()
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
        }
    }
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
        Some(Self {
            path: path.to_owned(),
            rank: entry["Level"].as_u64(),
            forma: entry["Polarized"].as_u64(),
            parts,
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
}

/// What the worker remembers between passes.
#[derive(Default)]
struct History {
    /// Every version met so far, so a freshly built one stands out.
    seen: HashSet<u64>,
    /// Versions saved for squad members; never taken for our own.
    members: HashSet<u64>,
    /// The version last saved as our own.
    own: Option<u64>,
}

fn run(receiver: Receiver<Job>, captured: Sender<Captured>) {
    let directory = loadout_directory();
    // Until the game shows ours again, the copy last saved stands in for it.
    if let Some(own) = directory.as_deref().and_then(latest_own) {
        let _ = captured.send(own);
    }
    let mut members: Vec<(CaptureRequest, Instant)> = Vec::new();
    let mut own: Option<(OwnRequest, u32)> = None;
    let mut history = History::default();
    loop {
        // Idle until asked; with work outstanding, wake up again to retry it.
        let received = if members.is_empty() && own.is_none() {
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
                // A newer rebuild supersedes one still being looked for.
                Job::Own(request) => own = Some((request, 0)),
            }
        }

        let mut pids: Vec<u32> = members
            .iter()
            .map(|(request, _)| request.pid)
            .chain(own.iter().map(|(request, _)| request.pid))
            .collect();
        pids.sort_unstable();
        pids.dedup();
        for pid in pids {
            let found = census(pid);
            // Members first, so a version just given to one is never taken for our own.
            members.retain(|(request, since)| {
                if request.pid != pid {
                    return true;
                }
                if let Some(hash) = pick_member(&found, request.bytes, &history.members) {
                    history.members.insert(hash);
                    let json = &found[&hash].json;
                    if let Some(directory) = &directory {
                        save_member(directory, request, json);
                    }
                    if let Some(loadout) = summarize(json) {
                        let _ = captured.send(Captured::Member {
                            name: request.name.clone(),
                            bytes: request.bytes,
                            loadout,
                        });
                    }
                    return false;
                }
                since.elapsed() < CAPTURE_TIMEOUT
            });
            let own_done = match &mut own {
                Some((request, passes)) if request.pid == pid => {
                    *passes += 1;
                    let pick = pick_own(&found, &history.seen, &history.members);
                    if let Some((hash, _)) = pick
                        && history.own != Some(hash)
                    {
                        history.own = Some(hash);
                        let json = &found[&hash].json;
                        if let Some(directory) = &directory {
                            save_own(directory, request, json);
                        }
                        if let Some(loadout) = summarize(json) {
                            let _ = captured.send(Captured::Own {
                                name: request.name.clone(),
                                platform: request.platform.label().to_owned(),
                                loadout,
                            });
                        }
                    }
                    // A version not met before is the rebuild itself; without one, look once
                    // more in case it was still being written.
                    pick.is_some_and(|(_, fresh)| fresh) || *passes >= OWN_PASSES
                }
                _ => false,
            };
            if own_done {
                own = None;
            }
            history.seen.extend(found.keys().copied());
        }
    }
}

/// Every distinct loadout string in the game's memory, keyed by content hash.
fn census(pid: u32) -> HashMap<u64, Found> {
    let mut found = HashMap::new();
    let Some(process) = Process::open(pid) else {
        return found;
    };
    for (base, size) in process.regions() {
        let mut offset = 0;
        while offset < size {
            let want = CHUNK.min(size - offset);
            let last = offset + want >= size;
            let chunk_base = base + offset;
            if let Some(data) = process.read(chunk_base, want) {
                // A head in the overlap is met again at the start of the next chunk.
                let accept_before = if last {
                    data.len()
                } else {
                    CHUNK - LOADOUT_HEAD.len()
                };
                collect(
                    &data,
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
/// counting identical copies. One that runs past the end of the chunk is fetched with
/// `read_past(start)` instead. A string counts only if it ends in `}` right before its NUL
/// terminator and parses as JSON.
fn collect(
    data: &[u8],
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
        } else if serde_json::from_slice::<serde::de::IgnoredAny>(json).is_ok() {
            found.insert(
                hash,
                Found {
                    json: json.to_vec(),
                    copies: 1,
                },
            );
        }
    }
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

/// Our own current loadout: the most-copied version no earlier pass has met, if there is one
/// (the rebuild itself, marked `true`), otherwise the most-copied version overall.
fn pick_own(
    found: &HashMap<u64, Found>,
    seen: &HashSet<u64>,
    members: &HashSet<u64>,
) -> Option<(u64, bool)> {
    let most_copied = |fresh_only: bool| {
        found
            .iter()
            .filter(|(hash, _)| !members.contains(*hash) && (!fresh_only || !seen.contains(*hash)))
            .max_by_key(|(_, entry)| entry.copies)
            .map(|(hash, _)| *hash)
    };
    most_copied(true)
        .map(|hash| (hash, true))
        .or_else(|| most_copied(false).map(|hash| (hash, false)))
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

fn save_member(directory: &Path, request: &CaptureRequest, json: &[u8]) {
    let captured = since_epoch().as_secs();
    if let Some(body) = record(&request.name, request.platform, captured, json)
        && fs::create_dir_all(directory).is_ok()
    {
        let path = directory.join(file_name(captured, &request.name, request.platform));
        let _ = fs::write(path, body);
    }
}

/// Adds to our own history under `self/` and replaces `self_latest.json`. The latest copy is
/// written aside and renamed over, so something reading it never sees half a file.
fn save_own(directory: &Path, request: &OwnRequest, json: &[u8]) {
    let now = since_epoch();
    let history = directory.join("self");
    let Some(body) = record(&request.name, request.platform, now.as_secs(), json) else {
        return;
    };
    if fs::create_dir_all(&history).is_err() {
        return;
    }
    let _ = fs::write(history.join(format!("{}.json", now.as_millis())), &body);
    let staged = directory.join("self_latest.json.tmp");
    if fs::write(&staged, &body).is_ok() {
        let _ = fs::rename(&staged, directory.join("self_latest.json"));
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
        collect(&memory, memory.len(), |_| None, &mut found);

        assert_eq!(found.len(), 2);
        assert_eq!(found[&content_hash(&suit)].copies, 2);
        assert_eq!(found[&content_hash(&other)].copies, 1);
    }

    #[test]
    fn leaves_a_head_in_the_overlap_to_the_next_chunk() {
        let mut memory = b"noise".to_vec();
        memory.extend_from_slice(&loadout(30, "/Example/Suit"));
        memory.push(0);

        let mut found = HashMap::new();
        collect(&memory, 5, |_| None, &mut found);

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
            chunk.len(),
            |start| whole.get(start..).map(<[u8]>::to_vec),
            &mut found,
        );

        assert_eq!(found[&content_hash(&suit)].json, suit);
    }

    #[test]
    fn reads_a_loadout_back_out_of_a_live_process() {
        // This test process stands in for the game: the string lives in its heap.
        let suit = loadout(7, "/Example/ReadBackSuit");
        let mut heap = suit.clone();
        heap.push(0);
        let heap = std::hint::black_box(heap);

        assert!(census(std::process::id()).contains_key(&content_hash(&suit)));
        drop(heap);
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

    #[test]
    fn our_own_loadout_is_the_version_just_built() {
        let old = loadout(36, "/Example/Old");
        let preset = loadout(36, "/Example/Preset");
        let rebuilt = loadout(36, "/Example/Rebuilt");
        let member = loadout(10, "/Example/Member");
        let found = census_of(&[(&old, 40), (&preset, 19), (&rebuilt, 3), (&member, 50)]);
        let members = HashSet::from([content_hash(&member)]);

        // Freshly built wins even with few copies yet, and a member's loadout never counts.
        let seen = HashSet::from([content_hash(&old), content_hash(&preset)]);
        assert_eq!(
            pick_own(&found, &seen, &members),
            Some((content_hash(&rebuilt), true))
        );
        // Nothing new: the most-copied version is the current one.
        let seen = HashSet::from([
            content_hash(&old),
            content_hash(&preset),
            content_hash(&rebuilt),
        ]);
        assert_eq!(
            pick_own(&found, &seen, &members),
            Some((content_hash(&old), false))
        );
    }

    #[test]
    fn keeps_our_history_and_replaces_the_latest_copy() {
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
        thread::sleep(Duration::from_millis(5));
        save_own(&directory, &request, &loadout(36, "/Example/Second"));

        let latest: Value =
            serde_json::from_slice(&fs::read(directory.join("self_latest.json")).unwrap()).unwrap();
        assert_eq!(latest["name"], "LocalTenno");
        assert_eq!(
            latest["loadout"]["NORMAL"][0]["ItemType"],
            "/Example/Second"
        );
        assert_eq!(fs::read_dir(directory.join("self")).unwrap().count(), 2);
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
        }) = latest_own(&directory)
        else {
            panic!("our loadout was just saved");
        };
        assert_eq!((name.as_str(), platform.as_str()), ("LocalTenno", "PC"));
        assert_eq!(loadout.mastery_rank, Some(36));
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
            "NORMAL": [
                {"ItemType": "/Example/Suit", "Level": 30, "Polarized": 2},
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
            })
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
                warframe: item("/Example/Suit", Some(30), Some(2)),
                primary: item("/Example/Rifle", None, None),
                secondary: item("/Example/Pistol", Some(25), None),
                melee: None,
                companion: Some(moa),
                companion_name: Some("Pup".to_owned()),
            }
        );
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

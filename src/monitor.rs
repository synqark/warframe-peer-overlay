use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT},
    Graphics::Gdi::ClientToScreen,
    UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowThreadProcessId, IsWindowVisible,
    },
};

use crate::{
    geo::{GeoInfo, GeoResolver, country_name},
    history::{History, HistoryEntry},
    loadout::{self, CaptureRequest, Captured, Job, Loadout, OwnRequest, RawJson, UpdateRequest},
    mission::{Mission, MissionTracker},
    parser::{LogParser, Peer},
};

#[derive(Clone, Debug, Default)]
pub struct PeerView {
    pub name: String,
    pub platform: String,
    pub ip: Option<String>,
    pub country: String,
    pub country_code: String,
    pub region: String,
    pub org: String,
    pub is_hosting: bool,
    pub is_host: bool,
    pub is_local: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowRect {
    pub left: i32,
    pub bottom: i32,
    pub width: i32,
    pub height: i32,
}

/// One card of the loadout window, and what the history keeps of a player.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadoutView {
    pub name: String,
    pub platform: String,
    pub is_local: bool,
    /// Whether the squad connects through this player, as `PeerView::is_host` has it.
    pub is_host: bool,
    /// Where the peer connects from, as resolved for its `PeerView`; empty when unknown, as
    /// it is for ourselves and whenever `--no-geo` is passed.
    pub country: String,
    pub country_code: String,
    pub region: String,
    /// `None` until captured, which for some members never happens (see README).
    pub loadout: Option<Loadout>,
    /// The whole of what was captured, for the windows to hand out; empty until then. The
    /// history leaves it out of what it writes down: tens of kilobytes a player would run to
    /// tens of megabytes, and the copies saved under `loadouts` have it already.
    #[serde(skip)]
    pub json: RawJson,
}

#[derive(Clone, Debug)]
pub struct MonitorSnapshot {
    pub warframe_running: bool,
    pub log_path: PathBuf,
    pub status: String,
    pub peers: Vec<PeerView>,
    pub window_rect: Option<WindowRect>,
    /// The loadout window's cards: ours first, then each member still in the squad.
    pub loadouts: Vec<LoadoutView>,
    /// Players from squads gone by, newest first.
    pub history: Arc<[HistoryEntry]>,
    /// The missions loaded lately, the newest last, for the session window.
    pub missions: Vec<Mission>,
    /// Everyone in the squad bar ourselves, by name, with the mission each is tied to: what
    /// the history will write down with them when they leave.
    pub squad_ties: Vec<(String, Option<Mission>)>,
}

pub fn spawn(geo_enabled: bool) -> Receiver<MonitorSnapshot> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || run(sender, geo_enabled));
    receiver
}

fn run(sender: Sender<MonitorSnapshot>, geo_enabled: bool) {
    let log_path = find_log_path();
    let mut system = System::new();
    let mut parser = LogParser::default();
    // Apart from the parser: leaving a squad clears that, and the mission goes on regardless.
    let mut missions = MissionTracker::default();
    let mut geo = GeoResolver::new(geo_enabled);
    let mut geo_results = HashMap::<String, GeoInfo>::new();
    let mut geo_failures = HashSet::<String>::new();
    let (loadouts, captures) = loadout::spawn();
    let mut requested_loadouts = HashSet::<(String, usize)>::new();
    // By name and announced size, so a member who re-joins with other gear is not shown the
    // old one while the new one is looked for.
    let mut member_loadouts = HashMap::<(String, usize), (Loadout, RawJson)>::new();
    // A member's loadout as changed since they joined, by name: it stands in for the one their
    // JOIN announced until they leave, or a JOIN brings a newer one.
    let mut member_updates = HashMap::<String, (Loadout, RawJson)>::new();
    let mut seen_loadout_messages = 0;
    // What each member's loadout was saved as, by name, until they leave and it is written down
    // with them. Until then it is what spares the file from a clear-out.
    let mut saved = HashMap::<String, String>::new();
    let mut own_loadout: Option<LoadoutView> = None;
    let mut history = History::load();
    discard_what_nothing_shows(&history, &saved);
    let mut written_down = Arc::<[HistoryEntry]>::from(history.entries());
    // The squad as the last pass round saw it, to tell who has left it since.
    let mut squad = HashMap::<String, LoadoutView>::new();
    // How many players the history has been given, to tell a snapshot it has more to show.
    let mut records = 0_u64;
    let mut seen_own_builds = 0;
    let mut seen_other_builds = 0;
    let mut file_position = 0;
    let mut pending = String::new();
    let mut was_running = false;
    let mut last_signature = String::new();

    loop {
        // Only process names/pids are needed to detect Warframe, so skip the
        // per-process CPU/memory/disk-usage/task refresh that `refresh_processes` does by default.
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        let warframe_pid = system.processes().values().find_map(|process| {
            matches!(
                process
                    .name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .as_str(),
                "warframe.x64.exe" | "warframe.exe"
            )
            .then(|| process.pid().as_u32())
        });
        let running = warframe_pid.is_some();
        let window_rect = warframe_pid.and_then(find_process_window_rect);
        if was_running && !running {
            parser.clear();
            missions.restart();
            geo_failures.clear();
            requested_loadouts.clear();
            member_loadouts.clear();
            member_updates.clear();
            file_position = 0;
            pending.clear();
        }
        was_running = running;

        // Whether a mission's session ended in what was read this time round.
        let mut session_ended = false;
        let status = if !running {
            "Warframeを待機中".to_owned()
        } else if !log_path.exists() {
            "EE.logが見つかりません".to_owned()
        } else {
            match read_appended_lines(
                &log_path,
                &mut file_position,
                &mut pending,
                &mut parser,
                &mut missions,
            ) {
                Ok(()) => {
                    session_ended = missions.sessions_ended();
                    "EE.logを監視中".to_owned()
                }
                Err(error) => format!("EE.log読み取りエラー: {error}"),
            }
        };

        if let Some(pid) = warframe_pid {
            // The worker only stops once this thread drops the sender, so sends cannot fail.
            for request in capture_requests(
                parser.peers(),
                parser.local_user(),
                pid,
                &mut requested_loadouts,
            ) {
                let _ = loadouts.send(Job::Member(request));
            }
            for request in update_requests(&parser, pid, &mut seen_loadout_messages) {
                let _ = loadouts.send(Job::Update(request));
            }
            if let Some(request) = own_request(&parser, pid, &mut seen_own_builds) {
                let _ = loadouts.send(Job::Own(request));
            }
            if let Some(job) = others_note(&parser, pid, &mut seen_other_builds) {
                let _ = loadouts.send(job);
            }
        }
        for captured in captures.try_iter() {
            match captured {
                Captured::Member {
                    name,
                    bytes,
                    loadout,
                    json,
                    file,
                } => {
                    if let Some(file) = file {
                        saved.insert(name.clone(), file);
                    }
                    // A JOIN captured now is newer than any change seen before it.
                    member_updates.remove(&name);
                    member_loadouts.insert((name, bytes), (loadout, json));
                }
                Captured::Update {
                    name,
                    loadout,
                    json,
                    file,
                } => {
                    if let Some(file) = file {
                        saved.insert(name.clone(), file);
                    }
                    member_updates.insert(name, (loadout, json));
                }
                Captured::Own {
                    name,
                    platform,
                    loadout,
                    json,
                } => {
                    own_loadout = Some(LoadoutView {
                        name,
                        platform,
                        is_local: true,
                        loadout: Some(loadout),
                        json,
                        ..LoadoutView::default()
                    });
                }
            }
        }

        for ip in parser.peers().iter().filter_map(|peer| peer.ip.as_deref()) {
            if !geo_results.contains_key(ip) && !geo_failures.contains(ip) {
                match geo.resolve(ip) {
                    Some(info) => {
                        geo_results.insert(ip.to_owned(), info);
                    }
                    None => {
                        geo_failures.insert(ip.to_owned());
                    }
                }
            }
        }
        let peers = parser
            .peers()
            .iter()
            .map(|peer| {
                let geo_info = peer.ip.as_deref().and_then(|ip| geo_results.get(ip));
                PeerView {
                    name: peer.name.clone(),
                    platform: peer.platform.label().to_owned(),
                    ip: peer.ip.clone(),
                    country: geo_info
                        .map(|info| country_name(&info.country).to_owned())
                        .unwrap_or_default(),
                    country_code: geo_info
                        .map(|info| info.country.clone())
                        .unwrap_or_default(),
                    region: geo_info.map(|info| info.region.clone()).unwrap_or_default(),
                    org: geo_info.map(|info| info.org.clone()).unwrap_or_default(),
                    is_hosting: geo_info.is_some_and(|info| info.is_hosting),
                    is_host: peer.is_host,
                    is_local: parser.local_user() == Some(peer.name.as_str()),
                }
            })
            .collect::<Vec<_>>();
        let loadout_cards = loadout_views(
            &parser,
            &peers,
            own_loadout.as_ref(),
            &member_loadouts,
            &member_updates,
        );
        // Whoever was in the squad a pass ago and is not in it now has left: that is the
        // moment everything about them is known, so that is when they are written down. Those
        // whose loadout was never captured are let go, being a name and little else.
        let members: HashMap<String, LoadoutView> = loadout_cards
            .iter()
            .filter(|view| !view.is_local)
            .map(|view| (view.name.clone(), view.clone()))
            .collect();
        // A change seen while they were here is no longer theirs to show if they come back:
        // whoever joins again is shown what their JOIN announces, until they change it again.
        member_updates.retain(|name, _| !squad.contains_key(name) || members.contains_key(name));
        // A session's end writes down everyone still in the squad it tied, with the loadout
        // they played it in: a squad that stays together is written down once a mission, not
        // only when it comes apart. It unties them, so leaving afterwards writes nothing down
        // twice, and the next mission ties them afresh. Their loadout's file stays as it is,
        // for the entries a later mission makes of the same gear.
        let mut written = false;
        if session_ended {
            let mut finished = members.values().cloned().collect::<Vec<_>>();
            finished.sort_by(|one, other| one.name.cmp(&other.name));
            for view in finished {
                let tie = missions.tie(&view.name).cloned();
                if let Some(mission) = tie.filter(|_| view.loadout.is_some()) {
                    let file = saved.get(&view.name).cloned().unwrap_or_default();
                    missions.untie(&view.name);
                    history.record(view, file, &mission);
                    records += 1;
                    written = true;
                }
            }
        }
        let left = squad
            .values()
            .filter(|view| !members.contains_key(&view.name) && view.loadout.is_some())
            .cloned()
            .collect::<Vec<_>>();
        if !left.is_empty() {
            for view in left {
                let file = saved.remove(&view.name).unwrap_or_default();
                // Only a member who played a mission with us is written down, with the mission
                // they were last tied to, which outlives their leaving. One who left before any
                // mission started — or who was written down by its end and has played none
                // since — is let go, and the loadout saved for them with them.
                if let Some(mission) = missions.tie(&view.name).cloned() {
                    history.record(view, file, &mission);
                    records += 1;
                    written = true;
                }
            }
            discard_what_nothing_shows(&history, &saved);
        }
        if written {
            history.save();
            written_down = Arc::from(history.entries());
        }
        squad = members;
        let recent_missions = Vec::from(missions.missions().clone());
        let squad_ties = missions.squad_ties();
        let signature = format!(
            "{running}:{status}:{peers:?}:{window_rect:?}:{loadout_cards:?}:{records}:{recent_missions:?}:{squad_ties:?}"
        );
        if signature != last_signature {
            if sender
                .send(MonitorSnapshot {
                    warframe_running: running,
                    log_path: log_path.clone(),
                    status,
                    peers,
                    window_rect,
                    loadouts: loadout_cards,
                    history: Arc::clone(&written_down),
                    missions: recent_missions,
                    squad_ties,
                })
                .is_err()
            {
                return;
            }
            last_signature = signature;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Loadouts to hand the capture worker: each member's announced loadout, once per
/// announcement, and never our own.
fn capture_requests(
    peers: &[Peer],
    local_user: Option<&str>,
    pid: u32,
    requested: &mut HashSet<(String, usize)>,
) -> Vec<CaptureRequest> {
    peers
        .iter()
        .filter(|peer| local_user != Some(peer.name.as_str()))
        .filter_map(|peer| {
            let bytes = peer.loadout_bytes?;
            requested
                .insert((peer.name.clone(), bytes))
                .then(|| CaptureRequest {
                    pid,
                    name: peer.name.clone(),
                    platform: peer.platform,
                    bytes,
                })
        })
        .collect()
}

/// Every member's loadout is worth looking for again whenever EE.log shows a member sending
/// theirs to the squad anew: the line cannot say whose it is (see
/// `LogParser::loadout_messages`), and a member who changed nothing is soon told apart, their
/// records having moved nowhere. Right after start-up that is immediately, as the log replay
/// counts every message so far.
fn update_requests(parser: &LogParser, pid: u32, seen_messages: &mut u64) -> Vec<UpdateRequest> {
    if parser.loadout_messages() == *seen_messages {
        return Vec::new();
    }
    *seen_messages = parser.loadout_messages();
    parser
        .peers()
        .iter()
        .filter(|peer| parser.local_user() != Some(peer.name.as_str()))
        .map(|peer| UpdateRequest {
            pid,
            name: peer.name.clone(),
            platform: peer.platform,
        })
        .collect()
}

/// Our own loadout is worth capturing again whenever EE.log shows it being rebuilt. Right
/// after start-up that is immediately, as the log replay counts every rebuild so far.
fn own_request(parser: &LogParser, pid: u32, seen_builds: &mut u64) -> Option<OwnRequest> {
    let name = parser.local_user()?;
    if parser.own_builds() == *seen_builds {
        return None;
    }
    *seen_builds = parser.own_builds();
    Some(OwnRequest {
        pid,
        name: name.to_owned(),
        platform: parser.local_platform(),
    })
}

/// Clears the loadouts folder of what nothing can show any more. What the history can still
/// lay out is kept, and so is what has been captured for a squad still together — its members
/// are written down only once they leave.
fn discard_what_nothing_shows(history: &History, saved: &HashMap<String, String>) {
    let kept: HashSet<&str> = history
        .files()
        .chain(saved.values().map(String::as_str))
        .collect();
    loadout::discard_unkept(&kept, history.horizon());
}

/// The loadout window's cards: ours always first, then every member still in the squad, in
/// the order they joined. A member whose loadout is not captured still gets a card. A member
/// is shown the loadout they changed to since joining, when there is one (`updates`), and the
/// one their JOIN announced otherwise.
fn loadout_views(
    parser: &LogParser,
    peers: &[PeerView],
    own: Option<&LoadoutView>,
    members: &HashMap<(String, usize), (Loadout, RawJson)>,
    updates: &HashMap<String, (Loadout, RawJson)>,
) -> Vec<LoadoutView> {
    let local_user = parser.local_user();
    let mut own = own.cloned().unwrap_or_else(|| LoadoutView {
        name: local_user.unwrap_or_default().to_owned(),
        platform: parser.local_platform().label().to_owned(),
        is_local: true,
        ..LoadoutView::default()
    });
    // Whoever hosts changes from squad to squad, so it is taken afresh rather than from
    // whenever our loadout was captured.
    own.is_host = parser
        .peers()
        .iter()
        .any(|peer| Some(peer.name.as_str()) == local_user && peer.is_host);
    // `peers` holds the `PeerView` built from this same `parser.peers()`, in that order, so
    // zipping them lands each member's card on its own resolved location.
    let squad = parser
        .peers()
        .iter()
        .zip(peers)
        .filter(|(peer, _)| local_user != Some(peer.name.as_str()))
        .map(|(peer, view)| {
            let captured = updates.get(&peer.name).or_else(|| {
                peer.loadout_bytes
                    .and_then(|bytes| members.get(&(peer.name.clone(), bytes)))
            });
            LoadoutView {
                name: peer.name.clone(),
                platform: peer.platform.label().to_owned(),
                is_local: false,
                is_host: peer.is_host,
                country: view.country.clone(),
                country_code: view.country_code.clone(),
                region: view.region.clone(),
                loadout: captured.map(|(loadout, _)| loadout.clone()),
                json: captured.map_or_else(RawJson::default, |(_, json)| json.clone()),
            }
        });
    std::iter::once(own).chain(squad).collect()
}

/// The game builds squad members' loadouts locally as well, and each build leaves a version
/// of theirs in memory. The worker is told to take note of those as soon as EE.log mentions
/// one, so that it never mistakes a member's version for a freshly built one of ours.
fn others_note(parser: &LogParser, pid: u32, seen_builds: &mut u64) -> Option<Job> {
    (parser.other_builds() != *seen_builds).then(|| {
        *seen_builds = parser.other_builds();
        Job::Others { pid }
    })
}

fn find_process_window_rect(process_id: u32) -> Option<WindowRect> {
    struct SearchState {
        process_id: u32,
        window: HWND,
    }

    unsafe extern "system" fn find_window(window: HWND, state: LPARAM) -> i32 {
        let state = unsafe { &mut *(state as *mut SearchState) };
        let mut window_process_id = 0;
        unsafe { GetWindowThreadProcessId(window, &mut window_process_id) };
        if window_process_id == state.process_id && unsafe { IsWindowVisible(window) } != 0 {
            state.window = window;
            return 0;
        }
        1
    }

    let mut state = SearchState {
        process_id,
        window: std::ptr::null_mut(),
    };
    unsafe { EnumWindows(Some(find_window), &mut state as *mut SearchState as LPARAM) };
    if state.window.is_null() {
        return None;
    }

    let mut client_rect = RECT::default();
    let mut origin = POINT::default();
    if unsafe { GetClientRect(state.window, &mut client_rect) } == 0
        || unsafe { ClientToScreen(state.window, &mut origin) } == 0
    {
        return None;
    }

    let width = client_rect.right - client_rect.left;
    let height = client_rect.bottom - client_rect.top;
    (width > 0 && height > 0).then_some(WindowRect {
        left: origin.x,
        bottom: origin.y + height,
        width,
        height,
    })
}

fn find_log_path() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Warframe")
        .join("EE.log")
}

fn read_appended_lines(
    path: &PathBuf,
    position: &mut u64,
    pending: &mut String,
    parser: &mut LogParser,
    missions: &mut MissionTracker,
) -> std::io::Result<()> {
    let length = fs::metadata(path)?.len();
    if length < *position {
        *position = 0;
        pending.clear();
        parser.clear();
        missions.restart();
    }
    if length == *position {
        return Ok(());
    }

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(*position))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    *position += bytes.len() as u64;
    pending.push_str(&String::from_utf8_lossy(&bytes));

    let complete_length = pending.rfind('\n').map(|index| index + 1).unwrap_or(0);
    if complete_length == 0 {
        return Ok(());
    }
    let complete = pending[..complete_length].to_owned();
    pending.drain(..complete_length);
    for line in complete.lines() {
        let line = line.trim_end_matches('\r');
        let squad_changed = parser.process_line(line);
        // Line by line rather than once a pass, so that the whole log replayed at start
        // ties each member as it went, not only as it ended.
        if missions.process_line(line) || squad_changed {
            let local_user = parser.local_user();
            missions.tie_squad(
                parser
                    .peers()
                    .iter()
                    .map(|peer| peer.name.as_str())
                    .filter(|name| Some(*name) != local_user),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn tails_complete_lines_and_survives_partial_writes() {
        // The thread name contains `::`, which Windows rejects in a file name.
        let path = env::temp_dir().join(format!(
            "warframe-peer-overlay-tail-{}.log",
            std::process::id()
        ));
        let mut file = File::create(&path).unwrap();
        write!(file, "Net [Info]: AddSquadMember: A, mm=a, squadCount=2").unwrap();
        file.flush().unwrap();

        let mut position = 0;
        let mut pending = String::new();
        let mut parser = LogParser::default();
        let mut missions = MissionTracker::default();
        read_appended_lines(
            &path,
            &mut position,
            &mut pending,
            &mut parser,
            &mut missions,
        )
        .unwrap();
        assert!(parser.peers().is_empty());

        writeln!(file).unwrap();
        file.flush().unwrap();
        read_appended_lines(
            &path,
            &mut position,
            &mut pending,
            &mut parser,
            &mut missions,
        )
        .unwrap();
        assert_eq!(parser.peers()[0].name, "A");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn asks_for_each_announced_loadout_once_and_never_our_own() {
        let peer = |name: &str, loadout_bytes| Peer {
            name: name.to_owned(),
            loadout_bytes,
            ..Peer::default()
        };
        let mut requested = HashSet::new();
        let peers = [
            peer("LocalTenno", Some(100)),
            peer("Tenno", Some(200)),
            peer("Lotus", None),
        ];

        let first = capture_requests(&peers, Some("LocalTenno"), 42, &mut requested);
        assert_eq!(
            first
                .iter()
                .map(|request| (request.name.as_str(), request.bytes, request.pid))
                .collect::<Vec<_>>(),
            [("Tenno", 200, 42)]
        );
        assert!(capture_requests(&peers, Some("LocalTenno"), 42, &mut requested).is_empty());

        // Re-joining with another loadout is a new announcement worth capturing.
        let peers = [peer("Tenno", Some(250))];
        assert_eq!(
            capture_requests(&peers, Some("LocalTenno"), 42, &mut requested).len(),
            1
        );
    }

    #[test]
    fn asks_after_every_member_whenever_a_loadout_is_sent_again() {
        let mut parser = LogParser::default();
        let mut seen = 0;
        let names = |requests: Vec<UpdateRequest>| {
            requests
                .into_iter()
                .map(|request| request.name)
                .collect::<Vec<_>>()
        };
        for line in [
            "1 Sys [Info]: Logged in LocalTenno",
            "2 Net [Info]: AddSquadMember: Tenno\u{e000}, mm=a, squadCount=1",
            "3 Net [Info]: AddSquadMember: LocalTenno\u{e000}, mm=local, squadCount=2",
            "4 Net [Info]: AddSquadMember: Lotus\u{e000}, mm=b, squadCount=3",
        ] {
            parser.process_line(line);
        }
        assert!(
            update_requests(&parser, 42, &mut seen).is_empty(),
            "nobody has sent one"
        );

        // Relayed by the host, the line names nobody: everyone but us is looked at, once.
        parser.process_line(
            "5 Game [Info]: HandleSquadMessage from 203.0.113.9:56152 LOADOUT (host: 0)",
        );
        assert_eq!(
            names(update_requests(&parser, 42, &mut seen)),
            ["Tenno", "Lotus"]
        );
        assert!(update_requests(&parser, 42, &mut seen).is_empty());
    }

    #[test]
    fn notes_another_players_rebuild_once() {
        let mut parser = LogParser::default();
        let mut seen = 0;
        parser.process_line("1 Sys [Info]: Logged in LocalTenno");
        assert_eq!(
            others_note(&parser, 42, &mut seen),
            None,
            "nobody has rebuilt anything yet"
        );

        parser.process_line("2 Sys [Info]: BuildLoadOut for RemoteTenno");
        assert_eq!(
            others_note(&parser, 42, &mut seen),
            Some(Job::Others { pid: 42 })
        );
        assert_eq!(
            others_note(&parser, 42, &mut seen),
            None,
            "nothing rebuilt since"
        );

        parser.process_line("3 Sys [Info]: BuildLoadOut for LocalTenno");
        assert_eq!(
            others_note(&parser, 42, &mut seen),
            None,
            "our own rebuild is not somebody else's"
        );
    }

    #[test]
    fn lists_our_loadout_first_then_each_member_still_in_the_squad() {
        fn cards(views: &[LoadoutView]) -> Vec<(&str, bool, bool)> {
            views
                .iter()
                .map(|view| (view.name.as_str(), view.is_local, view.loadout.is_some()))
                .collect()
        }

        // What `run` hands over: one `PeerView` per parser peer, in that order, carrying
        // whatever the geo lookup resolved for it.
        fn peer_views(parser: &LogParser) -> Vec<PeerView> {
            parser
                .peers()
                .iter()
                .map(|peer| PeerView {
                    name: peer.name.clone(),
                    country: "Japan".to_owned(),
                    country_code: "JP".to_owned(),
                    region: "Tokyo".to_owned(),
                    ..PeerView::default()
                })
                .collect()
        }

        let mut parser = LogParser::default();
        for line in [
            "1 Sys [Info]: Logged in LocalTenno",
            "2 Net [Info]: AddSquadMember: LocalTenno\u{e000}, mm=local, squadCount=1",
            "3 Net [Info]: MatchingServiceWeb::ProcessSquadMessage received JOIN message from Tenno\u{e001}, loadout: 200 bytes",
            "4 Net [Info]: AddSquadMember: Tenno\u{e001}, mm=a, squadCount=2",
            "5 Net [Info]: AddSquadMember: Lotus\u{e000}, mm=b, squadCount=3",
        ] {
            parser.process_line(line);
        }
        let captured = Loadout {
            mastery_rank: Some(5),
            ..Loadout::default()
        };
        let json = RawJson::from(br#"{"PlayerLevel":5}"#.as_slice());
        // Lotus announced no loadout, so one captured under their name is not theirs now.
        let members = HashMap::from([
            (("Tenno".to_owned(), 200), (captured.clone(), json.clone())),
            (("Lotus".to_owned(), 300), (captured.clone(), json)),
        ]);

        let no_updates = HashMap::new();
        let views = loadout_views(&parser, &peer_views(&parser), None, &members, &no_updates);
        assert_eq!(
            cards(&views),
            [
                ("LocalTenno", true, false),
                ("Tenno", false, true),
                ("Lotus", false, false)
            ]
        );
        // A loadout changed since joining stands in for the announced one, and a member who
        // announced none is shown theirs all the same.
        let changed = Loadout {
            mastery_rank: Some(6),
            ..Loadout::default()
        };
        let updates = HashMap::from([
            ("Tenno".to_owned(), (changed.clone(), RawJson::default())),
            ("Lotus".to_owned(), (changed, RawJson::default())),
        ]);
        let updated = loadout_views(&parser, &peer_views(&parser), None, &members, &updates);
        assert_eq!(
            updated
                .iter()
                .map(|view| view
                    .loadout
                    .as_ref()
                    .and_then(|loadout| loadout.mastery_rank))
                .collect::<Vec<_>>(),
            [None, Some(6), Some(6)]
        );
        assert_eq!(views[0].platform, "PC");
        // The whole of a member's loadout rides along, for a window to hand out.
        assert!(!views[1].json.is_empty());
        assert!(views[2].json.is_empty(), "nothing captured for Lotus");
        // Nobody else's IP answers to the host address, so the squad connects through us.
        assert_eq!(
            views.iter().map(|view| view.is_host).collect::<Vec<_>>(),
            [true, false, false]
        );
        // A member's card carries where they connect from; ours has nothing to show.
        assert_eq!(
            (views[0].region.as_str(), views[0].country.as_str()),
            ("", "")
        );
        assert_eq!(
            (
                views[1].region.as_str(),
                views[1].country.as_str(),
                views[1].country_code.as_str()
            ),
            ("Tokyo", "Japan", "JP")
        );

        // Ours stays on top once captured; a member who leaves takes their card along.
        parser.process_line("6 Net [Info]: RemoveSquadMember: Tenno\u{e001} has been removed");
        let own = LoadoutView {
            name: "LocalTenno".to_owned(),
            platform: "PC".to_owned(),
            is_local: true,
            loadout: Some(captured),
            ..LoadoutView::default()
        };
        assert_eq!(
            cards(&loadout_views(
                &parser,
                &peer_views(&parser),
                Some(&own),
                &members,
                &no_updates
            )),
            [("LocalTenno", true, true), ("Lotus", false, false)]
        );
    }

    #[test]
    fn asks_for_our_own_loadout_after_each_rebuild() {
        let mut parser = LogParser::default();
        let mut seen = 0;
        parser.process_line("1 Sys [Info]: BuildLoadOut for LocalTenno");
        assert_eq!(
            own_request(&parser, 42, &mut seen),
            None,
            "not logged in yet"
        );

        parser.process_line("2 Sys [Info]: Logged in LocalTenno");
        parser.process_line("3 Sys [Info]: BuildLoadOut for LocalTenno");
        let request = own_request(&parser, 42, &mut seen).unwrap();
        assert_eq!((request.pid, request.name.as_str()), (42, "LocalTenno"));
        assert_eq!(
            own_request(&parser, 42, &mut seen),
            None,
            "nothing rebuilt since"
        );

        parser.process_line("4 Sys [Info]: BuildLoadOut for LocalTenno");
        assert!(own_request(&parser, 42, &mut seen).is_some());
    }
}

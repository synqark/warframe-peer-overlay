use std::{
    collections::{HashMap, HashSet},
    env,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

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
    loadout::{self, CaptureRequest, Captured, Job, Loadout, OwnRequest},
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

/// One card of the loadout window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadoutView {
    pub name: String,
    pub platform: String,
    pub is_local: bool,
    /// `None` until captured, which for some members never happens (see README).
    pub loadout: Option<Loadout>,
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
    let mut geo = GeoResolver::new(geo_enabled);
    let mut geo_results = HashMap::<String, GeoInfo>::new();
    let mut geo_failures = HashSet::<String>::new();
    let (loadouts, captures) = loadout::spawn();
    let mut requested_loadouts = HashSet::<(String, usize)>::new();
    // By name and announced size, so a member who re-joins with other gear is not shown the
    // old one while the new one is looked for.
    let mut member_loadouts = HashMap::<(String, usize), Loadout>::new();
    let mut own_loadout: Option<LoadoutView> = None;
    let mut seen_own_builds = 0;
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
            geo_failures.clear();
            requested_loadouts.clear();
            member_loadouts.clear();
            file_position = 0;
            pending.clear();
        }
        was_running = running;

        let status = if !running {
            "Warframeを待機中".to_owned()
        } else if !log_path.exists() {
            "EE.logが見つかりません".to_owned()
        } else {
            match read_appended_lines(&log_path, &mut file_position, &mut pending, &mut parser) {
                Ok(()) => "EE.logを監視中".to_owned(),
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
            if let Some(request) = own_request(&parser, pid, &mut seen_own_builds) {
                let _ = loadouts.send(Job::Own(request));
            }
        }
        for captured in captures.try_iter() {
            match captured {
                Captured::Member {
                    name,
                    bytes,
                    loadout,
                } => {
                    member_loadouts.insert((name, bytes), loadout);
                }
                Captured::Own {
                    name,
                    platform,
                    loadout,
                } => {
                    own_loadout = Some(LoadoutView {
                        name,
                        platform,
                        is_local: true,
                        loadout: Some(loadout),
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
        let loadout_cards = loadout_views(&parser, own_loadout.as_ref(), &member_loadouts);
        let signature = format!("{running}:{status}:{peers:?}:{window_rect:?}:{loadout_cards:?}");
        if signature != last_signature {
            if sender
                .send(MonitorSnapshot {
                    warframe_running: running,
                    log_path: log_path.clone(),
                    status,
                    peers,
                    window_rect,
                    loadouts: loadout_cards,
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

/// The loadout window's cards: ours always first, then every member still in the squad, in
/// the order they joined. A member whose loadout is not captured still gets a card.
fn loadout_views(
    parser: &LogParser,
    own: Option<&LoadoutView>,
    members: &HashMap<(String, usize), Loadout>,
) -> Vec<LoadoutView> {
    let local_user = parser.local_user();
    let own = own.cloned().unwrap_or_else(|| LoadoutView {
        name: local_user.unwrap_or_default().to_owned(),
        platform: parser.local_platform().label().to_owned(),
        is_local: true,
        loadout: None,
    });
    let squad = parser
        .peers()
        .iter()
        .filter(|peer| local_user != Some(peer.name.as_str()))
        .map(|peer| LoadoutView {
            name: peer.name.clone(),
            platform: peer.platform.label().to_owned(),
            is_local: false,
            loadout: peer
                .loadout_bytes
                .and_then(|bytes| members.get(&(peer.name.clone(), bytes)))
                .cloned(),
        });
    std::iter::once(own).chain(squad).collect()
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
) -> std::io::Result<()> {
    let length = fs::metadata(path)?.len();
    if length < *position {
        *position = 0;
        pending.clear();
        parser.clear();
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
        parser.process_line(line.trim_end_matches('\r'));
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
        read_appended_lines(&path, &mut position, &mut pending, &mut parser).unwrap();
        assert!(parser.peers().is_empty());

        writeln!(file).unwrap();
        file.flush().unwrap();
        read_appended_lines(&path, &mut position, &mut pending, &mut parser).unwrap();
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
    fn lists_our_loadout_first_then_each_member_still_in_the_squad() {
        fn cards(views: &[LoadoutView]) -> Vec<(&str, bool, bool)> {
            views
                .iter()
                .map(|view| (view.name.as_str(), view.is_local, view.loadout.is_some()))
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
        // Lotus announced no loadout, so one captured under their name is not theirs now.
        let members = HashMap::from([
            (("Tenno".to_owned(), 200), captured.clone()),
            (("Lotus".to_owned(), 300), captured.clone()),
        ]);

        let views = loadout_views(&parser, None, &members);
        assert_eq!(
            cards(&views),
            [
                ("LocalTenno", true, false),
                ("Tenno", false, true),
                ("Lotus", false, false)
            ]
        );
        assert_eq!(views[0].platform, "PC");

        // Ours stays on top once captured; a member who leaves takes their card along.
        parser.process_line("6 Net [Info]: RemoveSquadMember: Tenno\u{e001} has been removed");
        let own = LoadoutView {
            name: "LocalTenno".to_owned(),
            platform: "PC".to_owned(),
            is_local: true,
            loadout: Some(captured),
        };
        assert_eq!(
            cards(&loadout_views(&parser, Some(&own), &members)),
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

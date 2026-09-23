//! Which missions the game has loaded, whether each has ended since, and which of them each
//! squad member is tied to, read from EE.log.
//!
//! Loading a mission logs a line ending in `with MissionInfo:` — as the host
//! (`ThemedSquadOverlay.lua: Host loading {...}`, or `EidolonMP.lua: Host entering plains`
//! for an open world entered from its hub) or as a client (`Client loaded {...}`) — the `{...}`
//! being JSON that names the node (`"name":"SolNode253_Hard"`). A block of the mission's
//! details follows on lines of their own, with no timestamp: laid out as Lua by a host
//! (`    missionType=MT_DESCENT`), as JSON by a client (`    "missionType" : "MT_EXTERMINATION",`).
//! Either way the mission's own fields are indented by four spaces, anything nested deeper,
//! and the block closes with a `}` at the start of a line. Only missions at a node are kept,
//! a location with `Node` in it (`SolNode228`, `CrewBattleNode501`): hubs (`CetusHub4`,
//! relays) load the same way and are let go.
//!
//! A session ends with `EOM missionLocationUnlocked=<n>` ("end of mission"), which a host
//! logs even on aborting. A client aborting logs `TopMenu.lua: Abort: client/session/PVP` and
//! no EOM, so an abort is taken as the end as well; whichever comes first stands.
//!
//! Like `LogParser`, this is a pure state machine over the log's lines: no I/O.

use std::collections::{HashMap, HashSet, VecDeque};

/// How many missions are kept, the oldest letting go first.
const RECENT: usize = 10;

/// A mission as its load and its end were logged. Its type and location are the game's own
/// names (`MT_DESCENT`, `SolNode253`), not what the game shows the player.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mission {
    /// When it was loaded, by the log's clock: seconds since the game started, as written.
    pub loaded_at: String,
    /// Whether we loaded it as the host rather than as a client.
    pub host: bool,
    /// The node the load line names, `SolNode253_Hard`; empty where it names none, as it does
    /// for an open world entered from its hub. A client entering an open world is handed the
    /// hub's name here (`CetusHub4_HUB`), so `location` is what tells where the mission is.
    pub node: String,
    pub mission_type: String,
    /// Always a node (`is_node`).
    pub location: String,
    /// `None` while the session goes on.
    pub ended: Option<Ended>,
}

/// How a session's end was logged, and when, by the log's clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    pub by: Ending,
    pub at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    Eom,
    Abort,
}

#[derive(Default)]
pub struct MissionTracker {
    /// How many sessions have ended, and how many of those the monitor has been told of;
    /// `None` until it has caught up with a log read from its start (`sessions_ended`).
    endings: u64,
    told: Option<u64>,
    /// The newest last.
    missions: VecDeque<Mission>,
    /// A mission whose block of details is still being read; kept once the block shows its
    /// location is a node.
    loading: Option<Mission>,
    /// Who is in the squad, bar ourselves, as last told (`tie_squad`).
    squad: HashSet<String>,
    /// The mission each member is tied to, if any.
    ties: HashMap<String, Mission>,
}

impl MissionTracker {
    /// The missions seen so far, the newest last.
    pub fn missions(&self) -> &VecDeque<Mission> {
        &self.missions
    }

    /// Starts over with a log that has started over, the game having been restarted. The ties
    /// are kept: the squad the old log left behind leaves with it, and the history has yet to
    /// write them down. Everyone the new log shows in a squad counts as joining afresh.
    pub fn restart(&mut self) {
        self.missions.clear();
        self.loading = None;
        self.squad.clear();
        self.told = None;
    }

    /// Whether a session has ended since this was last asked, for the monitor to write down
    /// everyone it tied. The first time it is asked after the log has been read from its
    /// start — a run beginning, or the log rotating — nothing is reported and the ties to
    /// sessions already ended are let go: those were written down, if at all, by the run that
    /// watched them end.
    pub fn sessions_ended(&mut self) -> bool {
        let Some(told) = self.told.replace(self.endings) else {
            // Caught up: every tie but one to the session still going on is let go.
            let ongoing = self.ongoing().cloned();
            self.ties.retain(|_, tie| Some(&*tie) == ongoing.as_ref());
            return false;
        };
        self.endings > told
    }

    /// Lets a member's tie go, once the history has written them down with it.
    pub fn untie(&mut self, member: &str) {
        self.ties.remove(member);
    }

    /// The newest mission, while its session goes on.
    pub fn ongoing(&self) -> Option<&Mission> {
        self.missions
            .back()
            .filter(|mission| mission.ended.is_none())
    }

    /// Takes in a line of the log, saying whether what is known of the missions changed.
    pub fn process_line(&mut self, line: &str) -> bool {
        let mut changed = false;
        if self.loading.is_some() {
            if line.starts_with('}') {
                return self.finish_loading();
            }
            // A block's lines carry no timestamp, so one that does means the block is over,
            // however it ended.
            if !line.starts_with(|c: char| c.is_ascii_digit()) {
                self.take_field(line);
                return false;
            }
            changed = self.finish_loading();
        }

        let (time, rest) = line.split_once(' ').unwrap_or_default();
        // Some of these lines end in a space, some do not.
        if let Some(head) = rest.trim_end().strip_suffix(" with MissionInfo:") {
            self.loading = Some(Mission {
                loaded_at: time.to_owned(),
                host: head.contains("Host loading") || head.contains("Host entering"),
                node: node_name(head),
                ..Mission::default()
            });
            return changed;
        }

        let by = if rest.contains("]: EOM missionLocationUnlocked=") {
            Ending::Eom
        } else if rest.contains("TopMenu.lua: Abort:") {
            Ending::Abort
        } else {
            return changed;
        };
        if let Some(mission) = self.missions.back_mut()
            && mission.ended.is_none()
        {
            mission.ended = Some(Ended {
                by,
                at: time.to_owned(),
            });
            self.endings += 1;
            changed = true;
        }
        changed
    }

    /// Keeps the mission whose block has just been read, if it is at a node. Says whether it
    /// was kept.
    fn finish_loading(&mut self) -> bool {
        let Some(mission) = self.loading.take() else {
            return false;
        };
        if !is_node(&mission.location) {
            return false;
        }
        self.missions.push_back(mission);
        if self.missions.len() > RECENT {
            self.missions.pop_front();
        }
        true
    }

    /// Fills in the loading mission's type or location from a line of its block, the first
    /// time the block gives either.
    fn take_field(&mut self, line: &str) {
        let (Some((key, value)), Some(mission)) = (field(line), self.loading.as_mut()) else {
            return;
        };
        let slot = match key {
            "missionType" => &mut mission.mission_type,
            "location" => &mut mission.location,
            _ => return,
        };
        if slot.is_empty() {
            *slot = value.to_owned();
        }
    }

    /// Ties the squad, bar ourselves, to the mission going on, to be told after every line
    /// that changed the squad or the missions. Whoever is in the squad while a session goes
    /// on is tied to its mission, and stays tied after it ends until another session ties
    /// them afresh. Whoever joins while none goes on starts untied, since they may leave
    /// again before any mission starts; and whoever joins again after leaving starts afresh.
    pub fn tie_squad<'a>(&mut self, squad: impl IntoIterator<Item = &'a str>) {
        let squad = squad.into_iter().map(str::to_owned).collect::<HashSet<_>>();
        for joined in squad.difference(&self.squad) {
            self.ties.remove(joined);
        }
        if let Some(mission) = self.ongoing() {
            let mission = mission.clone();
            for member in &squad {
                if self.ties.get(member) != Some(&mission) {
                    self.ties.insert(member.clone(), mission.clone());
                }
            }
        }
        self.squad = squad;
    }

    /// The mission a member is tied to, if any: still theirs after they leave, for the
    /// history to write down with them.
    pub fn tie(&self, member: &str) -> Option<&Mission> {
        self.ties.get(member)
    }

    /// Everyone in the squad, bar ourselves, by name, with the mission each is tied to.
    pub fn squad_ties(&self) -> Vec<(String, Option<Mission>)> {
        let mut squad = self
            .squad
            .iter()
            .map(|member| (member.clone(), self.ties.get(member).cloned()))
            .collect::<Vec<_>>();
        squad.sort_by(|(one, _), (other, _)| one.cmp(other));
        squad
    }
}

/// A node of the star chart, where every mission worth tying a player to is played: a
/// location with `Node` in it, as `SolNode228`, `CrewBattleNode501` or `EventNode12` are and
/// hubs (`CetusHub4`, `MercuryHUB`) are not.
fn is_node(location: &str) -> bool {
    location.contains("Node")
}

/// One of the mission's own fields from a line of its block, as `(key, value)`, in either
/// layout: `    key=value` or `    "key" : "value",`. Lines nested deeper are not its own.
fn field(line: &str) -> Option<(&str, &str)> {
    let inner = line.strip_prefix("    ")?;
    if inner.starts_with(char::is_whitespace) {
        return None;
    }
    if let Some(quoted) = inner.strip_prefix('"') {
        let (key, rest) = quoted.split_once('"')?;
        let value = rest.trim_start().strip_prefix(':')?.trim();
        let value = value.strip_suffix(',').unwrap_or(value);
        return Some((key, value.trim_matches('"')));
    }
    let (key, value) = inner.split_once('=')?;
    Some((key.trim(), value.trim()))
}

/// The node a load line's JSON names (`"name"`), or nothing where it has none.
fn node_name(head: &str) -> String {
    let (Some(start), Some(end)) = (head.find('{'), head.rfind('}')) else {
        return String::new();
    };
    serde_json::from_str::<serde_json::Value>(&head[start..=end])
        .ok()
        .and_then(|json| json.get("name")?.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(tracker: &mut MissionTracker, lines: &str) {
        for line in lines.lines() {
            tracker.process_line(line);
        }
    }

    const HOST: &str = r#"51.956 Script [Info]: ThemedSquadOverlay.lua: Host loading {"name":"SolNode253_Hard","difficulty":1} with MissionInfo:
info={
    missionType=MT_DESCENT
    faction=FC_OROKIN
    location=SolNode253
    levelAuras={
        /Lotus/Upgrades/Mods/DirectorMods/HardModeLevelAura
    }
}

51.958 Script [Info]: ThemedSquadOverlay.lua: Lobby::Host_StartMatch: launching level for SolNode253_Hard"#;

    const CLIENT: &str = r#"2057.807 Sys [Info]: Client loaded {"difficulty":"","name":"SolNode6_InvasionDefender","quest":""} with MissionInfo:
{
    "missionType" : "MT_EXTERMINATION",
    "faction" : "FC_INFESTATION",
    "missionReward" : {
        "credits" : 0,
        "location" : "Nested",
        "items" : []
    },
    "location" : "SolNode6",
    "levelOverride" : "/Lotus/Levels/Proc/Corpus/CorpusOutpostExterminate"
}

2057.807 Script [Info]: LotusGameRules.lua: Adding static medal valuation..."#;

    const HUB: &str = r#"2675.363 Script [Info]: ThemedSquadOverlay.lua: Host loading {"difficulty":0.5,"name":"CetusHub4_HUB"} with MissionInfo:
info={
    missionType=MT_PVP
    location=CetusHub4
}"#;

    const EOM: &str = "1882.148 Sys [Info]: EOM missionLocationUnlocked=1";

    #[test]
    fn reads_a_mission_the_host_loaded() {
        let mut tracker = MissionTracker::default();
        feed(&mut tracker, HOST);

        assert_eq!(
            tracker.ongoing(),
            Some(&Mission {
                loaded_at: "51.956".to_owned(),
                host: true,
                node: "SolNode253_Hard".to_owned(),
                mission_type: "MT_DESCENT".to_owned(),
                location: "SolNode253".to_owned(),
                ended: None,
            })
        );
    }

    #[test]
    fn reads_a_mission_loaded_as_a_client_and_skips_what_is_nested() {
        let mut tracker = MissionTracker::default();
        feed(&mut tracker, CLIENT);

        let mission = tracker.ongoing().unwrap();
        assert!(!mission.host);
        assert_eq!(mission.node, "SolNode6_InvasionDefender");
        assert_eq!(mission.mission_type, "MT_EXTERMINATION");
        assert_eq!(mission.location, "SolNode6", "not the nested one");
    }

    #[test]
    fn reads_an_open_world_entered_from_its_hub() {
        let mut tracker = MissionTracker::default();
        feed(
            &mut tracker,
            "82311.305 Script [Info]: EidolonMP.lua: Host entering plains with MissionInfo: \n\
             info={\n    missionType=MT_LANDSCAPE\n    location=SolNode228\n}",
        );

        let mission = tracker.ongoing().unwrap();
        assert!(mission.host);
        assert_eq!(mission.node, "", "the line names no node");
        assert_eq!(
            (mission.mission_type.as_str(), mission.location.as_str()),
            ("MT_LANDSCAPE", "SolNode228")
        );
    }

    #[test]
    fn lets_a_hub_go() {
        let mut tracker = MissionTracker::default();
        feed(&mut tracker, HOST);
        tracker.process_line(EOM);
        feed(&mut tracker, HUB);
        tracker.process_line("2680.000 Sys [Info]: after the hub's block");

        assert_eq!(tracker.missions().len(), 1);
        assert_eq!(tracker.missions()[0].location, "SolNode253");
        assert_eq!(tracker.ongoing(), None, "the hub is no session");
    }

    #[test]
    fn ends_a_session_at_its_eom_or_an_abort() {
        let mut tracker = MissionTracker::default();
        feed(&mut tracker, HOST);
        assert!(!tracker.process_line("60.000 Script [Info]: ThemedMainMenu.lua: EOM BLOCKED"));
        assert!(
            tracker.process_line("1882.100 Script [Info]: TopMenu.lua: Abort: host/no session")
        );
        assert!(!tracker.process_line(EOM), "the first end stands");
        feed(&mut tracker, CLIENT);
        assert!(
            tracker.process_line("2100.000 Script [Info]: TopMenu.lua: Abort: client/session/PVP")
        );

        let missions = tracker.missions();
        let ended = |index: usize| missions[index].ended.clone().unwrap();
        assert_eq!(ended(0).by, Ending::Abort);
        assert_eq!(ended(0).at, "1882.100");
        assert_eq!(ended(1).by, Ending::Abort, "a client's abort logs no EOM");
        assert_eq!(tracker.ongoing(), None);
    }

    #[test]
    fn keeps_only_the_recent_missions() {
        let mut tracker = MissionTracker::default();
        for index in 0..RECENT + 3 {
            feed(
                &mut tracker,
                &format!(
                    "{index}.000 Sys [Info]: Client loaded {{\"name\":\"SolNode{index}\"}} with MissionInfo:\n\
                     {{\n    \"location\" : \"SolNode{index}\",\n}}"
                ),
            );
        }

        let missions = tracker.missions();
        assert_eq!(missions.len(), RECENT);
        assert_eq!(missions[0].location, "SolNode3", "the oldest three let go");
    }

    #[test]
    fn ignores_an_end_with_no_mission_before_it() {
        let mut tracker = MissionTracker::default();
        assert!(!tracker.process_line(EOM));
        assert!(tracker.missions().is_empty());
    }

    #[test]
    fn tells_a_node_of_the_star_chart() {
        for location in ["SolNode6", "SolNode851", "CrewBattleNode501", "EventNode12"] {
            assert!(is_node(location), "{location}");
        }
        for location in ["CetusHub4", "MercuryHUB", "1999Hub", "ZarimanHub", ""] {
            assert!(!is_node(location), "{location}");
        }
    }

    #[test]
    fn ties_the_squad_to_the_session_they_are_in() {
        let mut tracker = MissionTracker::default();
        let location = |tracker: &MissionTracker, member: &str| {
            tracker.tie(member).map(|mission| mission.location.clone())
        };

        // Joined with no session going on: untied, however they leave.
        tracker.tie_squad(["Ordis"]);
        assert_eq!(location(&tracker, "Ordis"), None);

        // A session starts with them in the squad.
        feed(&mut tracker, HOST);
        tracker.tie_squad(["Ordis"]);
        assert_eq!(location(&tracker, "Ordis").as_deref(), Some("SolNode253"));

        // Joining during it ties at once.
        tracker.tie_squad(["Ordis", "Lotus"]);
        assert_eq!(location(&tracker, "Lotus").as_deref(), Some("SolNode253"));

        // The session ends: both stay tied; one who joins now is not.
        tracker.process_line(EOM);
        tracker.tie_squad(["Ordis", "Lotus", "Teshin"]);
        assert_eq!(location(&tracker, "Lotus").as_deref(), Some("SolNode253"));
        assert_eq!(location(&tracker, "Teshin"), None);

        // Lotus leaves: still tied, for the history.
        tracker.tie_squad(["Ordis", "Teshin"]);
        assert_eq!(location(&tracker, "Lotus").as_deref(), Some("SolNode253"));

        // Another session ties the rest afresh.
        feed(&mut tracker, CLIENT);
        tracker.tie_squad(["Ordis", "Teshin"]);
        assert_eq!(location(&tracker, "Ordis").as_deref(), Some("SolNode6"));
        assert_eq!(location(&tracker, "Teshin").as_deref(), Some("SolNode6"));
        assert_eq!(location(&tracker, "Lotus").as_deref(), Some("SolNode253"));

        // Lotus comes back after it ends: nothing carried over from before.
        tracker.process_line("2200.000 Sys [Info]: EOM missionLocationUnlocked=1");
        tracker.tie_squad(["Ordis", "Teshin", "Lotus"]);
        assert_eq!(location(&tracker, "Lotus"), None);

        assert_eq!(
            tracker
                .squad_ties()
                .into_iter()
                .map(|(member, tie)| (member, tie.map(|mission| mission.location)))
                .collect::<Vec<_>>(),
            [
                ("Lotus".to_owned(), None),
                ("Ordis".to_owned(), Some("SolNode6".to_owned())),
                ("Teshin".to_owned(), Some("SolNode6".to_owned())),
            ]
        );
    }

    #[test]
    fn tells_of_a_session_ending_once_it_has_caught_up_with_the_log() {
        let mut tracker = MissionTracker::default();
        feed(&mut tracker, HOST);
        tracker.tie_squad(["Ordis"]);
        tracker.process_line(EOM);

        assert!(
            !tracker.sessions_ended(),
            "the log was read from its start, and that end is not ours to write down"
        );
        assert_eq!(
            tracker.tie("Ordis"),
            None,
            "nor is the tie to it, which the run that watched it end wrote down"
        );

        feed(&mut tracker, CLIENT);
        tracker.tie_squad(["Ordis"]);
        assert!(!tracker.sessions_ended(), "this session goes on");
        tracker.process_line("2200.000 Sys [Info]: EOM missionLocationUnlocked=1");

        assert!(tracker.sessions_ended(), "and now it has ended");
        assert!(!tracker.sessions_ended(), "told of once");
        assert_eq!(
            tracker
                .tie("Ordis")
                .map(|mission| mission.location.as_str()),
            Some("SolNode6"),
            "still tied, for the history to write them down"
        );
        tracker.untie("Ordis");
        assert_eq!(tracker.tie("Ordis"), None, "and untied once it has");
    }

    #[test]
    fn keeps_a_tie_to_the_session_a_replay_leaves_going_on() {
        let mut tracker = MissionTracker::default();
        feed(&mut tracker, HOST);
        tracker.tie_squad(["Ordis"]);

        assert!(!tracker.sessions_ended());
        assert!(
            tracker.tie("Ordis").is_some(),
            "the session the replay ends inside of is ours to write down"
        );
    }

    #[test]
    fn keeps_the_ties_of_a_squad_the_game_closed_on() {
        let mut tracker = MissionTracker::default();
        feed(&mut tracker, HOST);
        tracker.tie_squad(["Ordis"]);

        tracker.restart();

        assert!(tracker.missions().is_empty());
        assert_eq!(
            tracker
                .tie("Ordis")
                .map(|mission| mission.location.as_str()),
            Some("SolNode253"),
            "for the history to write down"
        );
        // Found in a squad again by the new log: joining afresh, with no session going on.
        tracker.tie_squad(["Ordis"]);
        assert_eq!(tracker.tie("Ordis"), None);
    }
}

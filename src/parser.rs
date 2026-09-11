use std::collections::HashMap;

use regex::Regex;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub platform: Platform,
    pub matchmaking_id: String,
    pub ip: Option<String>,
    pub is_host: bool,
    /// Size of the loadout EE.log announced when this member joined, used to find it in
    /// the game's memory.
    pub loadout_bytes: Option<usize>,
}

impl Platform {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pc => "PC",
            Self::Xbox => "Xbox",
            Self::Playstation => "PS",
            Self::Switch => "NSW",
            Self::Ios => "iOS",
            Self::Android => "And.",
            Self::Switch2 => "NS2",
            Self::Unknown => "???",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Platform {
    Pc,
    Xbox,
    Playstation,
    Switch,
    Ios,
    Android,
    Switch2,
    #[default]
    Unknown,
}

#[derive(Debug)]
pub struct LogParser {
    login: Regex,
    logout: Regex,
    add_squad: Regex,
    remove_squad: Regex,
    leave_squad: Regex,
    host_address: Regex,
    remote_player: Regex,
    voip_player: Regex,
    squad_join: Regex,
    build_loadout: Regex,

    received_ping: Regex,
    peers: Vec<Peer>,
    matchmaking_ips: HashMap<String, String>,
    ping_ips_by_name: HashMap<String, String>,
    loadout_bytes_by_name: HashMap<String, usize>,
    host_ip: Option<String>,
    local_user: Option<String>,
    local_platform: Platform,
    own_builds: u64,
}

impl Default for LogParser {
    fn default() -> Self {
        Self {
            login: Regex::new(r"Sys \[Info\]: Logged in (.+?)(?: \(|\s*$)")
                .expect("valid login regex"),
            logout: Regex::new(r"Sys \[Info\]: Logout confirmed").expect("valid logout regex"),
            add_squad: Regex::new(r"Net \[Info\]: AddSquadMember: (.+?), mm=(.+?), squadCount=\d+")
                .expect("valid AddSquadMember regex"),
            remove_squad: Regex::new(r"Net \[Info\]: RemoveSquadMember: (.+?) has been removed")
                .expect("valid RemoveSquadMember regex"),
            leave_squad: Regex::new(r"Net \[Info\]: MatchingService::LeaveSquad")
                .expect("valid LeaveSquad regex"),
            host_address: Regex::new(r"Net \[Info\]: Squad host address: ([\d.]+):\d+")
                .expect("valid host address regex"),
            remote_player: Regex::new(
                r"Net \[Info\]: Added remote player \(mm=([^)]+)\), address=([\d.]+):\d+",
            )
            .expect("valid remote player regex"),
            voip_player: Regex::new(
                r"Net \[Info\]: VOIP: Registered remote player (.+?) \(([\d.]+):\d+\)",
            )
            .expect("valid VOIP player regex"),
            squad_join: Regex::new(
                r"Net \[Info\]: MatchingServiceWeb::ProcessSquadMessage received JOIN message from (.+?), loadout: (\d+) bytes",
            )
            .expect("valid squad JOIN regex"),
            build_loadout: Regex::new(r"Sys \[Info\]: BuildLoadOut for (.+?)\s*$")
                .expect("valid BuildLoadOut regex"),
            received_ping: Regex::new(
                r"Net \[Info\]: Received ping from ([\d.]+):\d+ \((.+?) - \d+ms\)",
            )
            .expect("valid received ping regex"),
            peers: Vec::new(),
            matchmaking_ips: HashMap::new(),
            ping_ips_by_name: HashMap::new(),
            loadout_bytes_by_name: HashMap::new(),
            host_ip: None,
            local_user: None,
            local_platform: Platform::Unknown,
            own_builds: 0,
        }
    }
}

impl LogParser {
    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }

    pub fn local_user(&self) -> Option<&str> {
        self.local_user.as_deref()
    }

    /// Our platform, as our own `AddSquadMember` line showed it.
    pub fn local_platform(&self) -> Platform {
        self.local_platform
    }

    /// How many times EE.log has shown our own loadout being rebuilt (`BuildLoadOut`): each
    /// change in the arsenal, leaving it, and loading into a mission.
    pub fn own_builds(&self) -> u64 {
        self.own_builds
    }

    pub fn clear(&mut self) {
        self.peers.clear();
        self.matchmaking_ips.clear();
        self.ping_ips_by_name.clear();
        self.loadout_bytes_by_name.clear();
        self.host_ip = None;
    }

    pub fn process_line(&mut self, line: &str) -> bool {
        if self.logout.is_match(line) {
            self.local_user = None;
            let changed = !self.peers.is_empty();
            self.clear();
            return changed;
        }

        if let Some(captures) = self.login.captures(line) {
            self.local_user = Some(captures[1].trim().to_owned());
            return self.reconcile();
        }

        if self.leave_squad.is_match(line) {
            let changed = !self.peers.is_empty();
            self.clear();
            return changed;
        }

        if let Some(captures) = self.host_address.captures(line) {
            self.host_ip = Some(captures[1].to_owned());
            return self.reconcile();
        }

        if let Some(captures) = self.remote_player.captures(line) {
            self.matchmaking_ips
                .insert(captures[1].to_owned(), captures[2].to_owned());
            return self.reconcile();
        }

        if let Some(captures) = self.voip_player.captures(line) {
            self.matchmaking_ips
                .insert(captures[1].to_owned(), captures[2].to_owned());
            return self.reconcile();
        }

        if let Some(captures) = self.received_ping.captures(line) {
            let (name, _) = parse_player_name(&captures[2]);
            self.ping_ips_by_name.insert(name, captures[1].to_owned());
            return self.reconcile();
        }

        if let Some(captures) = self.squad_join.captures(line) {
            // Our own JOIN line names nobody and carries no loadout.
            let bytes = captures[2].parse::<usize>().unwrap_or(0);
            if bytes == 0 {
                return false;
            }
            let (name, _) = parse_player_name(&captures[1]);
            self.loadout_bytes_by_name.insert(name, bytes);
            return self.reconcile();
        }

        if let Some(captures) = self.build_loadout.captures(line) {
            let (name, _) = parse_player_name(&captures[1]);
            if self.local_user.as_deref() == Some(name.as_str()) {
                self.own_builds += 1;
            }
            return false;
        }

        if let Some(captures) = self.add_squad.captures(line) {
            let (name, platform) = parse_player_name(&captures[1]);
            let matchmaking_id = captures[2].to_owned();
            if self.local_user.as_deref() == Some(name.as_str()) {
                self.local_platform = platform;
            }
            if self.peers.iter().any(|peer| peer.name == name) {
                return false;
            }
            self.peers.push(Peer {
                name,
                platform,
                matchmaking_id,
                ..Peer::default()
            });
            self.reconcile();
            return true;
        }

        if let Some(captures) = self.remove_squad.captures(line) {
            let (name, _) = parse_player_name(&captures[1]);
            let original_len = self.peers.len();
            self.peers.retain(|peer| peer.name != name);
            return original_len != self.peers.len();
        }

        false
    }

    fn reconcile(&mut self) -> bool {
        let mut changed = false;
        let has_remote_host = self.peers.iter().any(|peer| {
            self.matchmaking_ips
                .get(&peer.matchmaking_id)
                .is_some_and(|ip| Some(ip) == self.host_ip.as_ref())
        });
        for peer in &mut self.peers {
            let ip = self
                .matchmaking_ips
                .get(&peer.matchmaking_id)
                .or_else(|| self.ping_ips_by_name.get(&peer.name))
                .cloned();
            let is_host = (ip.is_some() && ip == self.host_ip)
                || (!has_remote_host && self.local_user.as_deref() == Some(peer.name.as_str()));
            let loadout_bytes = self.loadout_bytes_by_name.get(&peer.name).copied();
            if peer.ip != ip || peer.is_host != is_host || peer.loadout_bytes != loadout_bytes {
                peer.ip = ip;
                peer.is_host = is_host;
                peer.loadout_bytes = loadout_bytes;
                changed = true;
            }
        }
        changed
    }
}

fn parse_player_name(raw: &str) -> (String, Platform) {
    let Some(last) = raw.chars().last() else {
        return (String::new(), Platform::Unknown);
    };
    let platform = match last {
        '\u{e000}' => Platform::Pc,
        '\u{e001}' => Platform::Xbox,
        '\u{e002}' => Platform::Playstation,
        '\u{e003}' => Platform::Switch,
        '\u{e004}' => Platform::Ios,
        '\u{e005}' => Platform::Android,
        '\u{e006}' => Platform::Switch2,
        _ => Platform::Unknown,
    };
    if platform == Platform::Unknown {
        (raw.to_owned(), platform)
    } else {
        (raw[..raw.len() - last.len_utf8()].to_owned(), platform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_member_ip_and_host_when_events_arrive_out_of_order() {
        let mut parser = LogParser::default();
        parser.process_line(
            "123 Net [Info]: Added remote player (mm=abc-123), address=203.0.113.7:4950",
        );
        parser.process_line("124 Net [Info]: Squad host address: 203.0.113.7:4950");
        parser.process_line(
            "125 Net [Info]: AddSquadMember: Tenno\u{e001}, mm=abc-123, squadCount=2",
        );

        assert_eq!(
            parser.peers(),
            &[Peer {
                name: "Tenno".to_owned(),
                platform: Platform::Xbox,
                matchmaking_id: "abc-123".to_owned(),
                ip: Some("203.0.113.7".to_owned()),
                is_host: true,
                loadout_bytes: None,
            }]
        );
    }

    #[test]
    fn resolves_voip_mapping_after_member_was_added() {
        let mut parser = LogParser::default();
        parser.process_line("1 Net [Info]: AddSquadMember: Lotus, mm=peer-a, squadCount=2");
        parser.process_line(
            "2 Net [Info]: VOIP: Registered remote player peer-a (198.51.100.4:6000)",
        );

        assert_eq!(parser.peers()[0].ip.as_deref(), Some("198.51.100.4"));
    }

    #[test]
    fn uses_received_ping_mapping_when_member_is_added_later() {
        let mut parser = LogParser::default();
        parser.process_line(
            "331.083 Net [Info]: Received ping from 67.166.126.173:4950 (SieProf\u{e000} - 146ms)",
        );
        parser.process_line(
            "332 Net [Info]: AddSquadMember: SieProf\u{e000}, mm=peer-a, squadCount=2",
        );

        assert_eq!(parser.peers()[0].name, "SieProf");
        assert_eq!(parser.peers()[0].platform, Platform::Pc);
        assert_eq!(parser.peers()[0].ip.as_deref(), Some("67.166.126.173"));
        assert!(!parser.peers()[0].is_host);
    }

    #[test]
    fn formal_mapping_overrides_received_ping_mapping() {
        let mut parser = LogParser::default();
        parser.process_line(
            "1 Net [Info]: Received ping from 198.51.100.3:4950 (Tenno\u{e001} - 40ms)",
        );
        parser.process_line("2 Net [Info]: AddSquadMember: Tenno\u{e001}, mm=peer-a, squadCount=2");
        parser.process_line(
            "3 Net [Info]: Added remote player (mm=peer-a), address=203.0.113.7:4950",
        );

        assert_eq!(parser.peers()[0].ip.as_deref(), Some("203.0.113.7"));
    }

    #[test]
    fn removes_members_and_resets_session() {
        let mut parser = LogParser::default();
        parser.process_line("1 Net [Info]: AddSquadMember: A, mm=a, squadCount=2");
        parser.process_line("2 Net [Info]: RemoveSquadMember: A has been removed");
        assert!(parser.peers().is_empty());

        parser.process_line("3 Net [Info]: AddSquadMember: B, mm=b, squadCount=2");
        parser.process_line("4 Net [Info]: MatchingService::LeaveSquad");
        assert!(parser.peers().is_empty());
    }

    #[test]
    fn falls_back_to_logged_in_player_as_host() {
        let mut parser = LogParser::default();
        parser.process_line("1 Sys [Info]: Logged in LocalTenno");
        parser.process_line("2 Net [Info]: AddSquadMember: LocalTenno, mm=local, squadCount=2");
        parser.process_line("3 Net [Info]: AddSquadMember: RemoteTenno, mm=remote, squadCount=2");

        assert_eq!(parser.local_user(), Some("LocalTenno"));
        assert!(parser.peers()[0].is_host);
        assert!(!parser.peers()[1].is_host);
    }

    #[test]
    fn replaces_local_fallback_when_remote_host_resolves() {
        let mut parser = LogParser::default();
        parser.process_line("1 Sys [Info]: Logged in LocalTenno");
        parser.process_line("2 Net [Info]: AddSquadMember: LocalTenno, mm=local, squadCount=2");
        parser.process_line("3 Net [Info]: AddSquadMember: RemoteTenno, mm=remote, squadCount=2");
        parser.process_line("4 Net [Info]: Squad host address: 8.8.8.8:4950");
        parser.process_line("5 Net [Info]: Added remote player (mm=remote), address=8.8.8.8:4950");

        assert!(!parser.peers()[0].is_host);
        assert!(parser.peers()[1].is_host);
    }

    #[test]
    fn keeps_the_announced_loadout_size_of_each_member() {
        let mut parser = LogParser::default();
        // The JOIN line arrives just before the member is added to the squad.
        parser.process_line(
            "1 Net [Info]: MatchingServiceWeb::ProcessSquadMessage received JOIN message from Tenno\u{e001}, loadout: 30615 bytes",
        );
        parser.process_line("1 Net [Info]: AddSquadMember: Tenno\u{e001}, mm=peer-a, squadCount=2");
        // Our own JOIN line names nobody and carries no loadout.
        parser.process_line(
            "2 Net [Info]: MatchingServiceWeb::ProcessSquadMessage received JOIN message from , loadout: 0 bytes",
        );
        assert_eq!(parser.peers()[0].loadout_bytes, Some(30615));

        parser.process_line("3 Net [Info]: MatchingService::LeaveSquad");
        parser.process_line("4 Net [Info]: AddSquadMember: Tenno\u{e001}, mm=peer-a, squadCount=2");
        assert_eq!(
            parser.peers()[0].loadout_bytes,
            None,
            "a new squad brings a new announcement"
        );
    }

    #[test]
    fn counts_rebuilds_of_our_own_loadout_only() {
        let mut parser = LogParser::default();
        // Before login the game builds a placeholder player's loadout.
        parser.process_line("1 Sys [Info]: BuildLoadOut for Player");
        parser.process_line("2 Sys [Info]: Logged in LocalTenno");
        parser.process_line("3 Sys [Info]: BuildLoadOut for LocalTenno");
        parser.process_line("4 Sys [Info]: BuildLoadOut for RemoteTenno");
        parser.process_line("5 Sys [Info]: BuildLoadOut for LocalTenno");

        assert_eq!(parser.own_builds(), 2);
    }

    #[test]
    fn remembers_our_platform_across_squads() {
        let mut parser = LogParser::default();
        parser.process_line("1 Sys [Info]: Logged in LocalTenno");
        parser.process_line(
            "2 Net [Info]: AddSquadMember: LocalTenno\u{e000}, mm=local, squadCount=1",
        );
        parser.process_line("3 Net [Info]: MatchingService::LeaveSquad");

        assert_eq!(parser.local_platform(), Platform::Pc);
    }
}

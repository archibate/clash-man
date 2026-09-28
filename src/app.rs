use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{
    model::{
        Connection, ConnectionsPayload, LogEntry, MemorySample, ProxyInfo, ProxyPayload, Rule,
        RulesPayload, RuntimeConfig, TrafficSample, VersionInfo,
    },
    subscription::SubscriptionState,
};

const HISTORY_LIMIT: usize = 120;
const LOG_LIMIT: usize = 2_000;
const PAGE_STEP: isize = 10;
const STATUS_LIFETIME: Duration = Duration::from_secs(5);
const ERROR_LIFETIME: Duration = Duration::from_secs(15);
pub const DEFAULT_TEST_URL: &str = "https://www.gstatic.com/generate_204";

/// Proxy types that contain other proxies.
const GROUP_KINDS: [&str; 6] = [
    "selector",
    "urltest",
    "url-test",
    "fallback",
    "loadbalance",
    "relay",
];
/// Built-in outbounds that are not worth latency-testing.
const BUILTIN_KINDS: [&str; 5] = ["direct", "reject", "rejectdrop", "pass", "compatible"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Proxies,
    Connections,
    Rules,
    Logs,
}

impl Page {
    pub const ALL: [Self; 4] = [Self::Proxies, Self::Connections, Self::Rules, Self::Logs];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Proxies => "Proxies",
            Self::Connections => "Connections",
            Self::Rules => "Rules",
            Self::Logs => "Logs",
        }
    }

    fn next(self) -> Self {
        Self::ALL[(self as usize + 1) % Self::ALL.len()]
    }

    fn previous(self) -> Self {
        Self::ALL[(self as usize + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Which half of the Proxies page receives j/k.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Pane {
    #[default]
    Groups,
    Nodes,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NodeSort {
    #[default]
    Config,
    Latency,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConnectionSort {
    #[default]
    Newest,
    Download,
    Upload,
    Host,
}

impl ConnectionSort {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Newest => "newest",
            Self::Download => "download",
            Self::Upload => "upload",
            Self::Host => "host",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Newest => Self::Download,
            Self::Download => Self::Upload,
            Self::Upload => Self::Host,
            Self::Host => Self::Newest,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    Warning,
    Error,
}

impl LogLevel {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "debug" => Self::Debug,
            "warning" | "warn" => Self::Warning,
            "error" | "fatal" => Self::Error,
            _ => Self::Info,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Debug => Self::Info,
            Self::Info => Self::Warning,
            Self::Warning => Self::Error,
            Self::Error => Self::Debug,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    None,
    Help,
    ConfirmCloseAll,
}

#[derive(Debug, Clone)]
pub enum Command {
    RefreshAll,
    SelectProxy { group: String, proxy: String },
    TestGroup { group: String, url: String },
    CloseConnection { id: String },
    CloseAllConnections,
    SetMode(String),
    UpdateSubscription,
}

#[derive(Debug)]
pub enum InputOutcome {
    Continue,
    Quit,
    Run(Command),
}

#[derive(Debug)]
pub enum ApiEvent {
    Version(Result<VersionInfo, String>),
    Proxies(Result<ProxyPayload, String>),
    Connections(Result<ConnectionsPayload, String>),
    Rules(Result<RulesPayload, String>),
    Config(Result<RuntimeConfig, String>),
    Traffic(TrafficSample),
    Memory(MemorySample),
    Log(LogEntry),
    Status(Result<String, String>),
    GroupTested {
        group: String,
        result: Result<(), String>,
    },
    Subscription(SubscriptionState),
    StreamError {
        stream: &'static str,
        error: String,
    },
}

pub struct App {
    pub page: Page,
    pub overlay: Overlay,
    pub version: Option<VersionInfo>,
    pub proxies: ProxyPayload,
    pub connections: ConnectionsPayload,
    pub rules: RulesPayload,
    pub config: RuntimeConfig,
    pub traffic: VecDeque<TrafficSample>,
    pub memory: VecDeque<MemorySample>,
    pub logs: VecDeque<LogEntry>,
    pub connected: bool,
    pub status: String,
    pub status_error: bool,
    status_since: Instant,
    pub filter: String,
    pub editing_filter: bool,

    pub pane: Pane,
    pub group_index: usize,
    pub node_index: usize,
    pub node_sort: NodeSort,
    /// Groups with a latency test in flight.
    pub testing: BTreeSet<String>,
    /// Groups already auto-tested this session.
    tested: BTreeSet<String>,
    pub group_test_urls: BTreeMap<String, String>,

    pub connection_index: usize,
    /// Keeps the cursor on the same connection while the list reorders.
    connection_id: Option<String>,
    pub connection_sort: ConnectionSort,
    pub rule_index: usize,
    pub log_index: usize,
    pub log_level: LogLevel,
    pub log_follow: bool,

    pub config_source: Option<String>,
    pub subscription: SubscriptionState,
    pub subscription_interval: Duration,
    pub subscription_busy: bool,
}

impl App {
    pub fn new(group_test_urls: BTreeMap<String, String>) -> Self {
        Self {
            page: Page::Proxies,
            overlay: Overlay::None,
            version: None,
            proxies: ProxyPayload::default(),
            connections: ConnectionsPayload::default(),
            rules: RulesPayload::default(),
            config: RuntimeConfig::default(),
            traffic: VecDeque::with_capacity(HISTORY_LIMIT),
            memory: VecDeque::with_capacity(HISTORY_LIMIT),
            logs: VecDeque::with_capacity(LOG_LIMIT),
            connected: false,
            status: "Connecting to controller…".into(),
            status_error: false,
            status_since: Instant::now(),
            filter: String::new(),
            editing_filter: false,
            pane: Pane::Groups,
            group_index: 0,
            node_index: 0,
            node_sort: NodeSort::Config,
            testing: BTreeSet::new(),
            tested: BTreeSet::new(),
            group_test_urls,
            connection_index: 0,
            connection_id: None,
            connection_sort: ConnectionSort::Newest,
            rule_index: 0,
            log_index: 0,
            log_level: LogLevel::Info,
            log_follow: true,
            config_source: None,
            subscription: SubscriptionState::default(),
            subscription_interval: Duration::ZERO,
            subscription_busy: false,
        }
    }

    pub fn set_config_source(&mut self, source: Option<String>) {
        self.config_source = source;
    }

    /// Applies a controller event; may ask for a follow-up command such as the first latency test.
    pub fn apply(&mut self, event: ApiEvent) -> Option<Command> {
        match event {
            ApiEvent::Version(Ok(value)) => {
                self.version = Some(value);
                self.mark_connected();
            }
            ApiEvent::Proxies(Ok(value)) => {
                let first_load = self.proxies.proxies.is_empty();
                self.proxies = value;
                self.mark_connected();
                if first_load {
                    self.focus_active_node();
                    return self.auto_test();
                }
                self.clamp_selections();
            }
            ApiEvent::Connections(Ok(value)) => {
                self.connections = value;
                self.mark_connected();
                self.follow_connection();
            }
            ApiEvent::Rules(Ok(value)) => {
                self.rules = value;
                self.mark_connected();
                self.clamp_selections();
            }
            ApiEvent::Config(Ok(value)) => {
                self.config = value;
                self.mark_connected();
            }
            ApiEvent::Traffic(value) => push_bounded(&mut self.traffic, value, HISTORY_LIMIT),
            ApiEvent::Memory(value) => push_bounded(&mut self.memory, value, HISTORY_LIMIT),
            ApiEvent::Log(value) => {
                push_bounded(&mut self.logs, value, LOG_LIMIT);
                if self.log_follow {
                    self.log_index = self.logs().len().saturating_sub(1);
                }
            }
            ApiEvent::Status(Ok(message)) => self.set_status(message),
            ApiEvent::Status(Err(error)) => self.set_error(error),
            ApiEvent::GroupTested { group, result } => {
                self.testing.remove(&group);
                match result {
                    Ok(()) => self.set_status(format!("Tested {group}")),
                    Err(error) => self.set_error(format!("Testing {group} failed: {error}")),
                }
            }
            ApiEvent::Subscription(state) => {
                self.subscription = state;
                self.subscription_busy = false;
            }
            ApiEvent::StreamError { stream, error } => {
                self.set_error(format!("{stream} stream reconnecting: {error}"));
            }
            ApiEvent::Version(Err(error))
            | ApiEvent::Proxies(Err(error))
            | ApiEvent::Connections(Err(error))
            | ApiEvent::Rules(Err(error))
            | ApiEvent::Config(Err(error)) => {
                self.connected = false;
                self.set_error(format!("Controller unavailable: {error}"));
            }
        }
        None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> InputOutcome {
        if key.kind != KeyEventKind::Press {
            return InputOutcome::Continue;
        }
        match self.overlay {
            Overlay::ConfirmCloseAll => {
                self.overlay = Overlay::None;
                return if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    InputOutcome::Run(Command::CloseAllConnections)
                } else {
                    self.set_status("Cancelled");
                    InputOutcome::Continue
                };
            }
            Overlay::Help => {
                self.overlay = Overlay::None;
                return InputOutcome::Continue;
            }
            Overlay::None => {}
        }
        if self.editing_filter {
            self.edit_filter(key);
            return InputOutcome::Continue;
        }

        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let command = match (self.page, key.code) {
            (_, KeyCode::Char('d')) if control => self.move_cursor(PAGE_STEP, false),
            (_, KeyCode::Char('u')) if control => self.move_cursor(-PAGE_STEP, false),
            (_, KeyCode::Char('q')) => return InputOutcome::Quit,
            (_, KeyCode::Char('?')) => {
                self.overlay = Overlay::Help;
                None
            }
            (_, KeyCode::Char('/')) => {
                self.editing_filter = true;
                if self.page == Page::Proxies {
                    self.pane = Pane::Nodes;
                }
                None
            }
            (_, KeyCode::Esc) => {
                self.back();
                None
            }
            (_, KeyCode::Tab) => {
                self.change_page(self.page.next());
                None
            }
            (_, KeyCode::BackTab) => {
                self.change_page(self.page.previous());
                None
            }
            (_, KeyCode::Char(digit @ '1'..='4')) => {
                self.change_page(Page::ALL[usize::from(digit as u8 - b'1')]);
                None
            }
            (_, KeyCode::Char('r')) => Some(Command::RefreshAll),
            (_, KeyCode::Char('m')) => Some(Command::SetMode(self.next_mode().into())),
            (_, KeyCode::Char('u')) => {
                self.subscription_busy = true;
                self.set_status("Updating subscription…");
                Some(Command::UpdateSubscription)
            }
            (_, KeyCode::Down | KeyCode::Char('j')) => self.move_cursor(1, true),
            (_, KeyCode::Up | KeyCode::Char('k')) => self.move_cursor(-1, true),
            (_, KeyCode::PageDown) => self.move_cursor(PAGE_STEP, false),
            (_, KeyCode::PageUp) => self.move_cursor(-PAGE_STEP, false),
            (_, KeyCode::Home | KeyCode::Char('g')) => self.move_cursor(isize::MIN, false),
            (_, KeyCode::End | KeyCode::Char('G')) => self.move_cursor(isize::MAX, false),

            (Page::Proxies, KeyCode::Left | KeyCode::Char('h')) => {
                self.pane = Pane::Groups;
                None
            }
            (Page::Proxies, KeyCode::Right | KeyCode::Char('l')) => self.open_group(),
            (Page::Proxies, KeyCode::Enter) => match self.pane {
                Pane::Groups => self.open_group(),
                Pane::Nodes => self.use_selected_node(),
            },
            (Page::Proxies, KeyCode::Char('t')) => self.test_group(),
            (Page::Proxies, KeyCode::Char('s')) => {
                self.node_sort = match self.node_sort {
                    NodeSort::Config => NodeSort::Latency,
                    NodeSort::Latency => NodeSort::Config,
                };
                self.focus_active_node();
                None
            }

            (Page::Connections, KeyCode::Char('x' | 'd') | KeyCode::Delete) => self
                .selected_connection()
                .map(|connection| Command::CloseConnection {
                    id: connection.id.clone(),
                }),
            (Page::Connections, KeyCode::Char('D')) => {
                self.overlay = Overlay::ConfirmCloseAll;
                None
            }
            (Page::Connections, KeyCode::Char('s')) => {
                self.connection_sort = self.connection_sort.next();
                self.follow_connection();
                self.set_status(format!(
                    "Connections sorted by {}",
                    self.connection_sort.title()
                ));
                None
            }

            (Page::Logs, KeyCode::Char('v')) => {
                self.log_level = self.log_level.next();
                self.log_follow = true;
                self.log_index = self.logs().len().saturating_sub(1);
                self.set_status(format!("Showing {} and above", self.log_level.title()));
                None
            }
            (Page::Logs, KeyCode::Char('c')) => {
                self.logs.clear();
                self.log_index = 0;
                self.log_follow = true;
                None
            }
            _ => None,
        };
        command.map_or(InputOutcome::Continue, InputOutcome::Run)
    }

    /// Selector-like groups in config order (the order of GLOBAL), GLOBAL itself last.
    pub fn group_names(&self) -> Vec<&str> {
        let proxies = &self.proxies.proxies;
        let is_group = |name: &str| proxies.get(name).is_some_and(|info| is_group(&info.kind));
        let mut names = proxies
            .get("GLOBAL")
            .map(|global| {
                global
                    .all
                    .iter()
                    .map(String::as_str)
                    .filter(|name| is_group(name))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // Groups missing from GLOBAL (or no GLOBAL at all) follow alphabetically.
        let listed = names.iter().copied().collect::<BTreeSet<_>>();
        names.extend(
            proxies
                .keys()
                .map(String::as_str)
                .filter(|name| *name != "GLOBAL" && !listed.contains(name) && is_group(name)),
        );
        if is_group("GLOBAL") {
            names.push("GLOBAL");
        }
        names
    }

    pub fn selected_group(&self) -> Option<(&str, &ProxyInfo)> {
        let name = *self.group_names().get(self.group_index)?;
        self.proxies.proxies.get(name).map(|info| (name, info))
    }

    pub fn nodes(&self) -> Vec<&str> {
        let Some((_, group)) = self.selected_group() else {
            return Vec::new();
        };
        let mut nodes = group
            .all
            .iter()
            .map(String::as_str)
            .filter(|name| contains_folded(name, &self.filter))
            .collect::<Vec<_>>();
        if self.node_sort == NodeSort::Latency {
            // Tested nodes fastest first, then untested, then timeouts.
            nodes.sort_by_key(|name| match self.delay(name) {
                Some(0) => (2, 0),
                Some(delay) => (0, delay),
                None => (1, 0),
            });
        }
        nodes
    }

    /// Last measured delay in ms; `Some(0)` means the test timed out.
    pub fn delay(&self, name: &str) -> Option<u64> {
        self.proxies
            .proxies
            .get(name)?
            .history
            .last()
            .map(|history| history.delay)
    }

    pub fn connections(&self) -> Vec<&Connection> {
        let mut connections = self
            .connections
            .connections
            .iter()
            .filter(|connection| {
                let haystack = format!(
                    "{} {} {} {} {}",
                    connection.destination(),
                    connection.metadata.process,
                    connection.rule,
                    connection.rule_payload,
                    connection.chains.join(" ")
                );
                contains_folded(&haystack, &self.filter)
            })
            .collect::<Vec<_>>();
        match self.connection_sort {
            ConnectionSort::Newest => {
                connections.sort_by(|left, right| right.start.cmp(&left.start));
            }
            ConnectionSort::Download => {
                connections.sort_by_key(|connection| std::cmp::Reverse(connection.download));
            }
            ConnectionSort::Upload => {
                connections.sort_by_key(|connection| std::cmp::Reverse(connection.upload));
            }
            ConnectionSort::Host => connections.sort_by_key(|connection| connection.destination()),
        }
        connections
    }

    pub fn selected_connection(&self) -> Option<&Connection> {
        self.connections().get(self.connection_index).copied()
    }

    pub fn rules(&self) -> Vec<&Rule> {
        self.rules
            .rules
            .iter()
            .filter(|rule| {
                contains_folded(
                    &format!("{} {} {}", rule.kind, rule.payload, rule.proxy),
                    &self.filter,
                )
            })
            .collect()
    }

    pub fn logs(&self) -> Vec<&LogEntry> {
        self.logs
            .iter()
            .filter(|log| LogLevel::parse(&log.level) >= self.log_level)
            .filter(|log| contains_folded(&log.payload, &self.filter))
            .collect()
    }

    fn change_page(&mut self, page: Page) {
        self.page = page;
        self.filter.clear();
        self.editing_filter = false;
        self.clamp_selections();
    }

    /// Esc: clear the filter first, then step out of the node list.
    fn back(&mut self) {
        if !self.filter.is_empty() {
            self.filter.clear();
            self.clamp_selections();
            if self.page == Page::Proxies {
                self.focus_active_node();
            }
        } else if self.page == Page::Proxies {
            self.pane = Pane::Groups;
        }
    }

    fn open_group(&mut self) -> Option<Command> {
        if self.pane == Pane::Groups {
            self.pane = Pane::Nodes;
            self.focus_active_node();
        }
        self.auto_test()
    }

    fn use_selected_node(&mut self) -> Option<Command> {
        let (group, info) = self.selected_group()?;
        let proxy = *self.nodes().get(self.node_index)?;
        if proxy == info.now {
            let message = format!("{group} already uses {proxy}");
            self.set_status(message);
            return None;
        }
        if !info.kind.eq_ignore_ascii_case("selector") {
            let message = format!("{group} is a {} group and picks its node itself", info.kind);
            self.set_error(message);
            return None;
        }
        Some(Command::SelectProxy {
            group: group.to_owned(),
            proxy: proxy.to_owned(),
        })
    }

    fn test_group(&mut self) -> Option<Command> {
        let (group, info) = self.selected_group()?;
        let group = group.to_owned();
        if !self.has_testable_nodes(info) {
            self.set_status(format!("{group} only contains groups and built-ins"));
            return None;
        }
        self.tested.insert(group.clone());
        self.start_test(group)
    }

    /// Tests a group the first time it is opened, if it has real nodes.
    fn auto_test(&mut self) -> Option<Command> {
        let (group, info) = self.selected_group()?;
        if self.tested.contains(group) || !self.has_testable_nodes(info) {
            return None;
        }
        let group = group.to_owned();
        self.tested.insert(group.clone());
        self.start_test(group)
    }

    fn start_test(&mut self, group: String) -> Option<Command> {
        if !self.testing.insert(group.clone()) {
            return None;
        }
        let url = self
            .group_test_urls
            .get(&group)
            .cloned()
            .unwrap_or_else(|| DEFAULT_TEST_URL.to_owned());
        self.set_status(format!("Testing {group}…"));
        Some(Command::TestGroup { group, url })
    }

    fn has_testable_nodes(&self, group: &ProxyInfo) -> bool {
        group.all.iter().any(|name| {
            self.proxies.proxies.get(name).is_some_and(|info| {
                !is_group(&info.kind) && !BUILTIN_KINDS.contains(&info.kind.to_lowercase().as_str())
            })
        })
    }

    fn next_mode(&self) -> &'static str {
        match self.config.mode.to_ascii_lowercase().as_str() {
            "rule" => "global",
            "global" => "direct",
            _ => "rule",
        }
    }

    /// Moves the cursor of the focused list; `isize::MIN`/`MAX` jump to the ends.
    fn move_cursor(&mut self, delta: isize, wrap: bool) -> Option<Command> {
        let length = match (self.page, self.pane) {
            (Page::Proxies, Pane::Groups) => self.group_names().len(),
            (Page::Proxies, Pane::Nodes) => self.nodes().len(),
            (Page::Connections, _) => self.connections().len(),
            (Page::Rules, _) => self.rules().len(),
            (Page::Logs, _) => self.logs().len(),
        };
        let index = match (self.page, self.pane) {
            (Page::Proxies, Pane::Groups) => &mut self.group_index,
            (Page::Proxies, Pane::Nodes) => &mut self.node_index,
            (Page::Connections, _) => &mut self.connection_index,
            (Page::Rules, _) => &mut self.rule_index,
            (Page::Logs, _) => &mut self.log_index,
        };
        *index = moved_index(*index, length, delta, wrap);
        let at_end = *index + 1 >= length;
        match (self.page, self.pane) {
            (Page::Proxies, Pane::Groups) => self.focus_active_node(),
            (Page::Connections, _) => {
                self.connection_id = self
                    .selected_connection()
                    .map(|connection| connection.id.clone());
            }
            (Page::Logs, _) => self.log_follow = at_end,
            _ => {}
        }
        None
    }

    /// Puts the node cursor on the node the selected group currently uses.
    fn focus_active_node(&mut self) {
        let now = self
            .selected_group()
            .map(|(_, info)| info.now.clone())
            .unwrap_or_default();
        self.node_index = self
            .nodes()
            .iter()
            .position(|name| *name == now)
            .unwrap_or(0);
    }

    fn follow_connection(&mut self) {
        let position = self.connection_id.as_ref().and_then(|id| {
            self.connections()
                .iter()
                .position(|connection| &connection.id == id)
        });
        match position {
            Some(position) => self.connection_index = position,
            None => {
                self.connection_index =
                    clamp_index(self.connection_index, self.connections().len());
                self.connection_id = self
                    .selected_connection()
                    .map(|connection| connection.id.clone());
            }
        }
    }

    fn edit_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.editing_filter = false,
            KeyCode::Esc => {
                self.editing_filter = false;
                self.filter.clear();
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.clear();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.filter.push(character);
            }
            _ => return,
        }
        match self.page {
            Page::Proxies => self.focus_active_node(),
            Page::Connections => self.follow_connection(),
            Page::Rules => self.rule_index = 0,
            Page::Logs => self.log_index = self.logs().len().saturating_sub(1),
        }
    }

    fn clamp_selections(&mut self) {
        self.group_index = clamp_index(self.group_index, self.group_names().len());
        self.node_index = clamp_index(self.node_index, self.nodes().len());
        self.follow_connection();
        self.rule_index = clamp_index(self.rule_index, self.rules().len());
        self.log_index = clamp_index(self.log_index, self.logs().len());
    }

    fn mark_connected(&mut self) {
        self.connected = true;
        if self.status.starts_with("Connecting")
            || self.status.starts_with("Controller unavailable")
        {
            self.set_status("Connected");
        }
    }

    /// The status line, until it has been on screen long enough to be read.
    pub fn visible_status(&self) -> Option<&str> {
        let lifetime = if self.status_error {
            ERROR_LIFETIME
        } else {
            STATUS_LIFETIME
        };
        let busy = self.subscription_busy || !self.testing.is_empty() || !self.connected;
        (busy || self.status_since.elapsed() < lifetime).then_some(self.status.as_str())
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_error = false;
        self.status_since = Instant::now();
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_error = true;
        self.status_since = Instant::now();
    }
}

fn is_group(kind: &str) -> bool {
    GROUP_KINDS
        .iter()
        .any(|group| kind.eq_ignore_ascii_case(group))
}

fn push_bounded<T>(queue: &mut VecDeque<T>, value: T, limit: usize) {
    if queue.len() == limit {
        queue.pop_front();
    }
    queue.push_back(value);
}

fn contains_folded(haystack: &str, needle: &str) -> bool {
    needle.is_empty() || haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn clamp_index(index: usize, length: usize) -> usize {
    index.min(length.saturating_sub(1))
}

/// Steps wrap around the ends when `wrap` is set; larger jumps stop at them.
fn moved_index(index: usize, length: usize, delta: isize, wrap: bool) -> usize {
    if length == 0 {
        return 0;
    }
    let last = length - 1;
    let target = index as i128 + delta as i128;
    if target < 0 {
        if wrap { last } else { 0 }
    } else if target > last as i128 {
        if wrap { 0 } else { last }
    } else {
        target as usize
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{ApiEvent, App, Command, InputOutcome, Page, Pane, moved_index};
    use crate::model::{Connection, ConnectionsPayload, DelayHistory, ProxyInfo, ProxyPayload};

    fn press(app: &mut App, code: KeyCode) -> InputOutcome {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn proxy(kind: &str, now: &str, all: &[&str]) -> ProxyInfo {
        ProxyInfo {
            kind: kind.into(),
            now: now.into(),
            all: all.iter().map(|name| (*name).into()).collect(),
            ..Default::default()
        }
    }

    fn sample_proxies() -> ProxyPayload {
        let mut payload = ProxyPayload::default();
        let entries = [
            (
                "GLOBAL",
                proxy("Selector", "DIRECT", &["Proxy", "Auto", "Ads", "DIRECT"]),
            ),
            ("Proxy", proxy("Selector", "jp", &["hk", "jp", "us"])),
            ("Auto", proxy("URLTest", "hk", &["hk", "jp"])),
            ("Ads", proxy("Selector", "REJECT", &["REJECT", "DIRECT"])),
            ("hk", proxy("Shadowsocks", "", &[])),
            ("jp", proxy("Shadowsocks", "", &[])),
            ("us", proxy("Trojan", "", &[])),
            ("DIRECT", proxy("Direct", "", &[])),
            ("REJECT", proxy("Reject", "", &[])),
        ];
        for (name, info) in entries {
            payload.proxies.insert(name.into(), info);
        }
        payload
    }

    fn loaded_app() -> (App, Option<Command>) {
        let mut app = App::new(Default::default());
        let command = app.apply(ApiEvent::Proxies(Ok(sample_proxies())));
        (app, command)
    }

    #[test]
    fn cursor_wraps_on_step_and_clamps_on_jump() {
        assert_eq!(moved_index(0, 3, -1, true), 2);
        assert_eq!(moved_index(2, 3, 1, true), 0);
        assert_eq!(moved_index(1, 3, isize::MAX, false), 2);
        assert_eq!(moved_index(1, 3, isize::MIN, false), 0);
        assert_eq!(moved_index(0, 0, 1, true), 0);
    }

    #[test]
    fn groups_follow_config_order_with_global_last() {
        let (app, _) = loaded_app();
        assert_eq!(app.group_names(), ["Proxy", "Auto", "Ads", "GLOBAL"]);
    }

    #[test]
    fn first_load_lands_on_active_node_and_tests_main_group() {
        let (app, command) = loaded_app();
        assert_eq!(app.nodes()[app.node_index], "jp");
        assert!(matches!(command, Some(Command::TestGroup { ref group, .. }) if group == "Proxy"));
        assert!(app.testing.contains("Proxy"));
    }

    #[test]
    fn groups_pane_moves_groups_and_nodes_pane_selects() {
        let (mut app, _) = loaded_app();
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.selected_group().unwrap().0, "Auto");
        assert_eq!(app.nodes()[app.node_index], "hk");
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.pane, Pane::Nodes);
        press(&mut app, KeyCode::Char('j'));
        assert!(matches!(
            press(&mut app, KeyCode::Enter),
            InputOutcome::Run(Command::SelectProxy { ref group, ref proxy })
                if group == "Proxy" && proxy == "us"
        ));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.pane, Pane::Groups);
    }

    #[test]
    fn groups_without_real_nodes_are_not_auto_tested() {
        let (mut app, _) = loaded_app();
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.selected_group().unwrap().0, "Ads");
        assert!(matches!(
            press(&mut app, KeyCode::Char('l')),
            InputOutcome::Continue
        ));
    }

    #[test]
    fn latency_sort_puts_fastest_first_and_timeouts_last() {
        let (mut app, _) = loaded_app();
        for (name, delay) in [("hk", 0), ("jp", 300), ("us", 90)] {
            app.proxies.proxies.get_mut(name).unwrap().history = vec![DelayHistory {
                delay,
                ..Default::default()
            }];
        }
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.nodes(), ["us", "jp", "hk"]);
        assert_eq!(app.nodes()[app.node_index], "jp");
    }

    #[test]
    fn esc_clears_filter_before_leaving_pane() {
        let (mut app, _) = loaded_app();
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('u'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.nodes(), ["us"]);
        press(&mut app, KeyCode::Esc);
        assert!(app.filter.is_empty());
        assert_eq!(app.pane, Pane::Nodes);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.pane, Pane::Groups);
    }

    #[test]
    fn connection_cursor_sticks_to_the_same_connection() {
        let mut app = App::new(Default::default());
        let connection = |id: &str, start: &str| Connection {
            id: id.into(),
            start: start.into(),
            ..Default::default()
        };
        let payload = |connections| ConnectionsPayload {
            connections,
            ..Default::default()
        };
        app.apply(ApiEvent::Connections(Ok(payload(vec![
            connection("a", "1"),
            connection("b", "2"),
        ]))));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.selected_connection().unwrap().id, "a");
        app.apply(ApiEvent::Connections(Ok(payload(vec![
            connection("a", "1"),
            connection("b", "2"),
            connection("c", "3"),
        ]))));
        assert_eq!(app.selected_connection().unwrap().id, "a");
    }

    #[test]
    fn bulk_close_requires_confirmation() {
        let mut app = App::new(Default::default());
        app.page = Page::Connections;
        assert!(matches!(
            press(&mut app, KeyCode::Char('D')),
            InputOutcome::Continue
        ));
        assert!(matches!(
            press(&mut app, KeyCode::Char('y')),
            InputOutcome::Run(Command::CloseAllConnections)
        ));
    }

    #[test]
    fn page_change_clears_filter() {
        let mut app = App::new(Default::default());
        app.filter = "hk".into();
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.page, Page::Connections);
        assert!(app.filter.is_empty());
    }
}

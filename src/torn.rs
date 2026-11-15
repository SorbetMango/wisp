//! Torn City status for the dock. Private to this fork.
//!
//! A worker thread asks the Torn API for the player every 30 s and for the
//! faction every 5 min (every minute during a war or a chain), through the
//! curl.exe that ships with Windows. The dock counts every timer down itself
//! between fetches, as the TornStatus Rainmeter skin does. Torn allows 100
//! requests a minute per player; this makes about three.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::messages::WM_TOGGL;
use crate::toggl::now;

pub const HOME: &str = "https://www.torn.com/";
const USER_URL: &str = "https://api.torn.com/user/?selections=bars,travel,cooldowns,profile,money,notifications,icons,education&comment=Wisp&key=";
const FACTION_URL: &str = "https://api.torn.com/v2/faction/?selections=wars,chain&comment=Wisp&key=";
const USER_EVERY: u64 = 30;
const FACTION_EVERY: u64 = 300;
/// During a ranked war or a chain.
const FACTION_FAST: u64 = 60;
/// A ranked war is announced on the dock this long before it starts.
const WAR_SOON: u64 = 3 * 86_400;
/// Older data than this is shown with a warning.
const STALE_AFTER: u64 = 90;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Torn bans an address that keeps sending a wrong key, so nothing is sent
/// again until the key changes.
const BAD_KEY: &str = "Torn refused the API key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bar {
    pub current: i64,
    pub maximum: i64,
    /// Seconds until full, when fetched.
    pub fulltime: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct User {
    /// Local time of the fetch, in seconds since the Unix epoch.
    pub fetched: u64,
    pub server_time: u64,
    pub name: String,
    pub level: u64,
    pub energy: Bar,
    pub nerve: Bar,
    pub life: Bar,
    pub happy: Bar,
    pub destination: String,
    /// Seconds to landing, when fetched.
    pub travel_left: u64,
    pub drug: u64,
    pub booster: u64,
    pub medical: u64,
    /// "Okay", "Traveling", "Hospital", "Jail", "Abroad"...
    pub state: String,
    pub description: String,
    /// Server time when hospital or jail ends.
    pub until: u64,
    pub faction_id: u64,
    pub money: i64,
    pub messages: u64,
    pub events: u64,
    /// Seconds left on the education course, when fetched. Zero with no
    /// course running; `None` if the reply said nothing about education.
    pub education_left: Option<u64>,
    /// The status icons of the player's profile, as "Title - details".
    pub icons: Vec<String>,
}

impl User {
    /// The details of the status icon with this title, such as "Racing".
    fn icon(&self, title: &str) -> Option<&str> {
        self.icons
            .iter()
            .find_map(|icon| icon.strip_prefix(title)?.strip_prefix(" - "))
    }

    /// The race is over and its result waits to be seen.
    fn race_finished(&self) -> bool {
        self.icon("Racing").is_some_and(|race| race.contains("finished"))
    }

    /// Seconds until the organized crime is ready at `elapsed` seconds after
    /// the fetch, if the player is in one.
    fn crime_left(&self, elapsed: u64) -> Option<u64> {
        let crime = self.icon("Organized Crime")?;
        Some(duration(crime).unwrap_or(0).saturating_sub(elapsed))
    }

    /// The bank investment has run its term.
    fn bank_ready(&self) -> bool {
        self.icon("Bank Investment")
            .is_some_and(|bank| duration(bank).is_none())
    }
}

/// The time at the end of a status icon's text: "2 hours, 1 minute and 21
/// seconds" or "02:09:02". `None` if the text ends with no time.
fn duration(text: &str) -> Option<u64> {
    let last = text.rsplit(" - ").next()?.trim();
    let mut seconds = 0;
    let mut found = false;
    let mut words = last.split([' ', ',']).filter(|word| !word.is_empty() && *word != "and");
    while let Some(word) = words.next() {
        let Ok(count) = word.parse::<u64>() else {
            return None;
        };
        let unit = match words.next()?.trim_end_matches('s') {
            "day" => 86_400,
            "hour" => 3600,
            "minute" => 60,
            "second" => 1,
            _ => return None,
        };
        seconds += count * unit;
        found = true;
    }
    found.then_some(seconds)
}

/// "2d 10h" above a day, a clock below.
fn long_clock(seconds: u64) -> String {
    if seconds >= 86_400 {
        format!("{}d {}h", seconds / 86_400, seconds / 3600 % 24)
    } else {
        clock(seconds)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct War {
    pub start: u64,
    /// Zero while the war is on.
    pub end: u64,
    pub ours: i64,
    pub theirs: i64,
    pub enemy: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Faction {
    pub fetched: u64,
    pub war: Option<War>,
    pub chain: u64,
    pub chain_max: u64,
    /// Seconds until the chain breaks, when fetched.
    pub chain_timeout: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub user: Option<User>,
    pub faction: Option<Faction>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Nothing to report.
    Idle,
    /// Flying away from Torn City.
    PlaneOut,
    /// Flying home.
    PlaneHome,
    Hospital,
    Jail,
    Energy,
    Nerve,
    War,
    Chain,
    Warning,
    /// The education course is over, or none is running.
    Education,
    /// A finished race.
    Race,
    /// The organized crime is ready.
    Crime,
    /// The bank investment has run its term.
    Bank,
}

impl Icon {
    pub fn glyph(self) -> &'static str {
        match self {
            Icon::Idle => "T",
            Icon::PlaneOut | Icon::PlaneHome => "\u{2708}\u{FE0F}",
            Icon::Hospital => "\u{1F3E5}",
            Icon::Jail => "\u{1F512}",
            Icon::Energy => "\u{26A1}",
            Icon::Nerve => "\u{1F525}",
            Icon::War => "\u{2694}\u{FE0F}",
            Icon::Chain => "\u{26D3}\u{FE0F}",
            Icon::Warning => "\u{26A0}\u{FE0F}",
            Icon::Education => "\u{1F393}",
            Icon::Race => "\u{1F3C1}",
            Icon::Crime => "\u{1F575}\u{FE0F}",
            Icon::Bank => "\u{1F3E6}",
        }
    }
}

/// One thing the dock shows: an icon and, for timers and scores, a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    pub icon: Icon,
    pub text: String,
    /// Drawn in red: a chain about to break.
    pub alert: bool,
}

impl Chip {
    fn new(icon: Icon, text: impl Into<String>) -> Self {
        Chip {
            icon,
            text: text.into(),
            alert: false,
        }
    }

    /// The chip's width in the dock, in DIPs.
    pub fn width(&self) -> f32 {
        CHIP_ICON
            + if self.text.is_empty() {
                0.0
            } else {
                self.text.chars().count() as f32 * CHIP_CHAR + CHIP_GAP
            }
    }
}

/// An icon's box, and the room each character and the gap after a text take, in DIPs.
pub const CHIP_ICON: f32 = 24.0;
const CHIP_CHAR: f32 = 7.5;
const CHIP_GAP: f32 = 8.0;

pub fn width(chips: &[Chip]) -> f32 {
    chips.iter().map(Chip::width).sum()
}

#[derive(Default)]
struct Shared {
    key: Mutex<String>,
    snapshot: Mutex<Snapshot>,
    sender: Mutex<Option<Sender<()>>>,
    notify: AtomicIsize,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(Shared::default)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Takes the API key from the config. An empty key turns the module off.
pub fn configure(key: &str) {
    let s = shared();
    let key = key.trim();
    let changed = {
        let mut current = lock(&s.key);
        let changed = *current != key;
        *current = key.to_string();
        changed
    };
    if key.is_empty() {
        *lock(&s.snapshot) = Snapshot::default();
        if changed {
            notify();
        }
        return;
    }
    let mut sender = lock(&s.sender);
    if sender.is_none() {
        let (tx, rx) = channel();
        match std::thread::Builder::new()
            .name("torn".into())
            .spawn(move || worker(rx))
        {
            Ok(_) => *sender = Some(tx),
            Err(error) => eprintln!("Torn: {error}"),
        }
    }
    if changed && let Some(sender) = sender.as_ref() {
        let _ = sender.send(());
    }
}

pub fn enabled() -> bool {
    !lock(&shared().key).is_empty()
}

pub fn snapshot() -> Snapshot {
    lock(&shared().snapshot).clone()
}

pub fn set_notify(hwnd: HWND) {
    shared().notify.store(hwnd.0 as isize, Ordering::Relaxed);
}

fn notify() {
    let hwnd = shared().notify.load(Ordering::Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_TOGGL, WPARAM(0), LPARAM(0));
        }
    }
}

fn worker(wake: Receiver<()>) {
    let mut refused: Option<String> = None;
    let mut faction_due = 0;
    loop {
        let key = lock(&shared().key).clone();
        if !key.is_empty() && refused.as_deref() != Some(key.as_str()) {
            let at = now();
            let mut snapshot = snapshot();
            match fetch_user(&key, at) {
                Ok(user) => {
                    snapshot.user = Some(user);
                    snapshot.error = None;
                }
                Err(error) => {
                    eprintln!("Torn: {error}");
                    if error == BAD_KEY {
                        refused = Some(key.clone());
                        snapshot.user = None;
                    }
                    snapshot.error = Some(error);
                }
            }
            if at >= faction_due
                && refused.is_none()
                && let Some(user) = &snapshot.user
            {
                match fetch_faction(&key, user.faction_id, at) {
                    Ok(faction) => {
                        let busy = war_now(&faction, user.server_time).is_some() || faction.chain >= CHAIN_SHOWN;
                        faction_due = at + if busy { FACTION_FAST } else { FACTION_EVERY };
                        snapshot.faction = Some(faction);
                    }
                    // A player without a faction still gets everything else.
                    Err(error) => {
                        eprintln!("Torn faction: {error}");
                        faction_due = at + FACTION_EVERY;
                    }
                }
            }
            *lock(&shared().snapshot) = snapshot;
            notify();
        }
        match wake.recv_timeout(Duration::from_secs(USER_EVERY)) {
            Ok(()) => {
                // A new key: forget what the old one was told.
                refused = None;
                faction_due = 0;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn fetch_user(key: &str, at: u64) -> Result<User, String> {
    let body = match std::env::var("WISP_TORN_FIXTURE") {
        Ok(path) => std::fs::read_to_string(path).map_err(|e| e.to_string())?,
        Err(_) => get(&format!("{USER_URL}{key}"))?,
    };
    parse_user(&body, at)
}

fn fetch_faction(key: &str, ours: u64, at: u64) -> Result<Faction, String> {
    let body = match std::env::var("WISP_TORN_FACTION_FIXTURE") {
        Ok(path) => std::fs::read_to_string(path).map_err(|e| e.to_string())?,
        Err(_) if std::env::var("WISP_TORN_FIXTURE").is_ok() => return Err("no faction fixture".into()),
        Err(_) => get(&format!("{FACTION_URL}{key}"))?,
    };
    parse_faction(&body, ours, at)
}

fn api_error(value: &serde_json::Value) -> Option<String> {
    let error = value.get("error")?;
    Some(match error["code"].as_u64() {
        Some(2 | 10 | 13 | 16 | 18) => BAD_KEY.to_string(),
        _ => format!("Torn: {}", error["error"].as_str().unwrap_or("unknown error")),
    })
}

pub fn parse_user(body: &str, fetched: u64) -> Result<User, String> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).map_err(|e| format!("Torn's reply: {e}"))?;
    if let Some(error) = api_error(&value) {
        return Err(error);
    }
    let number = |value: &serde_json::Value| value.as_u64().unwrap_or(0);
    let text = |value: &serde_json::Value| value.as_str().unwrap_or_default().to_string();
    let bar = |name: &str| Bar {
        current: value[name]["current"].as_i64().unwrap_or(0),
        maximum: value[name]["maximum"].as_i64().unwrap_or(0),
        fulltime: number(&value[name]["fulltime"]),
    };
    Ok(User {
        fetched,
        server_time: value["server_time"].as_u64().ok_or("Torn's reply has no server time")?,
        name: text(&value["name"]),
        level: number(&value["level"]),
        energy: bar("energy"),
        nerve: bar("nerve"),
        life: bar("life"),
        happy: bar("happy"),
        destination: text(&value["travel"]["destination"]),
        travel_left: number(&value["travel"]["time_left"]),
        drug: number(&value["cooldowns"]["drug"]),
        booster: number(&value["cooldowns"]["booster"]),
        medical: number(&value["cooldowns"]["medical"]),
        state: text(&value["status"]["state"]),
        description: text(&value["status"]["description"]),
        until: number(&value["status"]["until"]),
        faction_id: number(&value["faction"]["faction_id"]),
        money: value["money_onhand"].as_i64().unwrap_or(0),
        messages: number(&value["notifications"]["messages"]),
        events: number(&value["notifications"]["events"]),
        education_left: value["education_timeleft"].as_u64(),
        icons: value["icons"]
            .as_object()
            .map(|icons| {
                icons
                    .values()
                    .filter_map(|icon| icon.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

pub fn parse_faction(body: &str, ours: u64, fetched: u64) -> Result<Faction, String> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).map_err(|e| format!("Torn's reply: {e}"))?;
    if let Some(error) = api_error(&value) {
        return Err(error);
    }
    let number = |value: &serde_json::Value| value.as_u64().unwrap_or(0);
    let ranked = &value["wars"]["ranked"];
    let war = ranked["factions"].as_array().and_then(|factions| {
        let side = |mine: bool| {
            factions
                .iter()
                .find(|faction| (faction["id"].as_u64() == Some(ours)) == mine)
        };
        let (us, them) = (side(true)?, side(false)?);
        Some(War {
            start: number(&ranked["start"]),
            end: number(&ranked["end"]),
            ours: us["score"].as_i64().unwrap_or(0),
            theirs: them["score"].as_i64().unwrap_or(0),
            enemy: them["name"].as_str().unwrap_or_default().to_string(),
        })
    });
    Ok(Faction {
        fetched,
        war,
        chain: number(&value["chain"]["current"]),
        chain_max: number(&value["chain"]["max"]),
        chain_timeout: number(&value["chain"]["timeout"]),
    })
}

/// A chain shorter than this is not worth a place on the dock.
const CHAIN_SHOWN: u64 = 10;

/// The war that is on at `server_now`.
fn war_now(faction: &Faction, server_now: u64) -> Option<&War> {
    faction
        .war
        .as_ref()
        .filter(|war| war.start <= server_now && (war.end == 0 || war.end > server_now))
}

/// "1:02:03" above an hour, "2:03" below.
pub fn clock(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

/// "15.2k" for a war score.
fn score(points: i64) -> String {
    if points.abs() >= 10_000 {
        format!("{:.1}k", points as f64 / 1000.0)
    } else {
        points.to_string()
    }
}

fn money(amount: i64) -> String {
    let digits = amount.abs().to_string();
    let mut grouped = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{}${grouped}", if amount < 0 { "-" } else { "" })
}

fn flies_home(destination: &str) -> bool {
    matches!(destination, "Torn" | "Torn City")
}

/// What the dock shows at `now`: only what needs attention, or one idle icon.
pub fn chips(snapshot: &Snapshot, now: u64) -> Vec<Chip> {
    let Some(user) = &snapshot.user else {
        let icon = if snapshot.error.is_some() {
            Icon::Warning
        } else {
            Icon::Idle
        };
        return vec![Chip::new(icon, "")];
    };
    let elapsed = now.saturating_sub(user.fetched);
    let server_now = user.server_time + elapsed;
    let mut chips = Vec::new();
    if snapshot.error.is_some() || elapsed > STALE_AFTER {
        chips.push(Chip::new(Icon::Warning, ""));
    }
    let flight = user.travel_left.saturating_sub(elapsed);
    let held = user.until.saturating_sub(server_now);
    if flight > 0 {
        let icon = if flies_home(&user.destination) {
            Icon::PlaneHome
        } else {
            Icon::PlaneOut
        };
        chips.push(Chip::new(icon, clock(flight)));
    } else if held > 0 && user.state == "Hospital" {
        chips.push(Chip::new(Icon::Hospital, clock(held)));
    } else if held > 0 && user.state == "Jail" {
        chips.push(Chip::new(Icon::Jail, clock(held)));
    }
    let full = |bar: Bar| bar.maximum > 0 && (bar.current >= bar.maximum || bar.fulltime <= elapsed);
    if full(user.energy) {
        chips.push(Chip::new(Icon::Energy, ""));
    }
    if full(user.nerve) {
        chips.push(Chip::new(Icon::Nerve, ""));
    }
    if user.education_left.is_some_and(|left| left <= elapsed) {
        chips.push(Chip::new(Icon::Education, ""));
    }
    // A race's result is collected in the city.
    if user.race_finished() && flight == 0 && !matches!(user.state.as_str(), "Traveling" | "Abroad") {
        chips.push(Chip::new(Icon::Race, ""));
    }
    if user.crime_left(elapsed) == Some(0) {
        chips.push(Chip::new(Icon::Crime, ""));
    }
    if user.bank_ready() {
        chips.push(Chip::new(Icon::Bank, ""));
    }
    if let Some(faction) = &snapshot.faction {
        if let Some(war) = war_now(faction, server_now) {
            chips.push(Chip::new(
                Icon::War,
                format!("{}:{}", score(war.ours), score(war.theirs)),
            ));
        } else if let Some(war) = &faction.war
            && war.start > server_now
            && war.start - server_now <= WAR_SOON
        {
            chips.push(Chip::new(Icon::War, clock(war.start - server_now)));
        }
        let chain_left = faction
            .chain_timeout
            .saturating_sub(now.saturating_sub(faction.fetched));
        if faction.chain >= CHAIN_SHOWN && chain_left > 0 {
            chips.push(Chip {
                icon: Icon::Chain,
                text: format!("{} {}", faction.chain, clock(chain_left)),
                alert: chain_left <= 60,
            });
        }
    }
    if chips.is_empty() {
        chips.push(Chip::new(Icon::Idle, ""));
    }
    chips
}

/// One tile of the launcher's Torn panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Stays the same between updates, so the focus stays on the tile.
    pub id: &'static str,
    pub label: String,
    pub detail: String,
    pub value: String,
    /// How full a bar is, 0 to 100.
    pub meter: Option<u32>,
    pub glyph: &'static str,
    /// The page a click opens.
    pub link: Option<&'static str>,
    /// Takes the panel's full width.
    pub wide: bool,
}

impl Row {
    fn info(id: &'static str, glyph: &'static str, label: &str, value: impl Into<String>) -> Self {
        Row {
            id,
            label: label.to_string(),
            detail: String::new(),
            value: value.into(),
            meter: None,
            glyph,
            link: None,
            wide: false,
        }
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    fn wide(mut self) -> Self {
        self.wide = true;
        self
    }
}

/// The card above the dock: its width, padding and row heights, in DIPs.
pub const CARD_W: f32 = 248.0;
pub const CARD_PAD: f32 = 12.0;
const CARD_ROW: f32 = 22.0;
const CARD_METER_ROW: f32 = 31.0;

impl Row {
    pub fn card_height(&self) -> f32 {
        if self.meter.is_some() { CARD_METER_ROW } else { CARD_ROW }
    }
}

/// The compact card the dock opens above the Torn status: the panel's rows
/// without the ones that only matter in the launcher.
pub fn card(snapshot: &Snapshot, now: u64) -> Vec<Row> {
    let mut rows = rows(snapshot, now);
    rows.retain(|row| !matches!(row.id, "open" | "updated") && !(row.id == "unread" && row.value == "None"));
    for row in &mut rows {
        // The card has no room for "cooldown" three times.
        if let Some(short) = row.label.strip_suffix(" cooldown") {
            row.label = short.to_string();
        }
        match row.id {
            // The status alone; the card belongs to one player.
            "status" => row.label = std::mem::take(&mut row.value),
            "war" => {
                row.label = "War".to_string();
                row.detail.clear();
            }
            _ => {}
        }
    }
    rows
}

pub fn card_height(rows: &[Row]) -> f32 {
    2.0 * CARD_PAD + rows.iter().map(Row::card_height).sum::<f32>()
}

/// The launcher's Torn panel at `now`.
pub fn rows(snapshot: &Snapshot, now: u64) -> Vec<Row> {
    let open = Row {
        link: Some(HOME),
        ..Row::info("open", "\u{E774}", "Open Torn", "").wide()
    };
    let Some(user) = &snapshot.user else {
        let value = match &snapshot.error {
            Some(error) => error.clone(),
            None if enabled() => "Connecting to Torn...".to_string(),
            None => "Add torn_api_key to the config".to_string(),
        };
        return vec![Row::info("status", "\u{E783}", "Torn", value).wide(), open];
    };
    let elapsed = now.saturating_sub(user.fetched);
    let server_now = user.server_time + elapsed;
    let flight = user.travel_left.saturating_sub(elapsed);
    let held = user.until.saturating_sub(server_now);
    let status = if flight > 0 {
        format!("Traveling to {}, {}", user.destination, clock(flight))
    } else if held > 0 && matches!(user.state.as_str(), "Hospital" | "Jail") {
        format!("{} for {}", user.state, clock(held))
    } else if user.description.is_empty() {
        user.state.clone()
    } else {
        user.description.clone()
    };
    let name = format!("{} [{}]", user.name, user.level);
    let mut rows = vec![Row::info("status", "\u{E77B}", &name, status).wide()];
    if let Some(error) = &snapshot.error {
        rows.push(Row::info("error", "\u{E783}", "Not updated", error.clone()).wide());
    } else if elapsed > STALE_AFTER {
        rows.push(Row::info("error", "\u{E783}", "Not updated", format!("for {}", clock(elapsed))).wide());
    }
    rows.push(Row::info("cash", "\u{E8C7}", "Cash", money(user.money)));
    let unread = match (user.messages, user.events) {
        (0, 0) => "None".to_string(),
        (messages, events) => format!("{messages} messages, {events} events"),
    };
    rows.push(Row::info("unread", "\u{E715}", "Unread", unread));
    let meter = |id: &'static str, glyph: &'static str, label: &str, bar: Bar, regenerates: bool| {
        let left = bar.fulltime.saturating_sub(elapsed);
        // Life and happy are shown as fetched; energy and nerve fill by a known time.
        let current = if regenerates && left == 0 {
            bar.current.max(bar.maximum)
        } else {
            bar.current
        };
        let detail = if !regenerates {
            String::new()
        } else if current >= bar.maximum {
            "full".to_string()
        } else {
            format!("full in {}", clock(left))
        };
        Row {
            meter: Some(if bar.maximum > 0 {
                (current.clamp(0, bar.maximum) * 100 / bar.maximum) as u32
            } else {
                0
            }),
            ..Row::info(id, glyph, label, format!("{current}/{}", bar.maximum)).detail(detail)
        }
    };
    rows.push(meter("energy", "\u{E945}", "Energy", user.energy, true));
    rows.push(meter("nerve", "\u{E7C1}", "Nerve", user.nerve, true));
    rows.push(meter("life", "\u{EB51}", "Life", user.life, false));
    rows.push(meter("happy", "\u{E76E}", "Happy", user.happy, false));
    let cooldown = |id: &'static str, glyph: &'static str, label: &str, seconds: u64| {
        let value = match seconds.saturating_sub(elapsed) {
            0 => "Ready".to_string(),
            left => clock(left),
        };
        Row::info(id, glyph, label, value)
    };
    rows.push(cooldown("drug", "\u{E95E}", "Drug cooldown", user.drug));
    rows.push(cooldown("medical", "\u{E95E}", "Medical cooldown", user.medical));
    rows.push(cooldown("booster", "\u{E95E}", "Booster cooldown", user.booster));
    if let Some(left) = user.education_left {
        let value = match left.saturating_sub(elapsed) {
            0 => "Finished".to_string(),
            left => long_clock(left),
        };
        rows.push(Row::info("education", "\u{E7BE}", "Education", value));
    }
    if let Some(left) = user.crime_left(elapsed) {
        let value = match left {
            0 => "Ready".to_string(),
            left => long_clock(left),
        };
        rows.push(Row::info("crime", "\u{E716}", "Organized crime", value));
    }
    if user.race_finished() {
        rows.push(Row::info("race", "\u{E7C1}", "Race", "Finished"));
    }
    if user.bank_ready() {
        rows.push(Row::info("bank", "\u{E8C7}", "Bank investment", "Ready"));
    }
    rows.push(Row::info(
        "updated",
        "\u{E72C}",
        "Updated",
        format!("{} ago", clock(elapsed)),
    ));
    if let Some(faction) = &snapshot.faction {
        if let Some(war) = war_now(faction, server_now) {
            rows.push(
                Row::info(
                    "war",
                    "\u{E7FC}",
                    "Ranked war",
                    format!("{} : {}", war.ours, war.theirs),
                )
                .detail(format!("against {}", war.enemy))
                .wide(),
            );
        } else if let Some(war) = &faction.war
            && war.start > server_now
        {
            rows.push(
                Row::info(
                    "war",
                    "\u{E7FC}",
                    "Ranked war",
                    format!("starts in {}", clock(war.start - server_now)),
                )
                .detail(format!("against {}", war.enemy))
                .wide(),
            );
        }
        let chain_left = faction
            .chain_timeout
            .saturating_sub(now.saturating_sub(faction.fetched));
        if faction.chain > 0 && chain_left > 0 {
            rows.push(Row {
                meter: Some(
                    (faction.chain.min(faction.chain_max) * 100)
                        .checked_div(faction.chain_max)
                        .unwrap_or(0) as u32,
                ),
                ..Row::info(
                    "chain",
                    "\u{E71B}",
                    "Chain",
                    format!("{}/{}", faction.chain, faction.chain_max),
                )
                .detail(format!("breaks in {}", clock(chain_left)))
                .wide()
            });
        }
    }
    rows.push(open);
    rows
}

/// The tooltip: everything, one line each.
pub fn tip(snapshot: &Snapshot, now: u64) -> String {
    let mut lines = Vec::new();
    if let Some(error) = &snapshot.error {
        lines.push(error.clone());
    }
    let Some(user) = &snapshot.user else {
        if lines.is_empty() {
            lines.push("Connecting to Torn...".to_string());
        }
        return lines.join("\n");
    };
    let elapsed = now.saturating_sub(user.fetched);
    let server_now = user.server_time + elapsed;
    if elapsed > STALE_AFTER {
        lines.push(format!("Last update {} ago", clock(elapsed)));
    }
    let flight = user.travel_left.saturating_sub(elapsed);
    let held = user.until.saturating_sub(server_now);
    lines.push(format!("{} [{}]", user.name, user.level));
    lines.push(if flight > 0 {
        format!("Traveling to {}, {}", user.destination, clock(flight))
    } else if held > 0 && matches!(user.state.as_str(), "Hospital" | "Jail") {
        format!("{} for {}", user.state, clock(held))
    } else if user.description.is_empty() {
        user.state.clone()
    } else {
        user.description.clone()
    });
    lines.push(format!("Cash {}", money(user.money)));
    let bar = |name: &str, bar: Bar| {
        let left = bar.fulltime.saturating_sub(elapsed);
        if bar.current >= bar.maximum || left == 0 {
            format!("{name} {}/{} full", bar.current.max(bar.maximum), bar.maximum)
        } else {
            format!("{name} {}/{}, full in {}", bar.current, bar.maximum, clock(left))
        }
    };
    lines.push(bar("Energy", user.energy));
    lines.push(bar("Nerve", user.nerve));
    lines.push(format!(
        "Life {}/{}, happy {}/{}",
        user.life.current, user.life.maximum, user.happy.current, user.happy.maximum
    ));
    let cooldown = |name: &str, seconds: u64| match seconds.saturating_sub(elapsed) {
        0 => format!("{name} ready"),
        left => format!("{name} {}", clock(left)),
    };
    lines.push(format!(
        "{}, {}, {}",
        cooldown("Drug", user.drug),
        cooldown("Medical", user.medical),
        cooldown("Booster", user.booster)
    ));
    if let Some(left) = user.education_left {
        lines.push(match left.saturating_sub(elapsed) {
            0 => "Education finished".to_string(),
            left => format!("Education {}", long_clock(left)),
        });
    }
    if let Some(left) = user.crime_left(elapsed) {
        lines.push(match left {
            0 => "Organized crime ready".to_string(),
            left => format!("Organized crime {}", long_clock(left)),
        });
    }
    if user.race_finished() {
        lines.push("Race finished".to_string());
    }
    if user.bank_ready() {
        lines.push("Bank investment ready".to_string());
    }
    if let Some(faction) = &snapshot.faction {
        if let Some(war) = war_now(faction, server_now) {
            lines.push(format!("War {} : {} {}", war.ours, war.theirs, war.enemy));
        }
        let chain_left = faction
            .chain_timeout
            .saturating_sub(now.saturating_sub(faction.fetched));
        if faction.chain > 0 && chain_left > 0 {
            lines.push(format!(
                "Chain {}/{}, {}",
                faction.chain,
                faction.chain_max,
                clock(chain_left)
            ));
        }
    }
    if user.messages + user.events > 0 {
        lines.push(format!("{} messages, {} events", user.messages, user.events));
    }
    lines.join("\n")
}

/// One GET through curl.exe. The URL holds the key, so it goes in on stdin
/// and never shows in the process list.
fn get(url: &str) -> Result<String, String> {
    let config = format!("url = \"{}\"\n", url.replace(['\\', '"'], ""));
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let mut child = Command::new(format!(r"{root}\System32\curl.exe"))
        .args(["--silent", "--show-error", "--max-time", "15", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("curl.exe: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|e| format!("curl.exe: {e}"))?;
    }
    let output = child.wait_with_output().map_err(|e| format!("curl.exe: {e}"))?;
    if !output.status.success() {
        return Err("Offline, retrying".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = r#"{"server_time":1800000000,"name":"Test_Player","level":42,
        "energy":{"current":120,"maximum":150,"fulltime":1800},
        "nerve":{"current":30,"maximum":45,"fulltime":900},
        "life":{"current":3000,"maximum":4000,"fulltime":600},
        "happy":{"current":5025,"maximum":5025,"fulltime":0},
        "travel":{"destination":"Torn","timestamp":0,"departed":0,"time_left":0},
        "cooldowns":{"drug":9910,"medical":3600,"booster":40000},
        "status":{"description":"Okay","state":"Okay","until":0},
        "faction":{"faction_id":1000,"faction_name":"Test Faction"},
        "money_onhand":145250000,"notifications":{"messages":0,"events":0}}"#;
    const FACTION: &str = r#"{"wars":{"ranked":{"war_id":1,"start":1799990000,"end":null,"target":20000,
        "factions":[{"id":1000,"name":"Test Faction","score":15200,"chain":40},
        {"id":2000,"name":"Enemy Faction","score":13400,"chain":12}]},"raids":[],"territory":[]},
        "chain":{"id":1,"current":120,"max":250,"timeout":200,"cooldown":0}}"#;

    fn user() -> User {
        parse_user(USER, 5000).unwrap()
    }

    fn icons(snapshot: &Snapshot, now: u64) -> Vec<(Icon, String)> {
        chips(snapshot, now)
            .into_iter()
            .map(|chip| (chip.icon, chip.text))
            .collect()
    }

    fn with(user: User) -> Snapshot {
        Snapshot {
            user: Some(user),
            ..Default::default()
        }
    }

    #[test]
    fn replies_are_read() {
        let user = user();
        assert_eq!(
            (user.energy.current, user.energy.maximum, user.energy.fulltime),
            (120, 150, 1800)
        );
        assert_eq!((user.money, user.faction_id, user.level), (145_250_000, 1000, 42));
        assert_eq!((user.drug, user.medical, user.booster), (9910, 3600, 40_000));
        let faction = parse_faction(FACTION, 1000, 5000).unwrap();
        let war = faction.war.clone().unwrap();
        assert_eq!((war.ours, war.theirs, war.end), (15_200, 13_400, 0));
        assert_eq!(war.enemy, "Enemy Faction");
        assert_eq!(
            (faction.chain, faction.chain_max, faction.chain_timeout),
            (120, 250, 200)
        );
        // The same reply read from the other side.
        assert_eq!(parse_faction(FACTION, 2000, 0).unwrap().war.unwrap().ours, 13_400);
        assert_eq!(
            parse_user(r#"{"error":{"code":2,"error":"Incorrect key"}}"#, 0),
            Err(BAD_KEY.to_string())
        );
        assert_eq!(
            parse_user(r#"{"error":{"code":5,"error":"Too many requests"}}"#, 0),
            Err("Torn: Too many requests".to_string())
        );
        assert!(parse_user("<html>", 0).is_err());
    }

    #[test]
    fn a_quiet_player_shows_one_idle_icon() {
        assert_eq!(icons(&with(user()), 5000), [(Icon::Idle, String::new())]);
        assert_eq!(icons(&Snapshot::default(), 5000), [(Icon::Idle, String::new())]);
        let failed = Snapshot {
            error: Some("Offline".into()),
            ..Default::default()
        };
        assert_eq!(icons(&failed, 5000), [(Icon::Warning, String::new())]);
    }

    #[test]
    fn flights_count_down_and_point_the_right_way() {
        let mut flying = user();
        flying.state = "Traveling".into();
        flying.destination = "Mexico".into();
        flying.travel_left = 1500;
        assert_eq!(icons(&with(flying.clone()), 5000), [(Icon::PlaneOut, "25:00".into())]);
        assert_eq!(icons(&with(flying.clone()), 5010), [(Icon::PlaneOut, "24:50".into())]);
        flying.destination = "Torn".into();
        flying.travel_left = 3 * 3600 + 62;
        assert_eq!(
            icons(&with(flying.clone()), 5000),
            [(Icon::PlaneHome, "3:01:02".into())]
        );
        // Landed, by the local clock, before the next fetch says so.
        assert_eq!(icons(&with(flying), 5000 + 4 * 3600)[0].0, Icon::Warning);
    }

    #[test]
    fn hospital_and_jail_count_down_on_the_server_clock() {
        let mut held = user();
        held.state = "Hospital".into();
        held.until = held.server_time + 600;
        assert_eq!(icons(&with(held.clone()), 5000), [(Icon::Hospital, "10:00".into())]);
        assert_eq!(icons(&with(held.clone()), 5030), [(Icon::Hospital, "9:30".into())]);
        held.state = "Jail".into();
        assert_eq!(icons(&with(held.clone()), 5030), [(Icon::Jail, "9:30".into())]);
        // Out, by the local clock, before the next fetch says so.
        assert!(
            icons(&with(held), 5000 + 600)
                .iter()
                .all(|(icon, _)| *icon != Icon::Jail)
        );
    }

    #[test]
    fn full_bars_appear_only_when_full() {
        let mut player = user();
        player.energy.current = 150;
        assert_eq!(icons(&with(player.clone()), 5000), [(Icon::Energy, String::new())]);
        // Nerve fills 900 s after the fetch.
        assert_eq!(icons(&with(player.clone()), 5000 + 60).len(), 1);
        player.energy.current = 10;
        player.fetched = 5000;
        let later = icons(&with(player), 5000 + 80);
        assert_eq!(later, [(Icon::Idle, String::new())]);
    }

    #[test]
    fn war_and_chain_show_scores_and_the_time_left() {
        let snapshot = Snapshot {
            user: Some(user()),
            faction: Some(parse_faction(FACTION, 1000, 5000).unwrap()),
            error: None,
        };
        assert_eq!(
            icons(&snapshot, 5000),
            [(Icon::War, "15.2k:13.4k".into()), (Icon::Chain, "120 3:20".into())]
        );
        let mut ending = snapshot.clone();
        ending.faction.as_mut().unwrap().chain_timeout = 50;
        let last_minute = chips(&ending, 5000);
        assert!(last_minute[1].alert && last_minute[1].text == "120 0:50");
        // The chain broke; the war goes on.
        assert_eq!(icons(&ending, 5000 + 60), [(Icon::War, "15.2k:13.4k".into())]);
        let mut soon = snapshot.clone();
        soon.faction.as_mut().unwrap().war.as_mut().unwrap().start = 1_800_000_000 + 7200;
        assert_eq!(icons(&soon, 5000)[0], (Icon::War, "2:00:00".into()));
    }

    #[test]
    fn finished_things_appear_from_the_status_icons() {
        let reply = USER.replacen(
            '{',
            r#"{"education_timeleft":120,"icons":{
                "icon19":"Education - Currently completing the Criminal Law course - 2 minutes and 0 seconds",
                "icon18":"Racing - You finished 2nd in the Parkland race and have received 2 racing points!",
                "icon85":"Organized Crime - Muscle #2 in Honey Trap - 1 minute and 21 seconds",
                "icon29":"Bank Investment - Current bank investment worth $2 - 65 days, 0 hours, 23 minutes and 53 seconds",
                "icon45":"Medical Cooldown - 02:09:02 / 07:00:00"},"#,
            1,
        );
        let mut player = parse_user(&reply, 5000).unwrap();
        assert_eq!(player.education_left, Some(120));
        assert_eq!(player.crime_left(0), Some(81));
        assert!(player.race_finished() && !player.bank_ready());
        assert_eq!(icons(&with(player.clone()), 5000), [(Icon::Race, String::new())]);
        // The crime is ready after 81 s; the course ends after 120 s, past the stale mark.
        assert_eq!(
            icons(&with(player.clone()), 5085),
            [(Icon::Race, String::new()), (Icon::Crime, String::new())]
        );
        player.fetched = 5100;
        player.education_left = Some(20);
        assert!(icons(&with(player.clone()), 5120).contains(&(Icon::Education, String::new())));
        // Abroad, the race's result has to wait.
        player.state = "Abroad".into();
        assert!(!icons(&with(player.clone()), 5100).contains(&(Icon::Race, String::new())));
        player.icons = vec!["Bank Investment - Your investment is ready to be collected".into()];
        assert!(icons(&with(player.clone()), 5100).contains(&(Icon::Bank, String::new())));
        let panel = rows(&with(player), 5100);
        assert!(panel.iter().any(|row| row.id == "education" && row.value == "0:20"));
        assert!(panel.iter().any(|row| row.id == "bank"));
        assert_eq!(duration("Loan - Outstanding loan of $23,000,000"), None);
        assert_eq!(
            duration("X - 2 days, 10 hours, 38 minutes and 39 seconds"),
            Some(211_119)
        );
        assert_eq!(long_clock(211_119), "2d 10h");
        // A reply without education says nothing about it.
        assert_eq!(user().education_left, None);
    }

    #[test]
    fn texts_and_widths() {
        assert_eq!(clock(59), "0:59");
        assert_eq!(clock(3600), "1:00:00");
        assert_eq!(money(145_250_000), "$145,250,000");
        assert_eq!(money(999), "$999");
        assert_eq!(score(9999), "9999");
        let idle = Chip::new(Icon::Idle, "");
        let timer = Chip::new(Icon::Jail, "9:30");
        assert_eq!(idle.width(), CHIP_ICON);
        assert!(timer.width() > idle.width());
        assert_eq!(width(&[idle.clone(), timer.clone()]), idle.width() + timer.width());
        let panel = rows(&with(user()), 5000);
        let find = |id: &str| panel.iter().find(|row| row.id == id).unwrap();
        assert_eq!(find("status").value, "Okay");
        assert_eq!(find("cash").value, "$145,250,000");
        assert_eq!(
            (find("energy").meter, find("energy").value.as_str()),
            (Some(80), "120/150")
        );
        assert_eq!(find("energy").detail, "full in 30:00");
        assert_eq!(find("happy").meter, Some(100));
        assert_eq!(find("drug").value, "2:45:10");
        assert_eq!(panel.last().unwrap().link, Some(HOME));
        let both = Snapshot {
            user: Some(user()),
            faction: Some(parse_faction(FACTION, 1000, 5000).unwrap()),
            error: None,
        };
        let panel = rows(&both, 5000);
        let chain = panel.iter().find(|row| row.id == "chain").unwrap();
        assert_eq!((chain.meter, chain.value.as_str()), (Some(48), "120/250"));
        assert!(panel.iter().any(|row| row.id == "war" && row.value == "15200 : 13400"));
        let card = card(&both, 5000);
        assert!(card.iter().all(|row| !matches!(row.id, "open" | "updated" | "unread")));
        assert!(card.iter().any(|row| row.label == "Drug" && row.value == "2:45:10"));
        assert_eq!(
            card_height(&card),
            2.0 * CARD_PAD + card.iter().map(Row::card_height).sum::<f32>()
        );
        assert!(card_height(&card) < 320.0);
        let tip = tip(&with(user()), 5000);
        assert!(tip.contains("Cash $145,250,000") && tip.contains("Energy 120/150, full in 30:00"));
    }
}

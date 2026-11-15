//! A Toggl Track timer for the dock. Private to this fork.
//!
//! The dock shows the running entry and the launcher starts one with
//! `t <what I'm doing>`. A worker thread talks to Toggl through the curl.exe
//! that ships with Windows, so neither the dock nor typing waits on the
//! network. The dock counts the seconds itself from the entry's start time.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::messages::WM_TOGGL;
use crate::search::{Action, Kind, ResultItem};

/// The launcher query that starts a timer: `t writing the report`.
pub const PREFIX: &str = "t ";
/// What a launcher row opens to start or stop the timer.
pub const START_LINK: &str = "wisp-toggl:start:";
pub const STOP_LINK: &str = "wisp-toggl:stop";

const API: &str = "https://api.track.toggl.com/api/v9";
/// Catches a timer started or stopped in another Toggl app. Toggl's free plan
/// allows 30 requests an hour, so this leaves most of them for starts and stops.
const POLL: Duration = Duration::from_secs(600);
/// After Toggl says the hour's requests are used up, nothing is asked for this long.
const LIMIT_PAUSE: Duration = Duration::from_secs(3600);
const LIMIT: &str = "Toggl's hourly request limit is used up. Try again within the hour";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub id: u64,
    pub workspace: u64,
    /// Seconds since the Unix epoch.
    pub start: u64,
    pub description: String,
}

enum Request {
    Start(String),
    Stop,
    Poll,
}

#[derive(Default)]
struct Shared {
    token: Mutex<String>,
    running: Mutex<Option<Running>>,
    error: Mutex<Option<String>>,
    sender: Mutex<Option<Sender<Request>>>,
    /// The dock's window, told when the timer changes.
    notify: AtomicIsize,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(Shared::default)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Takes the API token from the config. An empty token turns the timer off.
pub fn configure(token: &str) {
    let s = shared();
    let token = token.trim();
    let changed = {
        let mut current = lock(&s.token);
        let changed = *current != token;
        *current = token.to_string();
        changed
    };
    if token.is_empty() {
        *lock(&s.running) = None;
        *lock(&s.error) = None;
        if changed {
            notify();
        }
        return;
    }
    let mut sender = lock(&s.sender);
    if sender.is_none() {
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("toggl".into())
            .spawn(move || worker(rx));
        match spawned {
            Ok(_) => *sender = Some(tx),
            Err(error) => eprintln!("Toggl: {error}"),
        }
    }
    if changed && let Some(sender) = sender.as_ref() {
        let _ = sender.send(Request::Poll);
    }
}

pub fn enabled() -> bool {
    !lock(&shared().token).is_empty()
}

pub fn running() -> Option<Running> {
    lock(&shared().running).clone()
}

/// The last failure, until a request succeeds.
pub fn error() -> Option<String> {
    lock(&shared().error).clone()
}

pub fn set_notify(hwnd: HWND) {
    shared().notify.store(hwnd.0 as isize, Ordering::Relaxed);
}

/// Starts a timer. The dock shows it at once; Toggl confirms it a moment later.
pub fn start(description: &str) {
    let description = description.trim().to_string();
    *lock(&shared().running) = Some(Running {
        id: 0,
        workspace: 0,
        start: now(),
        description: description.clone(),
    });
    notify();
    send(Request::Start(description));
}

pub fn stop() {
    *lock(&shared().running) = None;
    notify();
    send(Request::Stop);
}

/// Runs a launcher row's link. `false` means the link is not one of ours.
pub fn open(link: &str) -> bool {
    if let Some(description) = link.strip_prefix(START_LINK) {
        start(description);
        true
    } else if link == STOP_LINK {
        stop();
        true
    } else {
        false
    }
}

/// The launcher's rows for a `t ...` query: start a timer, and stop the one that runs.
pub fn results(text: &str) -> Vec<ResultItem> {
    let Some(description) = query(text).filter(|_| enabled()) else {
        return Vec::new();
    };
    let running = running();
    let mut rows = Vec::new();
    if !description.is_empty() || running.is_none() {
        let title = if description.is_empty() {
            "Start a timer".to_string()
        } else {
            format!("Start timer: {description}")
        };
        rows.push(
            ResultItem::new(
                Kind::Command,
                title,
                "Toggl Track",
                Action::Open(format!("{START_LINK}{description}")),
            )
            .glyph("\u{E916}")
            .verb("Start"),
        );
    }
    if let Some(entry) = running {
        let title = if entry.description.is_empty() {
            "Stop the timer".to_string()
        } else {
            format!("Stop timer: {}", entry.description)
        };
        rows.push(
            ResultItem::new(
                Kind::Command,
                title,
                format!("Toggl Track, {}", elapsed_text(entry.start, now())),
                Action::Open(STOP_LINK.to_string()),
            )
            .glyph("\u{E71A}")
            .verb("Stop"),
        );
    }
    rows
}

/// The text after `t `, if the query starts a timer.
pub fn query(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let head = text.get(..PREFIX.len())?;
    head.eq_ignore_ascii_case(PREFIX).then(|| text[PREFIX.len()..].trim())
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// "0:07:05" for the dock.
pub fn elapsed_text(start: u64, now: u64) -> String {
    let seconds = now.saturating_sub(start);
    format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
}

fn send(request: Request) {
    if let Some(sender) = lock(&shared().sender).as_ref() {
        let _ = sender.send(request);
    }
}

fn notify() {
    let hwnd = shared().notify.load(Ordering::Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_TOGGL, WPARAM(0), LPARAM(0));
        }
    }
}

fn worker(requests: Receiver<Request>) {
    // The worker's own copy is what Toggl confirmed; the shared one may be ahead of it.
    let mut current: Option<Running> = None;
    let mut workspace: Option<u64> = None;
    let mut paused: Option<Instant> = None;
    loop {
        let request = match requests.recv_timeout(POLL) {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) => Request::Poll,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // A start or stop is worth one more try; a poll is not.
        if matches!(request, Request::Poll) && paused.is_some_and(|until| Instant::now() < until) {
            continue;
        }
        let token = lock(&shared().token).clone();
        if token.is_empty() {
            current = None;
            continue;
        }
        let result = match request {
            Request::Start(description) => start_entry(&token, &mut workspace, &description).map(Some),
            Request::Stop => match current.take() {
                Some(entry) => stop_entry(&token, &entry).map(|()| None),
                // Stopped before Toggl knew of it, or already stopped elsewhere.
                None => current_entry(&token).and_then(|entry| match entry {
                    Some(entry) => stop_entry(&token, &entry).map(|()| None),
                    None => Ok(None),
                }),
            },
            Request::Poll => current_entry(&token),
        };
        // After a failure the dock goes back to what Toggl last confirmed.
        let s = shared();
        match result {
            Ok(entry) => {
                current = entry;
                paused = None;
                *lock(&s.error) = None;
            }
            Err(error) => {
                if error == LIMIT {
                    paused = Some(Instant::now() + LIMIT_PAUSE);
                }
                eprintln!("Toggl: {error}");
                *lock(&s.error) = Some(error);
            }
        }
        *lock(&s.running) = current.clone();
        notify();
    }
}

fn current_entry(token: &str) -> Result<Option<Running>, String> {
    let body = http(token, "GET", "/me/time_entries/current", None)?;
    parse_entry(&body)
}

fn start_entry(token: &str, workspace: &mut Option<u64>, description: &str) -> Result<Running, String> {
    let id = match *workspace {
        Some(id) => id,
        None => {
            let me = http(token, "GET", "/me", None)?;
            let id = parse_workspace(&me)?;
            *workspace = Some(id);
            id
        }
    };
    let body = serde_json::json!({
        "created_with": "wisp",
        "description": description,
        "workspace_id": id,
        "start": iso(now()),
        "duration": -1,
    })
    .to_string();
    let reply = http(token, "POST", &format!("/workspaces/{id}/time_entries"), Some(&body))?;
    parse_entry(&reply)?.ok_or_else(|| "Toggl did not return the new entry".to_string())
}

fn stop_entry(token: &str, entry: &Running) -> Result<(), String> {
    let path = format!("/workspaces/{}/time_entries/{}/stop", entry.workspace, entry.id);
    http(token, "PATCH", &path, None).map(|_| ())
}

fn parse_workspace(body: &str) -> Result<u64, String> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("Toggl's reply: {e}"))?;
    value["default_workspace_id"]
        .as_u64()
        .ok_or_else(|| "Toggl's reply has no default workspace".to_string())
}

/// `null` means no timer is running.
fn parse_entry(body: &str) -> Result<Option<Running>, String> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).map_err(|e| format!("Toggl's reply: {e}"))?;
    if value.is_null() {
        return Ok(None);
    }
    let field = |name: &str| format!("Toggl's reply has no {name}");
    Ok(Some(Running {
        id: value["id"].as_u64().ok_or_else(|| field("id"))?,
        workspace: value["workspace_id"].as_u64().ok_or_else(|| field("workspace_id"))?,
        start: value["start"]
            .as_str()
            .and_then(parse_iso)
            .ok_or_else(|| field("start time"))?,
        description: value["description"].as_str().unwrap_or_default().to_string(),
    }))
}

/// One request through curl.exe. The token and body go in on stdin, so they
/// never show in the process list.
fn http(token: &str, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
    let quote = |text: &str| {
        let mut quoted = String::with_capacity(text.len() + 2);
        quoted.push('"');
        for c in text.chars() {
            match c {
                '\\' => quoted.push_str("\\\\"),
                '"' => quoted.push_str("\\\""),
                '\n' => quoted.push_str("\\n"),
                '\r' => quoted.push_str("\\r"),
                '\t' => quoted.push_str("\\t"),
                c => quoted.push(c),
            }
        }
        quoted.push('"');
        quoted
    };
    let mut config = format!(
        "url = {}\nuser = {}\nrequest = {}\nwrite-out = \"\\n%{{http_code}}\"\n",
        quote(&format!("{API}{path}")),
        quote(&format!("{token}:api_token")),
        quote(method),
    );
    match body {
        Some(body) => {
            config.push_str("header = \"Content-Type: application/json\"\n");
            config.push_str(&format!("data = {}\n", quote(body)));
        }
        None if method != "GET" => config.push_str("header = \"Content-Length: 0\"\n"),
        None => {}
    }
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
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if message.is_empty() {
            "No connection to Toggl".to_string()
        } else {
            message
        });
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let (reply, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
    match code.trim().parse::<u16>() {
        Ok(code) if (200..300).contains(&code) => Ok(reply.to_string()),
        Ok(403 | 401) => Err("Toggl refused the API token".to_string()),
        Ok(402 | 429) => Err(LIMIT.to_string()),
        Ok(code) => Err(format!("Toggl answered {code}: {}", reply.trim())),
        Err(_) => Err("Toggl's reply could not be read".to_string()),
    }
}

/// Days since 1970-01-01 for a date in the Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year.rem_euclid(400);
    let of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    era * 146_097 + of_era * 365 + of_era / 4 - of_era / 100 + of_year - 719_468
}

/// "2026-11-15T12:34:56Z", in UTC.
fn iso(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let of_era = days.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * of_year + 2) / 153;
    let day = of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 { shifted + 3 } else { shifted - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// Reads "2026-11-15T12:34:56Z", with fractional seconds or a "+01:00" offset.
fn parse_iso(text: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let days = days_from_civil(number(0..4)?, number(5..7)?, number(8..10)?);
    let mut seconds = days * 86_400 + number(11..13)? * 3600 + number(14..16)? * 60 + number(17..19)?;
    let zone = text
        .get(19..)?
        .trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(sign) = zone.chars().next().filter(|c| matches!(c, '+' | '-')) {
        let (hours, minutes) = zone[1..].split_once(':')?;
        let offset = hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60;
        seconds -= if sign == '+' { offset } else { -offset };
    }
    u64::try_from(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_times_round_trip() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_794_746_096), "2026-11-15T12:34:56Z");
        assert_eq!(parse_iso("2026-11-15T12:34:56Z"), Some(1_794_746_096));
        assert_eq!(parse_iso("2026-11-15T12:34:56.123456Z"), Some(1_794_746_096));
        assert_eq!(parse_iso("2026-11-15T13:34:56+01:00"), Some(1_794_746_096));
        assert_eq!(parse_iso("2026-11-15T07:04:56-05:30"), Some(1_794_746_096));
        assert_eq!(
            parse_iso("2024-02-29T00:00:00Z").map(iso).as_deref(),
            Some("2024-02-29T00:00:00Z")
        );
        assert_eq!(parse_iso("yesterday"), None);
    }

    #[test]
    fn elapsed_time_reads_as_a_stopwatch() {
        assert_eq!(elapsed_text(100, 100), "0:00:00");
        assert_eq!(elapsed_text(100, 100 + 7 * 60 + 5), "0:07:05");
        assert_eq!(elapsed_text(0, 12 * 3600 + 59), "12:00:59");
        // A clock that runs behind Toggl's never shows a negative time.
        assert_eq!(elapsed_text(200, 100), "0:00:00");
    }

    #[test]
    fn replies_from_toggl_are_read() {
        assert_eq!(parse_entry("null"), Ok(None));
        let entry = parse_entry(
            r#"{"id":7,"workspace_id":3,"description":"write \"cv\"","start":"2026-11-15T12:34:56+00:00","duration":-1,"tags":[]}"#,
        );
        assert_eq!(
            entry,
            Ok(Some(Running {
                id: 7,
                workspace: 3,
                start: 1_794_746_096,
                description: "write \"cv\"".into(),
            }))
        );
        assert_eq!(
            parse_entry(r#"{"id":7,"workspace_id":3,"description":null,"start":"2026-11-15T12:34:56Z"}"#)
                .map(|entry| entry.map(|entry| entry.description)),
            Ok(Some(String::new()))
        );
        assert!(parse_entry("<html>").is_err());
        assert_eq!(parse_workspace(r#"{"default_workspace_id":42}"#), Ok(42));
    }

    #[test]
    fn launcher_queries_and_links() {
        assert_eq!(query("t write the report "), Some("write the report"));
        assert_eq!(query("  T x"), Some("x"));
        assert_eq!(query("t "), Some(""));
        assert_eq!(query("time"), None);
        assert_eq!(query("t"), None);
        assert!(!open("https://example.com"));
    }
}

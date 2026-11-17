//! CodeTime for OpenIDEA: a privacy-first coding-time tracker. Every file
//! opened, saved or focused is a heartbeat, and a heartbeat credits the time
//! since the previous one to the file that stretch was spent on (so the work
//! before a save, a switch or losing focus counts), until the idle threshold
//! passes. The session — which file, since when — is persisted to the
//! plugin's storage and restored on activation, so it survives the host
//! reclaiming the plugin's idle instance; the host's scheduled wakeup
//! (delivered only to an instance it just recreated) settles or expires it.
//! The totals land in per-day buckets in the plugin's storage, which the
//! status bar reads for today's `H:MM` and the dashboard for today and the
//! last 7 days: per project, per language and the files with the most time.
//!
//! Nothing leaves the machine: no network, no keystrokes (the plugin asks for
//! no `events.input`), only the file events the IDE already publishes.

use openidea_plugin::host::{self, storage};
use openidea_plugin::views::{Body, Button, Table, View, ViewEvent, Viewport, Views};
use openidea_plugin::{Context, Error, Event, Invocation, LogLevel, Plugin, StatusItem, register_plugin};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Storage keys of the day buckets: `d:YYYYMMDD` → a [`DayBucket`] as JSON.
const DAY: &str = "d:";
/// Storage key of the accounting session: a [`Session`] as JSON, persisted
/// so the stretch of work since the last heartbeat survives the host
/// reclaiming the plugin's idle instance.
const SESSION: &str = "session";
/// The last day (`YYYYMMDD`) pruning ran, so it runs once per day.
const PRUNED: &str = "pruned-day";
/// Day buckets older than this many days are deleted (only the 7-day
/// dashboard reads older days, so 90 is generous).
const RETENTION_DAYS: u64 = 90;
/// At most this many files are kept per day (the largest; the dashboard shows
/// the top few), so one day's key stays small and storage lasts.
const MAX_FILES: usize = 100;
/// How many files the dashboard lists.
const TOP_FILES: usize = 10;
/// The status bar item.
const ITEM: &str = "codetime";
/// Rows the IDE gets when it asks for no viewport.
const DEFAULT_ROWS: u32 = 200;

// ---------------------------------------------------------------------------
// Time: UTC calendar days. The host has no timezone API, so a "day" is a UTC
// day; the README says so.
// ---------------------------------------------------------------------------

/// `secs` since the epoch as the UTC day `YYYYMMDD` (the storage key's day
/// part). Lexicographic order of these keys is chronological.
pub fn day_key(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}{m:02}{d:02}")
}

/// Days since the epoch → proleptic Gregorian year, month, day
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `secs` as `H:MM` (minutes floored), how the status bar and the dashboard
/// show durations: `0:47`, `2:05`, `12:34`.
pub fn format_hm(secs: u64) -> String {
    format!("{}:{:02}", secs / 3600, (secs % 3600) / 60)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Globs (the `exclude` setting)
// ---------------------------------------------------------------------------

/// `pattern` matches `path`: `*` runs within a path segment, `**` runs across
/// segments, `?` is one character of a segment. A pattern without `/` matches
/// the file name alone (`*.min.js`). Backslashes count as separators.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let path = path.replace('\\', "/");
    if !pattern.contains('/') && match_from(pattern.as_bytes(), file_name(&path).as_bytes()) {
        return true;
    }
    match_from(pattern.as_bytes(), path.as_bytes())
}

fn match_from(p: &[u8], t: &[u8]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some(b'*') => {
            let double = p.get(1) == Some(&b'*');
            let rest = if double { &p[2..] } else { &p[1..] };
            if match_from(rest, t) {
                return true;
            }
            for i in 0..t.len() {
                if !double && t[i] == b'/' {
                    break;
                }
                if match_from(rest, &t[i + 1..]) {
                    return true;
                }
            }
            false
        }
        Some(b'?') => t.first().is_some_and(|&c| c != b'/') && match_from(&p[1..], &t[1..]),
        Some(&c) => t.first() == Some(&c) && match_from(&p[1..], &t[1..]),
    }
}

/// Everything after the last separator (the whole text when there is none).
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The `exclude` globs, as the user set them (default: none).
fn excluded_globs() -> Vec<String> {
    host::setting("exclude")
        .and_then(|v| serde_json::from_str::<Vec<String>>(&v).ok())
        .unwrap_or_default()
}

fn is_excluded(path: &str, globs: &[String]) -> bool {
    globs.iter().any(|g| glob_match(g, path))
}

// ---------------------------------------------------------------------------
// Files, languages, projects
// ---------------------------------------------------------------------------

/// A display language for the file, from its extension.
pub fn language_of(path: &str) -> String {
    let path = path.replace('\\', "/");
    let name = file_name(&path);
    let Some((_, ext)) = name.rsplit_once('.') else { return "Other".into() };
    match ext.to_ascii_lowercase().as_str() {
        "rs" => "Rust",
        "js" | "mjs" | "cjs" | "jsx" => "JavaScript",
        "ts" | "tsx" | "mts" | "cts" => "TypeScript",
        "py" => "Python",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "go" => "Go",
        "c" | "h" => "C",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "C++",
        "cs" => "C#",
        "rb" => "Ruby",
        "php" => "PHP",
        "swift" => "Swift",
        "scala" => "Scala",
        "dart" => "Dart",
        "lua" => "Lua",
        "hs" => "Haskell",
        "ex" | "exs" => "Elixir",
        "erl" => "Erlang",
        "clj" | "cljs" | "cljc" => "Clojure",
        "groovy" => "Groovy",
        "html" | "htm" => "HTML",
        "css" => "CSS",
        "scss" => "SCSS",
        "less" => "Less",
        "json" | "jsonc" => "JSON",
        "md" | "markdown" => "Markdown",
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        "xml" => "XML",
        "sh" | "bash" | "zsh" => "Shell",
        "sql" => "SQL",
        "vue" => "Vue",
        "svelte" => "Svelte",
        other => return other.to_ascii_uppercase(),
    }
    .into()
}

/// The project name shown in the dashboard: the file's open project's root
/// folder name, else the window's project, else "Other".
fn project_of(path: &str, ctx: &Invocation) -> String {
    let root = host::project_of(path).and_then(|p| p.root().ok()).or_else(|| ctx.project().and_then(|p| p.root().ok()));
    match root {
        Some(r) => file_name(&r.replace('\\', "/")).to_owned(),
        None => "Other".into(),
    }
}

/// The last three segments of the path (`…` when it is longer), how the
/// dashboard names files.
pub fn shorten(path: &str) -> String {
    let path = path.replace('\\', "/");
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 3 {
        return path;
    }
    format!("…/{}", parts[parts.len() - 3..].join("/"))
}

// ---------------------------------------------------------------------------
// Storage: one JSON day bucket per day, pruned by age
// ---------------------------------------------------------------------------

/// The times of one UTC day: the total and the per-project, per-language and
/// per-file splits. Serialized as the value of the `d:YYYYMMDD` storage key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayBucket {
    pub total: u64,
    pub projects: BTreeMap<String, u64>,
    pub langs: BTreeMap<String, u64>,
    pub files: BTreeMap<String, u64>,
}

impl DayBucket {
    /// Adds `secs` to one file's project, language and file splits.
    pub fn add(&mut self, project: &str, lang: &str, file: &str, secs: u64) {
        self.total += secs;
        *self.projects.entry(project.to_owned()).or_default() += secs;
        *self.langs.entry(lang.to_owned()).or_default() += secs;
        *self.files.entry(file.to_owned()).or_default() += secs;
        trim(&mut self.files, MAX_FILES);
    }

    /// Sums the buckets of several days.
    pub fn merge(&mut self, other: &DayBucket) {
        self.total += other.total;
        for (k, v) in &other.projects {
            *self.projects.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &other.langs {
            *self.langs.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &other.files {
            *self.files.entry(k.clone()).or_default() += v;
        }
        trim(&mut self.files, MAX_FILES);
    }
}

/// Keeps the `max` largest entries (a tie drops the greatest key), so a
/// bucket cannot grow without bound.
pub fn trim(map: &mut BTreeMap<String, u64>, max: usize) {
    while map.len() > max {
        let smallest = map
            .iter()
            .min_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(k, _)| k.clone())
            .expect("non-empty");
        map.remove(&smallest);
    }
}

/// `map` as rows ordered by time (descending), ties by name.
pub fn top(map: &BTreeMap<String, u64>, max: usize) -> Vec<(String, u64)> {
    let mut rows: Vec<(String, u64)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(max);
    rows
}

fn load_day(day: &str) -> Result<DayBucket, Error> {
    match storage::get(&format!("{DAY}{day}"))? {
        Some(json) => serde_json::from_str(&json).map_err(|e| Error::Invalid(format!("day {day}: {e}"))),
        None => Ok(DayBucket::default()),
    }
}

fn save_day(day: &str, bucket: &DayBucket) -> Result<(), Error> {
    let json = serde_json::to_string(bucket).expect("a day bucket serializes");
    storage::set(&format!("{DAY}{day}"), &json)
}

/// The buckets of the `days` UTC days ending today, summed.
fn aggregate(now: u64, days: u64) -> Result<DayBucket, Error> {
    let mut out = DayBucket::default();
    for i in 0..days {
        out.merge(&load_day(&day_key(now.saturating_sub(i * 86_400)))?);
    }
    Ok(out)
}

/// Deletes the day buckets older than [`RETENTION_DAYS`]; once per day.
fn prune(now: u64) -> Result<usize, Error> {
    let today = day_key(now);
    if storage::get(PRUNED)?.as_deref() == Some(today.as_str()) {
        return Ok(0);
    }
    let cutoff = day_key(now.saturating_sub(RETENTION_DAYS * 86_400));
    let mut dropped = 0;
    for key in storage::keys(DAY)? {
        if key[DAY.len()..].as_bytes() < cutoff.as_bytes() && storage::delete(&key).is_ok() {
            dropped += 1;
        }
    }
    storage::set(PRUNED, &today)?;
    Ok(dropped)
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// The idle threshold in seconds: `idleMinutes` (1–240, default 5).
fn idle_secs() -> u64 {
    let minutes = host::setting_number("idleMinutes").unwrap_or(5.0).clamp(1.0, 240.0);
    (minutes * 60.0) as u64
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// Which span the dashboard shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Period {
    #[default]
    Today,
    Days7,
}

/// The accounting session as persisted under [`SESSION`]: the file the
/// current stretch is credited to, when its last accounting point was, and
/// whether a scheduled wakeup already settled it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Session {
    path: String,
    last: u64,
    ticked: bool,
}

pub struct CodeTime {
    /// Seconds since the epoch; the tests swap a fixed clock in.
    clock: Box<dyn Fn() -> u64>,
    /// The file the current stretch is credited to, from the last heartbeat.
    focused: Option<String>,
    /// When the stretch's last accounting point was (a heartbeat or a
    /// scheduled wakeup).
    last_secs: u64,
    /// A scheduled wakeup already settled this stretch (no activity since).
    ticked: bool,
    period: Period,
}

impl Default for CodeTime {
    fn default() -> Self {
        Self { clock: Box::new(now_secs), focused: None, last_secs: 0, ticked: false, period: Period::default() }
    }
}

impl CodeTime {
    /// A plugin whose clock reads `now` (the tests).
    pub fn with_clock(now: Box<dyn Fn() -> u64>) -> Self {
        Self { clock: now, ..CodeTime::default() }
    }

    /// Restores the persisted session; runs on activation, i.e. when the
    /// host creates a fresh instance (also after reclaiming the idle one).
    fn restore(&mut self) {
        if let Ok(Some(json)) = storage::get(SESSION)
            && let Ok(s) = serde_json::from_str::<Session>(&json)
        {
            self.focused = Some(s.path);
            self.last_secs = s.last;
            self.ticked = s.ticked;
        }
    }

    /// Persists the session, or deletes the key when there is none. Storage
    /// trouble is swallowed: nothing is counted with or without it.
    fn persist(&self) {
        match &self.focused {
            Some(path) => {
                let json = serde_json::to_string(&Session {
                    path: path.clone(),
                    last: self.last_secs,
                    ticked: self.ticked,
                })
                .expect("a session serializes");
                let _ = storage::set(SESSION, &json);
            }
            None => {
                let _ = storage::delete(SESSION);
            }
        }
    }

    /// A heartbeat for `path` at `now`: the stretch since the previous
    /// accounting point settles to the file it was spent on (the previously
    /// focused one — a switch does not discard it) when it is within the
    /// idle threshold; anything else just starts a new session (a long
    /// stretch counts from the heartbeat that ends it).
    fn heartbeat(&mut self, path: &str, now: u64, globs: &[String], ctx: &Invocation) {
        if now < self.last_secs {
            return; // the clock went backwards: ignore the heartbeat
        }
        self.settle_stretch(now, globs, ctx);
        self.focused = Some(path.to_owned());
        self.last_secs = now;
        self.ticked = false;
        self.persist();
    }

    /// The editor lost its file focus: the stretch settles to the file it
    /// was spent on and the session ends.
    fn blur(&mut self, now: u64, globs: &[String], ctx: &Invocation) {
        if now < self.last_secs {
            return;
        }
        self.settle_stretch(now, globs, ctx);
        self.focused = None;
        self.last_secs = now;
        self.ticked = false;
        self.persist();
    }

    /// Credits the stretch since the last accounting point to the previously
    /// focused file when there is one and the stretch is within the idle
    /// threshold (an excluded path credits nothing).
    fn settle_stretch(&mut self, now: u64, globs: &[String], ctx: &Invocation) {
        let elapsed = now - self.last_secs;
        if elapsed <= idle_secs()
            && let Some(prev) = self.focused.clone()
        {
            self.credit(&prev, elapsed, now, globs, ctx);
        }
    }

    /// The `on-schedule` wakeup. The host delivers `Scheduled` only to an
    /// instance it recreated after reclaiming the idle one — never to a live
    /// instance — so this is not a minute counter: it settles the persisted
    /// stretch once (the `ticked` flag keeps it to once) while it is still
    /// within the idle threshold, and expires the session otherwise.
    fn tick(&mut self, now: u64, globs: &[String], ctx: &Invocation) {
        if now < self.last_secs {
            return;
        }
        let elapsed = now - self.last_secs;
        let settles = elapsed > 0 && elapsed <= idle_secs() && !self.ticked && self.focused.is_some();
        if settles {
            let path = self.focused.clone().expect("focused, checked above");
            self.credit(&path, elapsed, now, globs, ctx);
            self.last_secs = now;
            self.ticked = true;
        } else {
            self.focused = None;
            self.last_secs = now;
            self.ticked = false;
        }
        self.persist();
    }

    /// Credits `secs` of `path` into the day `now` ends in and saves the
    /// bucket. An excluded path is never credited.
    fn credit(&self, path: &str, secs: u64, now: u64, globs: &[String], ctx: &Invocation) {
        if secs == 0 || is_excluded(path, globs) {
            return;
        }
        let project = project_of(path, ctx);
        let lang = language_of(path);
        let day = day_key(now);
        let mut bucket = load_day(&day).unwrap_or_default();
        bucket.add(&project, &lang, path, secs);
        if save_day(&day, &bucket).is_err() {
            return; // storage denied or full: nothing to count with
        }
    }
}

impl Plugin for CodeTime {
    fn activate(&mut self, _ctx: &Context) -> Result<(), Error> {
        self.restore();
        Ok(())
    }

    fn run_command(&mut self, id: &str, _ctx: &Invocation, _arg: Option<&str>) -> Result<Option<String>, Error> {
        let now = (self.clock)();
        match id {
            "show-dashboard" => {
                host::reveal_view("dashboard")?;
                Ok(None)
            }
            "export-summary" => {
                let text = summary_text(now, aggregate(now, 1)?, aggregate(now, 7)?);
                match host::copy_to_clipboard(&text) {
                    Ok(()) => Ok(Some("CodeTime summary copied to the clipboard.".into())),
                    Err(Error::Denied(c)) => Err(format!("CodeTime needs the `{c}` capability to export the summary.").into()),
                    Err(e) => Err(e),
                }
            }
            "reset" => {
                for key in storage::keys(DAY)? {
                    storage::delete(&key)?;
                }
                let _ = storage::delete(PRUNED);
                // Reset ends the live accounting session together with the
                // stored one: a heartbeat or a reclaim afterwards starts from
                // nothing instead of re-settling the stretch from before the
                // reset. `persist` deletes the stored session when there is
                // no live one, so the two cannot drift apart.
                self.focused = None;
                self.last_secs = 0;
                self.ticked = false;
                self.persist();
                host::remove_status_item(ITEM);
                Ok(Some("CodeTime statistics were reset.".into()))
            }
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }

    fn handle_events(&mut self, events: &[Event], ctx: &Invocation) {
        let now = (self.clock)();
        let globs = excluded_globs();
        for e in events {
            match e {
                Event::FileOpened(path) | Event::FileSaved(path) => self.heartbeat(path, now, &globs, ctx),
                Event::EditorFocused(Some(path)) => self.heartbeat(path, now, &globs, ctx),
                Event::EditorFocused(None) => self.blur(now, &globs, ctx),
                Event::Scheduled => self.tick(now, &globs, ctx),
                _ => {}
            }
        }
        if let Ok(dropped) = prune(now)
            && dropped > 0
        {
            host::log(
                LogLevel::Info,
                &format!("CodeTime: pruned {dropped} day buckets older than {RETENTION_DAYS} days"),
            );
        }
        show_status(now);
    }
}

/// The status bar item: today's `H:MM`, a tooltip with the project split and
/// a click that opens the dashboard.
fn show_status(now: u64) {
    let bucket = load_day(&day_key(now)).unwrap_or_default();
    let mut tooltip = vec![format!("CodeTime — today: {}", format_hm(bucket.total))];
    let projects = top(&bucket.projects, 3);
    if !projects.is_empty() {
        tooltip.push(projects.iter().map(|(p, s)| format!("{p} {}", format_hm(*s))).collect::<Vec<_>>().join(", "));
    }
    tooltip.push(format!("Idle over {} min is not counted. Click for the dashboard.", idle_secs() / 60));
    host::set_status_item(
        ITEM,
        &StatusItem {
            text: format_hm(bucket.total),
            tooltip: Some(tooltip.join("\n")),
            command: Some("show-dashboard".into()),
        },
    );
}

/// The multi-line text "Export Summary to Clipboard" copies: today and the
/// last 7 days, each by project and by language.
pub fn summary_text(now: u64, today: DayBucket, week: DayBucket) -> String {
    let mut lines = vec![format!("CodeTime — today ({}): {}", day_key(now), format_hm(today.total))];
    lines.push("By project: ".to_owned() + &joined(&top(&today.projects, 10)));
    lines.push("By language: ".to_owned() + &joined(&top(&today.langs, 10)));
    lines.push(String::new());
    lines.push(format!("Last 7 days: {}", format_hm(week.total)));
    lines.push("By project: ".to_owned() + &joined(&top(&week.projects, 10)));
    lines.push("By language: ".to_owned() + &joined(&top(&week.langs, 10)));
    lines.join("\n")
}

fn joined(rows: &[(String, u64)]) -> String {
    if rows.is_empty() {
        "nothing recorded".into()
    } else {
        rows.iter().map(|(n, s)| format!("{n} {}", format_hm(*s))).collect::<Vec<_>>().join(", ")
    }
}

impl Views for CodeTime {
    fn render(&mut self, view: &str, viewport: Option<Viewport>, _ctx: &Invocation) -> Result<View, Error> {
        if view != "dashboard" {
            return Err(Error::NotFound(view.into()));
        }
        let now = (self.clock)();
        let (days, label) = match self.period {
            Period::Today => (1, "today"),
            Period::Days7 => (7, "last 7 days"),
        };
        let bucket = aggregate(now, days)?;
        let mut rows: Vec<Vec<String>> = Vec::new();
        for (name, secs) in top(&bucket.projects, 50) {
            rows.push(vec!["Project".into(), name, format_hm(secs)]);
        }
        for (name, secs) in top(&bucket.langs, 50) {
            rows.push(vec!["Language".into(), name, format_hm(secs)]);
        }
        for (name, secs) in top(&bucket.files, TOP_FILES) {
            rows.push(vec!["File".into(), shorten(&name), format_hm(secs)]);
        }
        let total = rows.len() as u32;
        let range = Viewport::range(viewport, total, DEFAULT_ROWS);
        let rows = rows[range.start as usize..range.end as usize].to_vec();
        Ok(View::new(Body::Table(Table {
            columns: vec!["Kind".into(), "Name".into(), "Time".into()],
            total_rows: total,
            first_row: range.start,
            rows,
            editable: false,
        }))
        // Both periods stay selectable: `enabled` renders as the button's
        // disabled state, so only the already-selected period being enabled
        // would lock the toolbar on the default one.
        .button(Button::new("today", "Today"))
        .button(Button::new("days7", "Last 7 Days"))
        .button(Button::new("refresh", "Refresh").icon("refresh"))
        .status(&format!("Total {label}: {}", format_hm(bucket.total))))
    }

    fn handle(&mut self, view: &str, event: ViewEvent, _ctx: &Invocation) -> Result<(), Error> {
        if view != "dashboard" {
            return Err(Error::NotFound(view.into()));
        }
        match event {
            ViewEvent::Button(b) if b == "today" => self.period = Period::Today,
            ViewEvent::Button(b) if b == "days7" => self.period = Period::Days7,
            _ => return Ok(()),
        }
        host::refresh_view("dashboard");
        Ok(())
    }
}

register_plugin!(CodeTime, views);

#[cfg(test)]
mod tests {
    use super::*;
    use openidea_plugin::mock::MockHost;
    use std::cell::Cell;
    use std::rc::Rc;

    /// 2025-11-16 00:00:00 UTC (a Sunday).
    const DAY0: u64 = 1_763_251_200;

    /// A plugin whose clock reads `t` (mutable through the returned cell).
    fn plugin(t0: u64) -> (CodeTime, Rc<Cell<u64>>) {
        let t = Rc::new(Cell::new(t0));
        let now = { let t = t.clone(); Box::new(move || t.get()) as Box<dyn Fn() -> u64> };
        (CodeTime::with_clock(now), t)
    }

    fn bucket_of(day: &str) -> DayBucket {
        load_day(day).unwrap_or_default()
    }

    /// The persisted session's JSON, as [`CodeTime::persist`] writes it.
    fn session_json(path: &str, last: u64, ticked: bool) -> String {
        format!(r#"{{"path":"{path}","last":{last},"ticked":{ticked}}}"#)
    }

    /// A fresh instance the way the host creates one after reclaiming the
    /// idle plugin: a new `CodeTime` that runs `activate` (restoring the
    /// persisted session) on the same storage and clock.
    fn reclaimed(host: &MockHost, t: &Rc<Cell<u64>>) -> CodeTime {
        let now = { let t = t.clone(); Box::new(move || t.get()) as Box<dyn Fn() -> u64> };
        let mut fresh = CodeTime::with_clock(now);
        fresh.activate(&host.context("inf1mum.codetime")).expect("activate");
        fresh
    }

    #[test]
    fn utc_day_keys_and_hm_durations() {
        assert_eq!(day_key(0), "19700101");
        assert_eq!(day_key(86_400), "19700102");
        assert_eq!(day_key(DAY0), "20251116");
        assert_eq!(day_key(DAY0 + 86_399), "20251116");
        assert_eq!(day_key(DAY0 + 86_400), "20251117");
        assert_eq!(format_hm(0), "0:00");
        assert_eq!(format_hm(2_817), "0:46");
        assert_eq!(format_hm(7_260), "2:01");
        assert_eq!(format_hm(45_240), "12:34");
    }

    #[test]
    fn exclude_globs_match_paths_and_file_names() {
        assert!(glob_match("**/node_modules/**", "C:/proj/node_modules/pkg/x.js"));
        assert!(glob_match("**/node_modules/**", "/proj/node_modules/x.js"));
        assert!(!glob_match("**/node_modules/**", "/proj/src/x.js"));
        assert!(glob_match("*.min.js", "app.min.js"), "a pattern without / matches the file name");
        assert!(glob_match("*.min.js", "src/app.min.js"));
        assert!(!glob_match("*.min.js", "src/app.js"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/deep/main.rs"));
        assert!(!glob_match("target/**", "C:/dev/target/x/y.rs"), "a pattern with / is anchored to the path root");
        assert!(glob_match("**/target/**", "C:/dev/target/x/y.rs"));
        assert!(!glob_match("**/target/**", "C:/dev/dist/x.rs"));
        assert!(glob_match("se?ret", "secret"));
        assert!(!glob_match("se?ret", "se/ret"));
        assert!(glob_match("*", "any.rs"));
        assert!(glob_match("*", "a/b.rs"), "a pattern without / matches the file name alone");
        assert!(!glob_match("/*.rs", "a/b.rs"), "a pattern with / is anchored to the path root");
        assert!(glob_match("**", "a/b/c.rs"));
        assert!(glob_match("**/src/*.rs", "C:\\proj\\src\\main.rs"), "backslashes are separators");
    }

    #[test]
    fn languages_come_from_the_extension() {
        assert_eq!(language_of("/p/src/main.rs"), "Rust");
        assert_eq!(language_of("/p/APP.TS"), "TypeScript");
        assert_eq!(language_of("/p/notes.md"), "Markdown");
        assert_eq!(language_of("/p/Makefile"), "Other");
        assert_eq!(language_of("/p/data.weird"), "WEIRD");
        assert_eq!(language_of("C:\\p\\a\\b.py"), "Python");
    }

    #[test]
    fn file_names_shorten_to_three_segments() {
        assert_eq!(shorten("a/b/c.rs"), "a/b/c.rs");
        assert_eq!(shorten("/deep/a/b/c.rs"), "…/a/b/c.rs");
        assert_eq!(shorten("C:\\w\\a\\b\\c.rs"), "…/a/b/c.rs");
    }

    #[test]
    fn trim_keeps_the_largest_and_top_orders_by_time() {
        let mut m: BTreeMap<String, u64> = ["a", "b", "c"].into_iter().map(|k| (k.to_owned(), 0)).collect();
        m.insert("big".into(), 100);
        trim(&mut m, 2);
        assert_eq!(m, BTreeMap::from([("a".into(), 0), ("big".into(), 100)]));
        let mut t = BTreeMap::new();
        t.insert("x".into(), 5);
        t.insert("y".into(), 9);
        t.insert("z".into(), 5);
        assert_eq!(top(&t, 10), [("y".to_owned(), 9), ("x".to_owned(), 5), ("z".to_owned(), 5)]);
        assert_eq!(top(&t, 1), [("y".to_owned(), 9)]);
    }

    #[test]
    fn buckets_add_and_merge() {
        let mut b = DayBucket::default();
        b.add("proj", "Rust", "/p/src/main.rs", 60);
        b.add("proj", "Rust", "/p/src/main.rs", 30);
        b.add("other", "Go", "/q/x.go", 10);
        assert_eq!(b.total, 100);
        assert_eq!(b.projects, BTreeMap::from([("proj".into(), 90), ("other".into(), 10)]));
        assert_eq!(b.langs, BTreeMap::from([("Rust".into(), 90), ("Go".into(), 10)]));
        let mut week = DayBucket::default();
        week.merge(&b);
        week.merge(&b);
        assert_eq!(week.total, 200);
        assert_eq!(week.projects["proj"], 180);
        assert_eq!(week.files["/p/src/main.rs"], 180);
    }

    #[test]
    fn heartbeats_credit_elapsed_time_and_idle_stops_it() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        let f = "/project/src/main.rs".to_owned();
        // The first heartbeat starts the session without credit.
        p.handle_events(&[Event::FileOpened(f.clone())], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 0);
        // Thirty seconds of work, credited by the next heartbeat.
        t.set(DAY0 + 30);
        p.handle_events(&[Event::FileSaved(f.clone())], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 30);
        // A wakeup settles the quiet stretch since the save once.
        t.set(DAY0 + 90);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 90);
        // Another wakeup without activity in between does not count on; past
        // the idle threshold (default 5 min) the quiet stretch is over.
        t.set(DAY0 + 90 + 600);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 90);
        t.set(DAY0 + 90 + 610);
        p.handle_events(&[Event::EditorFocused(Some(f.clone()))], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 90, "returning from idle starts over");
        // The status bar shows today's total and opens the dashboard.
        let item = &host.status_items()["codetime"];
        assert_eq!(item.text, "0:01");
        assert_eq!(item.command.as_deref(), Some("show-dashboard"));
        assert!(item.tooltip.as_deref().unwrap().contains("Idle over 5 min"));
    }

    #[test]
    fn switching_files_credits_the_stretch_before_the_switch() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/a.rs".into()))], &ctx);
        // Thirty seconds of work in a.rs, then a switch: the stretch settles
        // to a.rs, the file it was spent on, and b.rs counts from its focus.
        t.set(DAY0 + 30);
        p.handle_events(&[Event::EditorFocused(Some("/project/src/b.rs".into()))], &ctx);
        let b = bucket_of(&day_key(DAY0));
        assert_eq!(b.total, 30, "the stretch before the switch counts");
        assert_eq!(b.files.get("/project/src/a.rs"), Some(&30));
        assert!(b.files.get("/project/src/b.rs").is_none());
        assert_eq!(host.storage()["session"], session_json("/project/src/b.rs", DAY0 + 30, false));
        // A wakeup later settles b.rs's own stretch.
        t.set(DAY0 + 90);
        p.handle_events(&[Event::Scheduled], &ctx);
        let b = bucket_of(&day_key(DAY0));
        assert_eq!(b.total, 90);
        assert_eq!(b.files.get("/project/src/b.rs"), Some(&60));
        assert_eq!(b.projects, BTreeMap::from([("project".into(), 90)]));
        assert_eq!(b.langs, BTreeMap::from([("Rust".into(), 90)]));
    }

    #[test]
    fn losing_editor_focus_settles_and_ends_the_session() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/a.rs".into()))], &ctx);
        t.set(DAY0 + 120);
        p.handle_events(&[Event::EditorFocused(None)], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 120, "the stretch settles to the file it was spent on");
        assert!(!host.storage().contains_key(SESSION), "the session ended with the focus");
        // Without a focused file, a wakeup settles nothing.
        t.set(DAY0 + 240);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 120);
        // The editor-less stretch does not land on the next focused file.
        t.set(DAY0 + 300);
        p.handle_events(&[Event::EditorFocused(Some("/project/src/b.rs".into()))], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 120);
    }

    #[test]
    fn a_session_survives_reclaim_and_the_wakeup_settles_it_once() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::FileOpened("/project/src/a.rs".into())], &ctx);
        assert_eq!(
            host.storage()[SESSION],
            session_json("/project/src/a.rs", DAY0, false),
            "the session is persisted for the next instance"
        );
        // The host reclaims the idle instance; the due schedule reaches a
        // fresh one, which restores the session and settles the stretch.
        t.set(DAY0 + 200);
        let mut p = reclaimed(&host, &t);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 200, "the wakeup settles the persisted stretch");
        assert_eq!(bucket_of(&day_key(DAY0)).files.get("/project/src/a.rs"), Some(&200));
        assert_eq!(host.storage()[SESSION], session_json("/project/src/a.rs", DAY0 + 200, true));
        // A second wakeup without activity in between does not count on.
        t.set(DAY0 + 400);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 200, "a quiet stretch is settled once");
        assert!(!host.storage().contains_key(SESSION), "the expired session is gone");
        // A heartbeat afterwards starts a fresh session without credit.
        t.set(DAY0 + 420);
        p.handle_events(&[Event::FileSaved("/project/src/a.rs".into())], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 200);
        assert_eq!(host.storage()[SESSION], session_json("/project/src/a.rs", DAY0 + 420, false));
    }

    #[test]
    fn a_stretch_past_the_idle_threshold_is_expired_by_the_wakeup() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/a.rs".into()))], &ctx);
        // The wakeup arrives only after the host's idle reclaim, so the
        // quiet stretch is already past the idle threshold: it counts
        // nothing and the session ends.
        t.set(DAY0 + 400);
        let mut fresh = reclaimed(&host, &t);
        fresh.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 0);
        assert!(!host.storage().contains_key(SESSION));
        t.set(DAY0 + 430);
        fresh.handle_events(&[Event::EditorFocused(Some("/project/src/b.rs".into()))], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 0, "the fresh session starts without credit");
        assert_eq!(host.storage()[SESSION], session_json("/project/src/b.rs", DAY0 + 430, false));
    }

    #[test]
    fn a_focus_switch_after_reclaim_settles_the_saved_stretch() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::FileOpened("/project/src/a.rs".into())], &ctx);
        // The switch arrives at a fresh instance; the persisted stretch
        // settles to the file it was spent on.
        t.set(DAY0 + 150);
        let mut fresh = reclaimed(&host, &t);
        fresh.handle_events(&[Event::EditorFocused(Some("/project/src/b.rs".into()))], &ctx);
        let b = bucket_of(&day_key(DAY0));
        assert_eq!(b.total, 150, "the stretch saved before the reclaim settles at the switch");
        assert_eq!(b.files, BTreeMap::from([("/project/src/a.rs".into(), 150)]));
        // b.rs counts from its own focus, settled by the next wakeup.
        t.set(DAY0 + 300);
        fresh.handle_events(&[Event::Scheduled], &ctx);
        let b = bucket_of(&day_key(DAY0));
        assert_eq!(b.total, 300);
        assert_eq!(b.files.get("/project/src/b.rs"), Some(&150));
    }

    #[test]
    fn files_in_other_projects_group_under_their_root() {
        let host = MockHost::install();
        host.add_project("/repos/other");
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/repos/other/src/main.rs".into()))], &ctx);
        t.set(DAY0 + 120);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).projects, BTreeMap::from([("other".into(), 120)]));
    }

    #[test]
    fn excluded_paths_are_not_counted() {
        let host = MockHost::install();
        host.set_setting("exclude", r#"["**/node_modules/**", "*.min.js"]"#);
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        let junk = "/project/node_modules/pkg/index.js".to_owned();
        p.handle_events(&[Event::FileOpened(junk.clone())], &ctx);
        t.set(DAY0 + 120);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 0);
        // A tracked file after the excluded one counts only from its own focus.
        t.set(DAY0 + 200);
        p.handle_events(&[Event::EditorFocused(Some("/project/src/app.js".into()))], &ctx);
        t.set(DAY0 + 260);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 60);
    }

    #[test]
    fn the_idle_setting_is_clamped() {
        let host = MockHost::install();
        host.set_setting("idleMinutes", "0");
        assert_eq!(idle_secs(), 60);
        host.set_setting("idleMinutes", "1000");
        assert_eq!(idle_secs(), 240 * 60);
        host.set_setting("idleMinutes", "1");
        assert_eq!(idle_secs(), 60);
        host.set_setting("idleMinutes", "\"x\"");
        assert_eq!(idle_secs(), 300, "a non-number falls back to the default");
    }

    #[test]
    fn midnight_credits_the_day_the_stretch_ends_in() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0 + 86_220); // 23:57
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/a.rs".into()))], &ctx);
        t.set(DAY0 + 86_480); // 00:01:20 the next day, within the idle threshold
        p.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 0);
        assert_eq!(bucket_of(&day_key(DAY0 + 86_400)).total, 260);
    }

    #[test]
    fn pruning_drops_buckets_older_than_retention() {
        let host = MockHost::install();
        let now = DAY0 + 400 * 86_400;
        let (mut p, t) = plugin(now);
        let ctx = host.invocation();
        storage::set(&format!("{DAY}{}", day_key(now - 100 * 86_400)), "{}").unwrap();
        storage::set(&format!("{DAY}{}", day_key(now - 10 * 86_400)), "{}").unwrap();
        p.handle_events(&[Event::Scheduled], &ctx);
        assert!(!host.storage().contains_key(&format!("{DAY}{}", day_key(now - 100 * 86_400))));
        assert!(host.storage().contains_key(&format!("{DAY}{}", day_key(now - 10 * 86_400))));
        assert_eq!(host.storage()["pruned-day"], day_key(now));
        // Once per day: the same day prunes nothing again.
        t.set(now + 60);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert!(host.storage().contains_key(&format!("{DAY}{}", day_key(now - 10 * 86_400))));
    }

    /// Thirty minutes of work as the editor reports it: a save of `path`
    /// every minute (autosave), each heartbeat settling the minute since
    /// the previous one.
    fn thirty_minutes_of_saves(p: &mut CodeTime, host: &MockHost, t: &Rc<Cell<u64>>, path: &str, from: u64) {
        for m in 1..=30u64 {
            t.set(from + m * 60);
            p.handle_events(&[Event::FileSaved(path.into())], &host.invocation());
        }
    }

    #[test]
    fn the_dashboard_lists_projects_languages_and_files() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/main.rs".into()))], &ctx);
        thirty_minutes_of_saves(&mut p, &host, &t, "/project/src/main.rs", DAY0);
        p.handle_events(&[Event::EditorFocused(Some("/project/README.md".into()))], &host.invocation());
        thirty_minutes_of_saves(&mut p, &host, &t, "/project/README.md", DAY0 + 1_800);
        p.handle_events(&[Event::EditorFocused(Some("/project/src/lib.rs".into()))], &host.invocation());
        t.set(DAY0 + 3_660);
        p.handle_events(&[Event::FileSaved("/project/src/lib.rs".into())], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 3_660);

        let view = p.render("dashboard", None, &ctx).unwrap();
        let Body::Table(tb) = view.body else { panic!("a table") };
        assert_eq!(tb.columns, ["Kind", "Name", "Time"]);
        assert_eq!(tb.rows[0], ["Project".to_owned(), "project".into(), "1:01".into()]);
        assert_eq!(tb.rows[1], ["Language".to_owned(), "Rust".into(), "0:31".into()]);
        assert_eq!(tb.rows[2], ["Language".to_owned(), "Markdown".into(), "0:30".into()]);
        assert!(tb.rows.iter().any(|r| r[0] == "File" && r[1] == "…/project/src/main.rs" && r[2] == "0:30"));
        assert_eq!(view.status.as_deref(), Some("Total today: 1:01"));
        // The toolbar switches to the 7-day span (one day recorded: same total).
        p.handle("dashboard", ViewEvent::Button("days7".into()), &ctx).unwrap();
        assert_eq!(host.refreshed_views(), ["dashboard"]);
        let view = p.render("dashboard", None, &ctx).unwrap();
        assert_eq!(view.status.as_deref(), Some("Total last 7 days: 1:01"));
        // …and back to today.
        p.handle("dashboard", ViewEvent::Button("today".into()), &ctx).unwrap();
        let view = p.render("dashboard", None, &ctx).unwrap();
        assert_eq!(view.status.as_deref(), Some("Total today: 1:01"));
        assert_eq!(host.refreshed_views(), ["dashboard", "dashboard"]);
    }

    #[test]
    fn the_toolbar_keeps_both_periods_selectable() {
        let host = MockHost::install();
        let (mut p, _t) = plugin(DAY0);
        let ctx = host.invocation();
        let mut bucket = DayBucket::default();
        bucket.add("project", "Rust", "/project/src/main.rs", 60);
        storage::set(&format!("{DAY}{}", day_key(DAY0)), &serde_json::to_string(&bucket).unwrap()).unwrap();
        // The default (today) toolbar: the host renders `enabled: false` as a
        // disabled button, so the other period must not carry that flag.
        let enabled = |view: &View| -> Vec<(String, bool)> {
            view.toolbar
                .iter()
                .filter(|b| b.id == "today" || b.id == "days7")
                .map(|b| (b.id.clone(), b.enabled))
                .collect()
        };
        let view = p.render("dashboard", None, &ctx).unwrap();
        assert_eq!(enabled(&view), [("today".into(), true), ("days7".into(), true)]);
        assert_eq!(view.status.as_deref(), Some("Total today: 0:01"));
        // Both transitions work and keep both buttons selectable.
        p.handle("dashboard", ViewEvent::Button("days7".into()), &ctx).unwrap();
        let view = p.render("dashboard", None, &ctx).unwrap();
        assert_eq!(view.status.as_deref(), Some("Total last 7 days: 0:01"));
        assert_eq!(enabled(&view), [("today".into(), true), ("days7".into(), true)], "today stays selectable");
        p.handle("dashboard", ViewEvent::Button("today".into()), &ctx).unwrap();
        let view = p.render("dashboard", None, &ctx).unwrap();
        assert_eq!(view.status.as_deref(), Some("Total today: 0:01"));
    }

    #[test]
    fn the_dashboard_pages_through_the_viewport() {
        let host = MockHost::install();
        let (mut p, _t) = plugin(DAY0);
        let mut bucket = DayBucket::default();
        bucket.add("a", "Rust", "/p/1.rs", 60);
        bucket.add("b", "Go", "/p/2.go", 50);
        bucket.add("a", "Rust", "/p/3.rs", 40);
        storage::set(&format!("{DAY}{}", day_key(DAY0)), &serde_json::to_string(&bucket).unwrap()).unwrap();
        let ctx = host.invocation();
        let view = p.render("dashboard", Some(Viewport { first_row: 5, rows: 2 }), &ctx).unwrap();
        let Body::Table(tb) = view.body else { panic!("a table") };
        assert_eq!((tb.total_rows, tb.first_row), (7, 5));
        assert_eq!(tb.rows.len(), 2, "rows 5 and 6 of 7");
        let view = p.render("dashboard", Some(Viewport { first_row: 10, rows: 2 }), &ctx).unwrap();
        let Body::Table(tb) = view.body else { panic!("a table") };
        assert_eq!(tb.rows.len(), 0, "past the end");
    }

    #[test]
    fn unknown_views_and_commands_are_not_found_but_the_dashboard_opens() {
        let host = MockHost::install();
        let (mut p, _t) = plugin(DAY0);
        let ctx = host.invocation();
        assert_eq!(p.render("nope", None, &ctx).unwrap_err(), Error::NotFound("nope".into()));
        assert!(p.handle("nope", ViewEvent::Button("today".into()), &ctx).is_err());
        assert!(p.run_command("nope", &ctx, None).is_err());
        assert_eq!(p.run_command("show-dashboard", &ctx, None).unwrap(), None);
        assert_eq!(host.revealed_views(), ["dashboard"]);
    }

    #[test]
    fn export_copies_a_summary_and_reset_clears_everything() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/a.rs".into()))], &ctx);
        for m in 1..=60u64 {
            t.set(DAY0 + m * 60);
            p.handle_events(&[Event::FileSaved("/project/src/a.rs".into())], &ctx);
        }
        assert!(host.storage().contains_key(SESSION), "the live session is persisted");
        let out = p.run_command("export-summary", &ctx, None).unwrap().unwrap();
        let clipboard = host.clipboard().expect("the summary was copied");
        assert!(clipboard.contains("CodeTime — today (20251116): 1:00"));
        assert!(clipboard.contains("By project: project 1:00"));
        assert!(clipboard.contains("By language: Rust 1:00"));
        assert!(clipboard.contains("Last 7 days: 1:00"));
        assert!(out.contains("copied"));
        p.run_command("reset", &ctx, None).unwrap();
        assert!(host.storage().is_empty(), "the day buckets, the prune marker and the session are gone");
        assert!(host.status_items().is_empty(), "the status bar item is removed");
        assert_eq!(
            p.run_command("export-summary", &ctx, None).unwrap().unwrap(),
            "CodeTime summary copied to the clipboard."
        );
        assert!(host.clipboard().unwrap().contains("nothing recorded"));
    }

    /// The maintainer's repro: a heartbeat after the reset must not re-settle
    /// the stretch from before it — reset ends the live session, so only the
    /// work after the reset is eligible.
    #[test]
    fn reset_ends_the_live_session_and_the_next_heartbeat_starts_fresh() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        let f = "/project/src/a.rs".to_owned();
        p.handle_events(&[Event::EditorFocused(Some(f.clone()))], &ctx);
        t.set(DAY0 + 60);
        p.handle_events(&[Event::FileSaved(f.clone())], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 60);
        assert_eq!(host.storage()[SESSION], session_json(&f, DAY0 + 60, false));
        // Reset at +90 clears the buckets and the live session alike.
        t.set(DAY0 + 90);
        p.run_command("reset", &ctx, None).unwrap();
        assert!(host.storage().is_empty(), "the stored session is gone with the totals");
        // The save at +120 keeps the focus: with the session ended it starts
        // a fresh one and re-credits nothing — not the 30 seconds from
        // before the reset.
        t.set(DAY0 + 120);
        p.handle_events(&[Event::FileSaved(f.clone())], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 0, "no stale stretch re-settles after the reset");
        assert_eq!(host.storage()[SESSION], session_json(&f, DAY0 + 120, false));
        // Only the work after the reset counts, credited by the next heartbeat.
        t.set(DAY0 + 180);
        p.handle_events(&[Event::FileSaved(f.clone())], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 60, "the stretch after the reset counts from its own heartbeat");
        assert_eq!(bucket_of(&day_key(DAY0)).files.get(f.as_str()), Some(&60));
    }

    /// Reset followed by a reclaim: the fresh instance restores no session,
    /// so the scheduled wakeup expires nothing and no stale time re-settles.
    #[test]
    fn reset_followed_by_a_reclaim_starts_from_nothing() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/a.rs".into()))], &ctx);
        t.set(DAY0 + 60);
        p.handle_events(&[Event::FileSaved("/project/src/a.rs".into())], &ctx);
        t.set(DAY0 + 90);
        p.run_command("reset", &ctx, None).unwrap();
        assert!(!host.storage().contains_key(SESSION), "reset ends the persisted session");
        // The host reclaims the idle instance; the fresh one restores
        // nothing, and the due wakeup credits nothing from before the reset.
        t.set(DAY0 + 200);
        let mut fresh = reclaimed(&host, &t);
        fresh.handle_events(&[Event::Scheduled], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 0, "the reclaim after the reset starts from nothing");
        assert!(!host.storage().contains_key(SESSION), "the wakeup expires the empty session");
        // A heartbeat afterwards starts a fresh session without credit.
        t.set(DAY0 + 230);
        fresh.handle_events(&[Event::EditorFocused(Some("/project/src/a.rs".into()))], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0)).total, 0);
        assert_eq!(host.storage()[SESSION], session_json("/project/src/a.rs", DAY0 + 230, false));
    }

    #[test]
    fn export_without_the_clipboard_capability_explains_it() {
        let host = MockHost::install().with_capabilities(&["events", "project.read", "storage"]);
        let (mut p, _t) = plugin(DAY0);
        let err = p.run_command("export-summary", &host.invocation(), None).unwrap_err();
        assert!(err.to_string().contains("clipboard"), "{err}");
    }

    #[test]
    fn without_storage_the_totals_stay_empty() {
        let host = MockHost::install().with_capabilities(&["events", "project.read"]);
        let (mut p, t) = plugin(DAY0);
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/a.rs".into()))], &ctx);
        t.set(DAY0 + 60);
        p.handle_events(&[Event::Scheduled], &ctx);
        assert!(host.storage().is_empty());
        assert_eq!(host.status_items()["codetime"].text, "0:00");
    }

    #[test]
    fn the_summary_lists_today_and_the_week() {
        let mut today = DayBucket::default();
        today.add("proj", "Rust", "/p/main.rs", 3_600);
        let mut week = DayBucket::default();
        week.add("proj", "Rust", "/p/main.rs", 18_000);
        let text = summary_text(DAY0, today, week);
        assert!(text.contains("CodeTime — today (20251116): 1:00"), "{text}");
        assert!(text.contains("By project: proj 1:00"));
        assert!(text.contains("By language: Rust 1:00"));
        assert!(text.contains("Last 7 days: 5:00"));
        let empty = summary_text(DAY0, DayBucket::default(), DayBucket::default());
        assert!(empty.contains("nothing recorded"), "{empty}");
    }

    /// A heartbeat from before the last one (a clock that went backwards) is
    /// ignored instead of charging negative time.
    #[test]
    fn a_backwards_clock_is_ignored() {
        let host = MockHost::install();
        let (mut p, t) = plugin(DAY0 + 1_000);
        let ctx = host.invocation();
        p.handle_events(&[Event::EditorFocused(Some("/project/src/a.rs".into()))], &ctx);
        t.set(DAY0);
        p.handle_events(&[Event::FileSaved("/project/src/a.rs".into())], &ctx);
        assert_eq!(bucket_of(&day_key(DAY0 + 86_400)).total, 0);
        assert!(host.storage().keys().all(|k| !k.starts_with(DAY)), "no day bucket was written");
    }
}

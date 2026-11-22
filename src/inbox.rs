//! claude-inbox's list, docked on the right. claude-inbox owns the triage: it writes the
//! sections to a snapshot file, which is read here on the tick, and takes the pane's
//! requests as files in a directory, as its Claude Code pane leaves them. Nothing here
//! decides where a session belongs.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use serde::Deserialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const WIDTH: u16 = 48;
/// The writer rewrites every 5 s even when nothing changed, so three missed beats is a
/// writer that has quit.
const STALE_S: i64 = 15;
const CHORD: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Pinned,
    NeedsYou,
    Active,
    Working,
    Snoozed,
    Settled,
}

const ORDER: [Section; 6] = [
    Section::Pinned,
    Section::NeedsYou,
    Section::Active,
    Section::Working,
    Section::Snoozed,
    Section::Settled,
];

impl Section {
    fn key(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::NeedsYou => "needs_you",
            Self::Active => "active",
            Self::Working => "working",
            Self::Snoozed => "snoozed",
            Self::Settled => "settled",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Pinned => "Pinned",
            Self::NeedsYou => "Needs you",
            Self::Active => "Active",
            Self::Working => "Working",
            Self::Snoozed => "Snoozed",
            Self::Settled => "Settled",
        }
    }

    fn fold(self) -> Option<usize> {
        match self {
            Self::Working => Some(0),
            Self::Snoozed => Some(1),
            Self::Settled => Some(2),
            _ => None,
        }
    }

    fn is_quiet(self) -> bool {
        matches!(self, Self::Snoozed | Self::Settled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum WakeAt {
    At(i64),
    Word(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequest {
    pub short: String,
    #[serde(default)]
    pub state: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Row {
    #[serde(skip, default = "pinned")]
    pub section: Section,
    pub id: Option<String>,
    pub label: Option<String>,
    pub state: Option<String>,
    #[serde(default)]
    pub actionable: bool,
    #[serde(default)]
    pub waiting: bool,
    #[serde(default)]
    pub unread: bool,
    pub line: Option<String>,
    pub wake_at: Option<WakeAt>,
    pub last_message_at: Option<i64>,
    pub remote: Option<String>,
    pub color: Option<String>,
    // A chip without its link cannot be opened, so it is not drawn.
    #[serde(default, deserialize_with = "lenient")]
    pub pr: Option<PullRequest>,
    pub backend: Option<String>,
    pub thread: Option<String>,
}

fn pinned() -> Section {
    Section::Pinned
}

fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<PullRequest>, D::Error> {
    Ok(serde_json::Value::deserialize(d)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok()))
}

impl Row {
    fn label(&self) -> &str {
        self.label
            .as_deref()
            .or(self.id.as_deref())
            .unwrap_or_default()
    }

    fn state(&self) -> &str {
        self.state.as_deref().unwrap_or("unknown")
    }

    pub fn t3_thread(&self) -> Option<&str> {
        (self.backend.as_deref() == Some("t3"))
            .then_some(self.thread.as_deref())
            .flatten()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub written_at: i64,
    pub update: Option<String>,
    pub rows: Vec<Row>,
}

pub fn parse(text: &str) -> Option<Snapshot> {
    #[derive(Deserialize)]
    struct File {
        version: u32,
        #[serde(default)]
        written_at: i64,
        update: Option<String>,
        #[serde(default)]
        sections: HashMap<String, Vec<serde_json::Value>>,
    }
    let file: File = serde_json::from_str(text).ok()?;
    if file.version != 1 {
        return None;
    }
    let rows = ORDER
        .iter()
        .flat_map(|section| {
            file.sections
                .get(section.key())
                .into_iter()
                .flatten()
                // A row this cannot read is left out rather than the whole list.
                .filter_map(|row| serde_json::from_value::<Row>(row.clone()).ok())
                .map(|row| Row {
                    section: *section,
                    ..row
                })
        })
        .collect();
    Some(Snapshot {
        written_at: file.written_at,
        update: file.update,
        rows,
    })
}

pub fn write_handoff(path: &Path, id: &str) -> std::io::Result<()> {
    std::fs::write(path, serde_json::json!({ "attach": id }).to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Leave,
    Toast(String, bool),
    Open(String),
    Attach(String),
    Copy(String),
    Url(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item<'a> {
    Row(&'a Row),
    Fold(Section, usize),
}

struct Group<'a> {
    section: Option<Section>,
    items: Vec<Item<'a>>,
}

/// What the cursor is on, held apart from the list a key may change.
enum Picked {
    Nothing,
    Fold(Section),
    /// The thread open in tria, which leads the list.
    Here(Row),
    Row(Row),
}

impl Picked {
    fn row(&self) -> Option<&Row> {
        match self {
            Self::Here(row) | Self::Row(row) => Some(row),
            _ => None,
        }
    }

    /// The fold `z` and `h` act on: the one picked, or the one its row is in.
    fn fold(&self) -> Option<Section> {
        match self {
            Self::Fold(section) => Some(*section),
            Self::Row(row) => Some(row.section),
            _ => None,
        }
        .filter(|section| section.fold().is_some())
    }
}

#[derive(Debug)]
pub struct Pane {
    snapshot_path: PathBuf,
    actions: PathBuf,
    read_at: Option<SystemTime>,
    /// `None` while there is no snapshot file: claude-inbox is not running.
    snapshot: Option<Snapshot>,
    cursor: usize,
    folds: [bool; 3],
    chord: Option<(char, Instant)>,
    offset: usize,
    sent: u32,
}

impl Default for Pane {
    fn default() -> Self {
        // A test's app reads no real inbox.
        if cfg!(test) {
            return Self::at(Path::new("/nonexistent/claude-inbox"));
        }
        let dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".config")
            .join("claude-inbox");
        Self::at(&dir)
    }
}

impl Pane {
    pub fn at(dir: &Path) -> Self {
        Self {
            snapshot_path: dir.join("snapshot.json"),
            actions: dir.join("actions"),
            read_at: None,
            snapshot: None,
            cursor: 0,
            folds: [true; 3],
            chord: None,
            offset: 0,
            sent: 0,
        }
    }

    pub fn poll(&mut self) {
        let Ok(modified) = std::fs::metadata(&self.snapshot_path).and_then(|m| m.modified()) else {
            self.snapshot = None;
            self.read_at = None;
            return;
        };
        if self.read_at == Some(modified) {
            return;
        }
        // Torn mid-write: kept as it was, and read again on the next tick.
        if let Some(snapshot) = std::fs::read_to_string(&self.snapshot_path)
            .ok()
            .as_deref()
            .and_then(parse)
        {
            self.snapshot = Some(snapshot);
            self.read_at = Some(modified);
        }
    }

    #[cfg(test)]
    pub fn give(&mut self, snapshot: Snapshot) {
        self.snapshot = Some(snapshot);
    }

    fn rows(&self) -> &[Row] {
        self.snapshot.as_ref().map_or(&[], |s| s.rows.as_slice())
    }

    fn groups(&self, current: Option<&str>) -> Vec<Group<'_>> {
        let here = |row: &Row| current.is_some() && row.t3_thread() == current;
        let mut groups: Vec<Group> = self
            .rows()
            .iter()
            .find(|row| here(row))
            .map(|row| Group {
                section: None,
                items: vec![Item::Row(row)],
            })
            .into_iter()
            .collect();
        for section in ORDER {
            let own: Vec<&Row> = self
                .rows()
                .iter()
                .filter(|row| row.section == section && !here(row))
                .collect();
            if own.is_empty() {
                continue;
            }
            let folded = section.fold().is_some_and(|at| self.folds[at]);
            let items = if folded {
                vec![Item::Fold(section, own.len())]
            } else {
                own.into_iter().map(Item::Row).collect()
            };
            groups.push(Group {
                section: Some(section),
                items,
            });
        }
        groups
    }

    fn flat<'a>(groups: &[Group<'a>]) -> Vec<Item<'a>> {
        groups
            .iter()
            .flat_map(|g| g.items.iter().copied())
            .collect()
    }

    fn selection(&self, len: usize) -> usize {
        self.cursor.min(len.saturating_sub(1))
    }

    fn chord(&self) -> Option<char> {
        self.chord
            .filter(|(_, at)| at.elapsed() < CHORD)
            .map(|(c, _)| c)
    }

    pub fn key(&mut self, key: KeyEvent, current: Option<&str>) -> Effect {
        let chord = self.chord();
        self.chord = None;
        if key.code == KeyCode::Esc {
            return Effect::Leave;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Effect::None;
        }
        let groups = self.groups(current);
        let flat = Self::flat(&groups);
        let at = self.selection(flat.len());
        let leads = groups.first().is_some_and(|g| g.section.is_none());
        let picked = match flat.get(at) {
            None => Picked::Nothing,
            Some(Item::Fold(section, _)) => Picked::Fold(*section),
            Some(Item::Row(row)) if leads && at == 0 => Picked::Here((*row).clone()),
            Some(Item::Row(row)) => Picked::Row((*row).clone()),
        };
        let heads: Vec<usize> = groups
            .iter()
            .scan(0, |start, group| {
                let head = *start;
                *start += group.items.len();
                Some(head)
            })
            .collect();
        let last = flat.len().saturating_sub(1);
        drop(groups);
        let fold = picked.fold();
        let row = picked.row().cloned();
        let KeyCode::Char(c) = key.code else {
            return match key.code {
                KeyCode::Down => self.move_to(at + 1, last),
                KeyCode::Up => self.move_to(at.saturating_sub(1), last),
                KeyCode::Enter => self.enter(picked, current),
                _ => Effect::None,
            };
        };
        match (chord, c) {
            (Some('g'), 'g') => self.move_to(0, last),
            (Some('g'), 'e') => self.move_to(last, last),
            (Some('z'), 'a') => self.set_fold(fold, !fold.is_some_and(|s| self.folded(s)), current),
            (Some('z'), 'o') => self.set_fold(fold, true, current),
            (Some('z'), 'c') => self.set_fold(fold, false, current),
            (Some('s'), digit @ '1'..='4') => {
                let (choice, said) = match digit {
                    '1' => ("m15", "15m"),
                    '2' => ("h1", "1h"),
                    '3' => ("tomorrow_9am", "tomorrow 9am"),
                    _ => ("until_woken", "until woken"),
                };
                match row.as_ref().filter(|r| r.actionable && r.id.is_some()) {
                    Some(row) => self.ask(
                        "snooze",
                        row,
                        Some(choice),
                        format!("snoozed {} {said}", row.label()),
                    ),
                    None => Effect::Toast("nothing here can be snoozed".into(), false),
                }
            }
            (Some(_), _) => Effect::None,
            (None, 'j') => self.move_to(at + 1, last),
            (None, 'k') => self.move_to(at.saturating_sub(1), last),
            (None, 'G') => self.move_to(last, last),
            (None, 'n') => self.move_to(heads.iter().copied().find(|h| *h > at).unwrap_or(0), last),
            (None, 'p') => self.move_to(
                heads
                    .iter()
                    .copied()
                    .rfind(|h| *h < at)
                    .or(heads.last().copied())
                    .unwrap_or(0),
                last,
            ),
            (None, 'g' | 'z' | 's') => {
                self.chord = Some((c, Instant::now()));
                Effect::None
            }
            (None, 'h') => self.set_fold(fold, false, current),
            (None, 'l') => self.enter(picked, current),
            (None, 'x') => self.on_row(row, "settle", "settled"),
            (None, 'u') => self.on_row(row, "wake", "woke"),
            (None, 't') => self.on_row(row, "pin", "pinned"),
            (None, 'y') => match row {
                Some(row) => Effect::Copy(
                    row.pr
                        .as_ref()
                        .map(|pr| pr.url.clone())
                        .or(row.remote.clone())
                        .or(row.id.clone())
                        .unwrap_or_else(|| row.label().to_string()),
                ),
                None => Effect::Toast("nothing to copy".into(), false),
            },
            (None, 'o') => match row.and_then(|row| row.pr) {
                Some(pr) => Effect::Url(pr.url),
                None => Effect::Toast("no pull request linked".into(), false),
            },
            (None, 'w') => match row.and_then(|row| row.remote) {
                Some(url) => Effect::Url(url),
                None => Effect::Toast("no claude.ai/code page for this session".into(), false),
            },
            (None, 'q') => Effect::Leave,
            _ => Effect::None,
        }
    }

    fn folded(&self, section: Section) -> bool {
        section.fold().is_some_and(|at| self.folds[at])
    }

    fn move_to(&mut self, to: usize, last: usize) -> Effect {
        self.cursor = to.min(last);
        Effect::None
    }

    fn set_fold(&mut self, section: Option<Section>, open: bool, current: Option<&str>) -> Effect {
        let Some(section) = section else {
            return Effect::None;
        };
        if let Some(at) = section.fold() {
            self.folds[at] = !open;
        }
        let groups = self.groups(current);
        let mut head = 0;
        for group in &groups {
            if group.section == Some(section) {
                self.cursor = head;
                break;
            }
            head += group.items.len();
        }
        Effect::None
    }

    fn enter(&mut self, picked: Picked, current: Option<&str>) -> Effect {
        let row = match picked {
            Picked::Nothing => return Effect::None,
            Picked::Fold(section) => return self.set_fold(Some(section), true, current),
            Picked::Here(_) => return Effect::Toast("you are in this session".into(), false),
            Picked::Row(row) => row,
        };
        if let Some(thread) = row.t3_thread() {
            // claude-inbox marks a row read when it attaches one; this is the attach.
            if row.actionable {
                let _ = self.request("acknowledge", &row, None);
            }
            return Effect::Open(thread.to_string());
        }
        match row.id.as_ref().filter(|_| row.actionable) {
            Some(id) => Effect::Attach(id.clone()),
            None => Effect::Toast("this session cannot be attached".into(), false),
        }
    }

    fn on_row(&mut self, row: Option<Row>, action: &str, verb: &str) -> Effect {
        match row.filter(|r| r.actionable && r.id.is_some()) {
            Some(row) => {
                let said = format!("{verb} {}", row.label());
                self.ask(action, &row, None, said)
            }
            None => Effect::Toast("nothing here can be changed from the pane".into(), false),
        }
    }

    fn ask(&mut self, action: &str, row: &Row, choice: Option<&str>, said: String) -> Effect {
        match self.request(action, row, choice) {
            Ok(()) => Effect::Toast(said, false),
            Err(err) => Effect::Toast(format!("could not ask claude-inbox: {err}"), true),
        }
    }

    /// Written under another name and renamed: claude-inbox takes any `.json` it finds.
    fn request(&mut self, action: &str, row: &Row, choice: Option<&str>) -> std::io::Result<()> {
        let Some(id) = &row.id else {
            return Ok(());
        };
        let at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut request = serde_json::json!({ "action": action, "id": id, "at": at });
        if let Some(choice) = choice {
            request["choice"] = choice.into();
        }
        self.sent += 1;
        std::fs::create_dir_all(&self.actions)?;
        let name = format!("tria-{}-{at}-{}", std::process::id(), self.sent);
        let part = self.actions.join(format!("{name}.part"));
        std::fs::write(&part, request.to_string())?;
        std::fs::rename(part, self.actions.join(format!("{name}.json")))
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        current: Option<&str>,
        now: i64,
    ) {
        let block = Block::new()
            .borders(Borders::LEFT)
            .border_style(if focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let inner = Rect {
            x: inner.x + 1,
            width: inner.width.saturating_sub(2),
            ..inner
        };
        let width = inner.width as usize;
        let dim = Style::default().add_modifier(Modifier::DIM);

        let groups = self.groups(current);
        let flat = Self::flat(&groups);
        let at = self.selection(flat.len());
        let listed: Vec<&Row> = self
            .rows()
            .iter()
            .filter(|row| !(current.is_some() && row.t3_thread() == current))
            .collect();

        let mut top = vec![header(&listed, groups.is_empty())];
        let notice = match &self.snapshot {
            None => Some("claude-inbox is not running.".to_string()),
            // Seconds, since a stale writer is under a minute gone more often than not.
            Some(s) if now - s.written_at > STALE_S => Some(match now - s.written_at {
                gone @ ..60 => format!("claude-inbox stopped {gone}s ago; rows are stale."),
                gone => format!("claude-inbox stopped {}; rows are stale.", ago(gone)),
            }),
            Some(_) => None,
        };
        match (
            &notice,
            self.snapshot.as_ref().and_then(|s| s.update.as_ref()),
        ) {
            (Some(notice), _) => top.push(Line::styled(
                fit(notice, width),
                Style::default().fg(Color::Yellow),
            )),
            (None, Some(update)) => {
                top.push(Line::styled(format!("{update} · claude-inbox update"), dim))
            }
            (None, None) if flat.is_empty() => top.push(Line::styled("No sessions.", dim)),
            _ => {}
        }

        let mut body: Vec<Line> = Vec::new();
        let mut selected_line = 0;
        let mut index = 0;
        for group in &groups {
            body.push(Line::default());
            let title = match group.section {
                None => "This session".to_string(),
                Some(section) => format!(
                    "{} · {}",
                    section.title(),
                    listed.iter().filter(|r| r.section == section).count()
                ),
            };
            body.push(Line::styled(title, dim.add_modifier(Modifier::BOLD)));
            for item in &group.items {
                let here = index == at;
                if here {
                    selected_line = body.len();
                }
                match item {
                    Item::Fold(_, count) => body.push(Line::from(vec![
                        cursor(here),
                        Span::styled(format!("▸ {count} folded · za opens"), dim),
                    ])),
                    Item::Row(row) => body.extend(row_lines(row, here, width, now)),
                }
                index += 1;
            }
        }

        let chord = self.chord().filter(|_| focused);
        let mut foot = vec![Line::default()];
        match chord {
            _ if !focused => foot.push(Line::styled("ctrl+x tab takes you here", dim)),
            None => foot.push(Line::styled("esc leaves", dim)),
            Some(c) => {
                foot.push(Line::styled(format!("{c}…"), dim));
                for (key, what) in menu(c) {
                    foot.push(Line::from(vec![
                        Span::styled("  ", dim),
                        Span::styled(*key, dim.add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" {what}"), dim),
                    ]));
                }
            }
        }

        let height = inner.height as usize;
        let room = height.saturating_sub(top.len() + foot.len());
        if selected_line < self.offset {
            self.offset = selected_line.saturating_sub(1);
        } else if selected_line + 2 > self.offset + room {
            self.offset = (selected_line + 2).saturating_sub(room);
        }
        self.offset = self.offset.min(body.len().saturating_sub(room));
        let mut lines = top;
        lines.extend(body.into_iter().skip(self.offset).take(room));
        let fill = height.saturating_sub(lines.len() + foot.len());
        lines.extend(std::iter::repeat_n(Line::default(), fill));
        lines.extend(foot);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn menu(chord: char) -> &'static [(&'static str, &'static str)] {
    match chord {
        'g' => &[("g", "first"), ("e", "last")],
        'z' => &[
            ("a", "toggle the fold"),
            ("o", "open it"),
            ("c", "close it"),
        ],
        _ => &[
            ("1", "15 minutes"),
            ("2", "1 hour"),
            ("3", "tomorrow 9am"),
            ("4", "until woken"),
        ],
    }
}

fn header(listed: &[&Row], nothing: bool) -> Line<'static> {
    let count = |of: &dyn Fn(&Row) -> bool| listed.iter().filter(|r| of(r)).count();
    let running = |r: &Row| matches!(r.section, Section::Active | Section::Working);
    let chips = [
        (
            count(&|r| r.section == Section::Pinned),
            "★",
            Some(Color::Cyan),
        ),
        (
            count(&|r| r.section == Section::NeedsYou),
            "●",
            Some(Color::Red),
        ),
        (
            count(&|r| running(r) && r.state() == "working" && !r.waiting),
            "✻",
            Some(Color::Yellow),
        ),
        (
            count(&|r| running(r) && r.waiting),
            "◌",
            Some(Color::Yellow),
        ),
        (
            count(&|r| r.section == Section::Snoozed),
            "z",
            Some(Color::Magenta),
        ),
        (count(&|r| r.section == Section::Settled), "◦", None),
    ];
    let mut spans = vec![Span::styled(
        "claude-inbox",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    for (n, glyph, color) in chips.into_iter().filter(|(n, _, _)| *n > 0) {
        let style = match color {
            Some(color) => Style::default().fg(color),
            None => Style::default().add_modifier(Modifier::DIM),
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("{glyph} {n}"), style));
    }
    if nothing {
        spans.push(Span::styled(
            "  nothing running",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn cursor(here: bool) -> Span<'static> {
    if here {
        Span::styled("› ", Style::default().fg(Color::Yellow))
    } else {
        Span::raw("  ")
    }
}

fn state_style(state: &str) -> Style {
    match state {
        "blocked" | "failed" => Style::default().fg(Color::Red),
        "working" => Style::default().fg(Color::Green),
        "idle" => Style::default().fg(Color::Cyan),
        _ => Style::default(),
    }
}

fn glyph(state: &str) -> &'static str {
    match state {
        "blocked" => "?",
        "failed" => "✗",
        "working" => "◐",
        "idle" => "◌",
        "done" | "stopped" => "✓",
        _ => "·",
    }
}

fn pr_color(state: &str) -> Option<Color> {
    match state {
        "open" => Some(Color::Green),
        "draft" => Some(Color::Yellow),
        "merged" => Some(Color::Magenta),
        "closed" => Some(Color::Red),
        _ => None,
    }
}

/// `/color`'s names as a label's background: the deep 256-colour shades the Claude Code
/// pane draws, so the terminal's own ink stays readable on them.
fn tint(color: &str) -> Option<Color> {
    let index = match color {
        "red" => 88,
        "green" => 22,
        "yellow" => 94,
        "blue" => 24,
        "purple" => 54,
        "cyan" => 23,
        "orange" => 130,
        "pink" => 89,
        _ => return None,
    };
    Some(Color::Indexed(index))
}

fn row_lines(row: &Row, here: bool, width: usize, now: i64) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let quiet = row.section.is_quiet();
    let state = row.state();
    // A finished or blocked row attached to since dims, as claude-inbox's Active section
    // draws it, unless it is tinted: faint ink on a /color background is unreadable.
    let read = !quiet
        && row.actionable
        && matches!(state, "done" | "stopped" | "blocked" | "failed")
        && !row.unread;
    let needs_you = matches!(state, "blocked" | "failed");
    let dot = !quiet && (row.unread || (needs_you && !row.actionable));
    let tint = row.color.as_deref().and_then(tint);

    let mark = if here {
        cursor(true)
    } else if dot {
        Span::styled("● ", Style::default().fg(Color::Blue))
    } else {
        Span::raw("  ")
    };
    let mut glyph_style = state_style(state);
    if quiet || state == "done" {
        glyph_style = glyph_style.add_modifier(Modifier::DIM);
    }
    let glyph = Span::styled(format!("{} ", glyph(state)), glyph_style);

    // The PR and the age keep their cells; the label gives way, then the age, then the PR.
    let mut right: Vec<Span> = Vec::new();
    if let Some(pr) = &row.pr {
        let style = if quiet {
            dim
        } else {
            pr_color(&pr.state).map_or(Style::default(), |c| Style::default().fg(c))
        };
        right.push(Span::raw(" "));
        right.push(Span::styled(
            pr.short.clone(),
            style.add_modifier(Modifier::UNDERLINED),
        ));
        right.push(Span::styled(format!(" {}", pr.state), style));
    }
    let age = row
        .last_message_at
        .map(|at| Span::styled(format!(" {}", ago(now - at)), dim));
    let spans_width = |spans: &[Span]| spans.iter().map(|s| s.width()).sum::<usize>();
    let text = match &row.wake_at {
        Some(wake) => format!("{} · {}", row.label(), wake_label(wake)),
        None => row.label().to_string(),
    };
    const LEAST_LABEL: usize = 8;
    let lead = 4;
    let mut right_width = spans_width(&right) + age.as_ref().map_or(0, |a| a.width());
    let mut age = age;
    if width < lead + LEAST_LABEL + right_width {
        age = None;
        right_width = spans_width(&right);
    }
    if width < lead + LEAST_LABEL + right_width {
        right.clear();
        right_width = 0;
    }
    right.extend(age);
    let room = width.saturating_sub(lead + right_width);
    let label = fit(&text, room);
    let mut label_style = Style::default();
    if let Some(tint) = tint {
        label_style = label_style.bg(tint);
    }
    if row.unread && !quiet {
        label_style = label_style.add_modifier(Modifier::BOLD);
    }
    if (quiet || read) && !here && tint.is_none() {
        label_style = label_style.add_modifier(Modifier::DIM);
    }
    let pad = room.saturating_sub(label.width());
    let mut spans = vec![
        mark,
        glyph,
        Span::styled(label, label_style),
        Span::raw(" ".repeat(pad)),
    ];
    spans.extend(right);
    let mut lines = vec![Line::from(spans)];
    if !quiet && let Some(line) = &row.line {
        lines.push(Line::styled(fit(&format!("    ↳ {line}"), width), dim));
    }
    lines
}

fn fit(text: &str, max: usize) -> String {
    let text = text.replace(['\n', '\r'], " ");
    if text.width() <= max {
        return text;
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

fn ago(seconds: i64) -> String {
    match seconds {
        ..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn wake_label(wake: &WakeAt) -> String {
    let WakeAt::At(at) = wake else {
        return "until woken".into();
    };
    // Local time, which the `time` crate will not tell a threaded program.
    let at = *at as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals; localtime_r writes only `tm`.
    if unsafe { libc::localtime_r(&at, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    const SNAPSHOT: &str = r##"{
        "version": 1, "written_at": 1000,
        "sections": {
            "needs_you": [{"id": "a1", "harness": "claude", "label": "fix the login", "state": "blocked",
                           "actionable": true, "unread": true, "line": "say the word",
                           "pr": {"short": "#905", "state": "open", "url": "https://github.com/o/r/pull/905"}}],
            "active": [{"id": "t1", "harness": "codex", "label": "port the parser", "state": "done",
                        "actionable": true, "backend": "t3", "thread": "thread-1", "cwd": "/tmp/x",
                        "last_message_at": 880},
                       {"id": "t2", "harness": "antigravity", "label": "here", "state": "working",
                        "actionable": true, "backend": "t3", "thread": "thread-2"}],
            "settled": [{"id": "s1", "label": "old", "state": "done", "actionable": true}],
            "pinned": [{"label": "unreadable", "pr": {"short": "#1"}, "wake_at": "until_woken"}]
        }
    }"##;

    fn pane() -> (tempdir::Dir, Pane) {
        let dir = tempdir::Dir::new();
        let mut pane = Pane::at(dir.path());
        pane.give(parse(SNAPSHOT).unwrap());
        (dir, pane)
    }

    fn press(pane: &mut Pane, keys: &str, current: Option<&str>) -> Effect {
        let mut last = Effect::None;
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                c => KeyCode::Char(c),
            };
            last = pane.key(KeyEvent::new(code, KeyModifiers::NONE), current);
        }
        last
    }

    fn requests(dir: &Path) -> Vec<serde_json::Value> {
        let mut names: Vec<_> = std::fs::read_dir(dir.join("actions"))
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        names.sort();
        names
            .iter()
            .map(|p| {
                assert_eq!(p.extension().unwrap(), "json");
                serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
            })
            .collect()
    }

    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let path =
                    std::env::temp_dir().join(format!("tria-inbox-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn the_snapshot_reads_into_rows_in_section_order() {
        let snapshot = parse(SNAPSHOT).unwrap();
        assert_eq!(snapshot.written_at, 1000);
        let rows: Vec<(Section, &str)> = snapshot
            .rows
            .iter()
            .map(|r| (r.section, r.label()))
            .collect();
        assert_eq!(
            rows,
            [
                (Section::Pinned, "unreadable"),
                (Section::NeedsYou, "fix the login"),
                (Section::Active, "port the parser"),
                (Section::Active, "here"),
                (Section::Settled, "old"),
            ]
        );
        let t3 = &snapshot.rows[2];
        assert_eq!(t3.t3_thread(), Some("thread-1"));
        assert_eq!(snapshot.rows[1].t3_thread(), None, "a native row");
        assert_eq!(snapshot.rows[1].pr.as_ref().unwrap().short, "#905");
        assert_eq!(snapshot.rows[0].pr, None, "a chip without its link");
        assert_eq!(
            snapshot.rows[0].wake_at,
            Some(WakeAt::Word("until_woken".into()))
        );
        assert!(!snapshot.rows[0].actionable);
    }

    #[test]
    fn a_torn_or_unknown_snapshot_is_none() {
        assert_eq!(parse(&SNAPSHOT[..40]), None);
        assert_eq!(parse(r#"{"version": 2, "sections": {}}"#), None);
        assert_eq!(parse(r#"{"version": 1}"#).unwrap().rows, []);
    }

    #[test]
    fn a_changed_file_is_read_again_and_a_missing_one_is_not_running() {
        let dir = tempdir::Dir::new();
        let mut pane = Pane::at(dir.path());
        pane.poll();
        assert!(pane.snapshot.is_none());
        std::fs::write(dir.path().join("snapshot.json"), SNAPSHOT).unwrap();
        pane.poll();
        assert_eq!(pane.rows().len(), 5);
        std::fs::remove_file(dir.path().join("snapshot.json")).unwrap();
        pane.poll();
        assert!(pane.snapshot.is_none());
    }

    #[test]
    fn the_handoff_names_the_row_to_attach() {
        let dir = tempdir::Dir::new();
        let path = dir.path().join("handoff");
        write_handoff(&path, "a1").unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written, serde_json::json!({ "attach": "a1" }));
    }

    #[test]
    fn enter_opens_a_t3_row_in_place_and_hands_any_other_back() {
        let (dir, mut pane) = pane();
        assert_eq!(
            press(&mut pane, "\n", None),
            Effect::Toast("this session cannot be attached".into(), false)
        );
        assert_eq!(press(&mut pane, "j\n", None), Effect::Attach("a1".into()));
        assert_eq!(
            press(&mut pane, "j\n", None),
            Effect::Open("thread-1".into())
        );
        assert_eq!(
            requests(dir.path()),
            [
                serde_json::json!({"action": "acknowledge", "id": "t1", "at": requests(dir.path())[0]["at"]})
            ]
        );
    }

    #[test]
    fn the_open_thread_leads_and_enter_on_it_stays() {
        let (_dir, mut pane) = pane();
        assert_eq!(
            press(&mut pane, "\n", Some("thread-2")),
            Effect::Toast("you are in this session".into(), false)
        );
        assert_eq!(
            press(&mut pane, "jjj\n", Some("thread-2")),
            Effect::Open("thread-1".into())
        );
    }

    #[test]
    fn the_triage_keys_leave_requests_as_the_claude_code_pane_does() {
        let (dir, mut pane) = pane();
        press(&mut pane, "jx", None);
        press(&mut pane, "t", None);
        press(&mut pane, "s3", None);
        let asked: Vec<_> = requests(dir.path())
            .into_iter()
            .map(|mut r| {
                assert!(
                    r["at"].as_u64().unwrap() > 1_700_000_000_000,
                    "milliseconds"
                );
                r.as_object_mut().unwrap().remove("at");
                r
            })
            .collect();
        assert_eq!(
            asked,
            [
                serde_json::json!({"action": "settle", "id": "a1"}),
                serde_json::json!({"action": "pin", "id": "a1"}),
                serde_json::json!({"action": "snooze", "id": "a1", "choice": "tomorrow_9am"}),
            ]
        );
        assert_eq!(
            press(&mut pane, "ggx", None),
            Effect::Toast("nothing here can be changed from the pane".into(), false)
        );
    }

    #[test]
    fn a_fold_opens_and_closes() {
        let (_dir, mut pane) = pane();
        assert_eq!(press(&mut pane, "ge", None), Effect::None);
        let groups = pane.groups(None);
        assert!(matches!(
            Pane::flat(&groups).last(),
            Some(Item::Fold(Section::Settled, 1))
        ));
        drop(groups);
        press(&mut pane, "\n", None);
        assert!(!pane.folded(Section::Settled));
        press(&mut pane, "zc", None);
        assert!(pane.folded(Section::Settled));
        press(&mut pane, "zo", None);
        assert!(!pane.folded(Section::Settled));
        press(&mut pane, "h", None);
        assert!(pane.folded(Section::Settled));
    }

    #[test]
    fn esc_leaves_and_section_keys_jump() {
        let (_dir, mut pane) = pane();
        assert_eq!(
            pane.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None),
            Effect::Leave
        );
        press(&mut pane, "nn", None);
        assert_eq!(pane.cursor, 2, "Active's first row");
        press(&mut pane, "p", None);
        assert_eq!(pane.cursor, 1);
    }

    fn drawn(pane: &mut Pane, focused: bool, current: Option<&str>, now: i64) -> String {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, 24)).unwrap();
        terminal
            .draw(|frame| pane.draw(frame, frame.area(), focused, current, now))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_pane_draws_the_sections_as_the_claude_code_pane_does() {
        let (_dir, mut pane) = pane();
        let text = drawn(&mut pane, true, Some("thread-2"), 1001);
        let lines: Vec<&str> = text.lines().map(|l| l.trim_start_matches('│')).collect();
        assert_eq!(lines[0], " claude-inbox  ★ 1  ● 1  ◦ 1");
        assert_eq!(lines[2], " This session");
        assert_eq!(lines[3], " › ◐ here");
        assert_eq!(lines[5], " Pinned · 1");
        assert_eq!(lines[6], "   · unreadable · until woken");
        assert_eq!(lines[8], " Needs you · 1");
        assert!(lines[9].starts_with(" ● ? fix the login"), "{}", lines[9]);
        assert!(lines[9].ends_with("#905 open"), "{}", lines[9]);
        assert_eq!(lines[10], "     ↳ say the word");
        assert!(lines[13].ends_with("2m ago"), "{}", lines[13]);
        assert_eq!(lines[16], "   ▸ 1 folded · za opens");
        assert_eq!(lines[23], " esc leaves");
    }

    #[test]
    fn a_quiet_writer_is_stale_and_no_file_is_not_running() {
        let (_dir, mut pane) = pane();
        assert!(!drawn(&mut pane, false, None, 1010).contains("stale"));
        let text = drawn(&mut pane, false, None, 1020);
        assert!(
            text.contains("claude-inbox stopped 20s ago; rows are stale."),
            "{text}"
        );
        let text = drawn(&mut pane, false, None, 1100);
        assert!(
            text.contains("claude-inbox stopped 1m ago; rows are stale."),
            "{text}"
        );
        assert!(text.contains("ctrl+x tab takes you here"));
        let mut empty = Pane::at(Path::new("/nonexistent"));
        empty.poll();
        let text = drawn(&mut empty, false, None, 1100);
        assert!(text.contains("claude-inbox is not running."), "{text}");
    }
}

//! The cache schema, and every derivation the UI needs from it.
//!
//! This module is the whole data layer: `cache.rs` finds and reads the file,
//! everything else in the crate asks *this* module questions. It is pure -- no
//! I/O, no processes, no clock of its own (callers pass `now`), so all of it is
//! unit-testable and all of it is tested at the bottom of the file.
//!
//! # Parsing rules, all of them deliberate
//!
//! * **Unknown fields are ignored.** serde does that by default; nothing here
//!   opts into `deny_unknown_fields`. The collector may grow fields (it already
//!   did once, see `SourceStatus::fetched_unix`) and an older binary must keep
//!   working against a newer cache.
//! * **Unknown *values* are kept, not rejected.** `review_decision`, `checks`,
//!   `kind`, `section` and `status_category` are enums with an `Other(String)`
//!   arm. GitHub adding a seventh check state must not turn the inbox into an
//!   error frame.
//! * **null is the same as absent.** collect.sh's own cache guard accepts
//!   `sources: null`, so this parser has to as well; the same goes for `items`
//!   and for every string field.
//! * **Timestamps are read as f64 and floored.** ui.sh floors `fetched_unix`
//!   for exactly this reason: a cache written with a fractional timestamp must
//!   not fail to parse.
//!
//! # Schema (version 1)
//!
//! ```json
//! { "version": 1, "fetched_unix": 1786541389,
//!   "sources": { "github": {"ok": true, "note": "", "fetched_unix": 1786541389},
//!                "jira":   {"ok": false, "note": "jira: unreachable ...",
//!                           "fetched_unix": 1786538000} },
//!   "items": [ ... ] }
//! ```
//!
//! The per-leg `fetched_unix` is the phase 2 addition and is written by the
//! phase 2 collector; `version` stays 1 because the change is additive and the
//! fzf front end keeps working. **It is therefore optional here** -- a cache
//! written by the phase 1 collector has no per-leg timestamps at all, and the
//! staleness helpers fall back to the top-level value.
//!
//! Retention semantics that go with it: when a leg fails, its items from the
//! previous cache are retained and its `fetched_unix` keeps the last successful
//! value. So `ok == false` with items present means "stale, and here is why",
//! and `ok == false` with no items means "empty, and here is why".

use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------- deserialisers

/// `null` and "missing" both mean "default". Pair with `#[serde(default)]`:
/// `default` covers the absent key, this covers an explicit `null`.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Unix seconds, floored. Accepts an integer, a float, or null.
fn unix<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.map_or(0, |v| v.floor() as i64))
}

/// Optional unix seconds, floored. Absent and null are both `None`.
fn opt_unix<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.map(|v| v.floor() as i64))
}

// --------------------------------------------------------------- string enums
//
// `#[serde(other)]` only works on a unit variant, so it cannot carry the
// unrecognised string. `#[serde(from = "String")]` + `From<String>` can, and
// gives an infallible parse: there is no input that makes these fail.

macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $( $variant:ident => $text:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
        #[serde(from = "String")]
        pub enum $name {
            $( $variant, )+
            /// A value this build does not know. Displayed verbatim, never fatal.
            Other(String),
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                match s.as_str() {
                    $( $text => Self::$variant, )+
                    _ => Self::Other(s),
                }
            }
        }

        impl $name {
            /// The verbatim API string, round-tripping `Other`.
            ///
            /// The preview pane calls this on `ReviewDecision`, `Checks` and
            /// `StatusCategory`; on `Kind` and `Section` nothing does, because
            /// those two are only ever matched on. Generating it uniformly is
            /// what makes the macro a macro -- suppressing the warning is
            /// cheaper than five hand-written impls that differ by nothing.
            #[allow(dead_code)]
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $text, )+
                    Self::Other(s) => s.as_str(),
                }
            }
        }
    };
}

string_enum! {
    /// `kind` on an item. Anything else is `Other` and is skipped by the views.
    Kind { Pr => "pr", Jira => "jira" }
}

string_enum! {
    /// `section` on an item -- the list's three groups, in render order.
    Section { Review => "review", Mine => "mine", Jira => "jira" }
}

string_enum! {
    /// GitHub's `reviewDecision`. Absent/null on a PR nobody has acted on.
    ReviewDecision {
        Approved => "APPROVED",
        ChangesRequested => "CHANGES_REQUESTED",
        ReviewRequired => "REVIEW_REQUIRED",
    }
}

string_enum! {
    /// GitHub's `statusCheckRollup` state.
    Checks {
        Success => "SUCCESS",
        Failure => "FAILURE",
        Pending => "PENDING",
        Expected => "EXPECTED",
        Error => "ERROR",
    }
}

string_enum! {
    /// Jira's `statusCategory`, derived by the collector from the locale-stable
    /// `statusCategory.key`. The kanban keys on THIS, never on `status`: status
    /// names are tenant- and locale-specific (this tenant shows `進行中` next to
    /// an English `To Do`) while the category is stable.
    StatusCategory {
        ToDo => "To Do",
        InProgress => "In Progress",
        Done => "Done",
    }
}

impl Checks {
    /// FAILURE and ERROR both read as "the run failed".
    pub fn is_failed(&self) -> bool {
        matches!(self, Checks::Failure | Checks::Error)
    }
    /// PENDING and EXPECTED both mean "not finished yet".
    pub fn is_pending(&self) -> bool {
        matches!(self, Checks::Pending | Checks::Expected)
    }
}

// ------------------------------------------------------------------ cache root

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Cache {
    /// Schema version. Still 1 in phase 2 -- the additions are backward
    /// compatible.
    #[serde(default, deserialize_with = "null_as_default")]
    pub version: u32,
    /// Newest of the per-leg timestamps. 0 when the cache predates it entirely.
    #[serde(default, deserialize_with = "unix")]
    pub fetched_unix: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub sources: Sources,
    #[serde(default, deserialize_with = "null_as_default")]
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Sources {
    #[serde(default, deserialize_with = "null_as_default")]
    pub github: SourceStatus,
    #[serde(default, deserialize_with = "null_as_default")]
    pub jira: SourceStatus,
}

/// One collector leg.
///
/// `ok == false` does **not** imply "no items": phase 2 retains the previous
/// run's items for a failed leg (this tenant produces intermittent
/// `curl exit 56`, and a transient blip must not empty the Jira list). Ask
/// [`Cache::source_state`] rather than reading `ok` alone.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SourceStatus {
    #[serde(default, deserialize_with = "null_as_default")]
    pub ok: bool,
    /// Human explanation. Non-empty on failure, and ALSO non-empty on some
    /// successes (a group-readable credential file is reported as ok+note), so
    /// never branch on note-emptiness as a proxy for failure.
    #[serde(default, deserialize_with = "null_as_default")]
    pub note: String,
    /// Phase 2, optional: the timestamp of this leg's last **successful** fetch.
    /// `None` on a cache written by the phase 1 collector.
    #[serde(default, deserialize_with = "opt_unix")]
    pub fetched_unix: Option<i64>,
}

/// One row. Flat rather than an enum-per-kind: the two kinds share `ref`, `url`,
/// `title`, `updated` and `body`, the views switch on `kind` in two places only,
/// and a flat struct keeps the kind-specific fields as plain `Option`s that are
/// simply absent on the other kind.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Item {
    #[serde(default, deserialize_with = "null_as_default")]
    pub kind: Kind,
    #[serde(default, deserialize_with = "null_as_default")]
    pub section: Section,
    /// Short human reference: `owner/repo#123` for a PR, the issue key for Jira.
    #[serde(rename = "ref", default, deserialize_with = "null_as_default")]
    pub r#ref: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub url: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub title: String,
    /// ISO8601, e.g. `2026-08-11T04:12:33Z`. See [`Item::day`].
    #[serde(default, deserialize_with = "null_as_default")]
    pub updated: String,
    /// Pre-fetched preview text. The reason the preview pane costs no network.
    #[serde(default, deserialize_with = "null_as_default")]
    pub body: String,

    // -- pull requests --------------------------------------------------------
    #[serde(default, deserialize_with = "null_as_default")]
    pub repo: String,
    /// Part of the schema; nothing in the UI shows it (the `ref` already
    /// carries `owner/repo#123`). Kept so the struct is a faithful record of
    /// what the collector writes.
    #[allow(dead_code)]
    pub number: Option<u64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub author: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub draft: bool,
    pub review_decision: Option<ReviewDecision>,
    pub checks: Option<Checks>,

    // -- jira -----------------------------------------------------------------
    #[serde(default, deserialize_with = "null_as_default")]
    pub key: String,
    /// Verbatim from the API, tenant- and locale-specific (`進行中`). Displayed
    /// as-is; never used for bucketing.
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    pub status_category: Option<StatusCategory>,
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub r#type: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub priority: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub project: String,
}

impl Default for Kind {
    fn default() -> Self {
        Kind::Other(String::new())
    }
}
impl Default for Section {
    fn default() -> Self {
        Section::Other(String::new())
    }
}

// ------------------------------------------------------------------- item view

// ------------------------------------------------------------- status glyphs
//
// Two fixed slots at the head of every row, each exactly TWO display columns:
// slot 1 is the review/workflow state, slot 2 is CI. They replace the phase 1
// `PR   ` / `JIRA ` marker (also 5 columns wide with its separator), so the row
// gained a scannable status column without costing the title any room.
//
// EVERY glyph here is East Asian Wide and carries no variation selector, so the
// two slots are 2 columns whichever combination is drawn and the refs after them
// stay aligned. `glyphs_are_two_columns_wide` asserts exactly that -- swapping in
// a prettier glyph that happens to be narrow would shear every row below it.

/// Slot 1, PRs. Precedence, most actionable first: someone asked for changes,
/// then it is your unfinished draft, then it is approved, then it is simply
/// waiting on a review.
pub const G_CHANGES_REQUESTED: &str = "🔴";
pub const G_DRAFT: &str = "📝";
pub const G_APPROVED: &str = "✅";
pub const G_NEEDS_REVIEW: &str = "👀";
/// Slot 1, Jira, keyed on `status_category` -- never on the tenant's status
/// name, which is localised (`進行中`) while the category is not.
pub const G_TODO: &str = "⚪";
pub const G_IN_PROGRESS: &str = "🔵";
pub const G_DONE: &str = "🏁";
/// Slot 2, CI. Blank on success: a green tick beside the approved tick would
/// read as two review states, and "CI is fine" is not news.
pub const G_CI_FAILED: &str = "❌";
pub const G_CI_PENDING: &str = "⏳";
/// An empty slot, two columns of nothing.
pub const G_NONE: &str = "  ";

impl Item {
    /// `MM-DD` out of the ISO8601 `updated`, i.e. bytes 5..10, matching the
    /// `def day` in ui.sh. Empty when the field is missing or malformed.
    pub fn day(&self) -> &str {
        let b = self.updated.as_bytes();
        if b.len() >= 10 && b.is_ascii() {
            &self.updated[5..10]
        } else {
            ""
        }
    }

    /// The two status slots, `(review_or_workflow, ci)`. See the glyph constants
    /// above for the precedence and why every one of them is two columns wide.
    pub fn status_glyphs(&self) -> (&'static str, &'static str) {
        if self.kind == Kind::Jira {
            let first = match &self.status_category {
                Some(StatusCategory::ToDo) => G_TODO,
                Some(StatusCategory::InProgress) => G_IN_PROGRESS,
                Some(StatusCategory::Done) => G_DONE,
                _ => G_NONE,
            };
            return (first, G_NONE);
        }
        let first = match &self.review_decision {
            Some(ReviewDecision::ChangesRequested) => G_CHANGES_REQUESTED,
            Some(ReviewDecision::Approved) if !self.draft => G_APPROVED,
            _ if self.draft => G_DRAFT,
            Some(ReviewDecision::Approved) => G_APPROVED,
            _ => G_NEEDS_REVIEW,
        };
        let second = match &self.checks {
            Some(c) if c.is_failed() => G_CI_FAILED,
            Some(c) if c.is_pending() => G_CI_PENDING,
            _ => G_NONE,
        };
        (first, second)
    }

    /// The Jira status as it is drawn: the tenant's own wording, padded to a
    /// fixed column so the titles beside it line up. Empty for a PR.
    pub fn status_badge(&self) -> String {
        if self.kind != Kind::Jira {
            return String::new();
        }
        pad_cols(&format!("[{}]", or(&self.status, "?")), STATUS_BADGE_COLS)
    }

    /// The list row as plain text -- what `--dump` prints, and exactly what the
    /// TUI draws once the colour is stripped:
    ///
    /// ```text
    /// 📝 ❌  owner/repo#123                 Title  (@author, 08-11)
    /// 🔵    ACME-123                    [進行中]     Summary  (Task, Medium, 08-11)
    /// ```
    ///
    /// Two status slots, then `ref` padded to 30 **display columns**, then the
    /// Jira status badge, the title, and a dim trailer. Kept here rather than in
    /// `view/list.rs` so `--dump` and the TUI can never drift apart; the test
    /// `row_spans_are_list_row` in that module enforces it.
    ///
    /// The glyphs replaced the phase 1 `[draft] [approved]` word tags. They carry
    /// the same information in a column that can be scanned down instead of read,
    /// which is the whole point, and they give the title back the room the tags
    /// used to take.
    pub fn list_row(&self) -> String {
        let (g1, g2) = self.status_glyphs();
        match self.kind {
            Kind::Jira => {
                let key = if self.r#ref.is_empty() {
                    &self.key
                } else {
                    &self.r#ref
                };
                format!(
                    "{} {}  {} {} {}  ({}, {}, {})",
                    g1,
                    g2,
                    pad_cols(key, 30),
                    self.status_badge(),
                    self.title,
                    or(&self.r#type, "?"),
                    or(&self.priority, "-"),
                    self.day(),
                )
            }
            _ => format!(
                "{} {}  {} {}  (@{}, {})",
                g1,
                g2,
                pad_cols(&self.r#ref, 30),
                self.title,
                or(&self.author, "ghost"),
                self.day(),
            ),
        }
    }

    /// Kanban card text: ref plus a truncated title. Tags are drawn separately
    /// by the kanban view, which has its own colour for them.
    pub fn card_title(&self, width: usize) -> String {
        truncate(&self.title, width)
    }

    /// Search predicate for `/`: case-insensitive substring over ref + title.
    /// `needle` must already be lowercase (the caller lowercases once per
    /// keystroke rather than once per item).
    pub fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        self.r#ref.to_lowercase().contains(needle) || self.title.to_lowercase().contains(needle)
    }
}

/// Width of the Jira status badge column. `[進行中]` is 8 columns and `[In
/// Review]` is 11, so 13 holds the realistic names and keeps the titles beside
/// them on one line. A longer one is truncated rather than shoving the column.
pub const STATUS_BADGE_COLS: usize = 13;

/// Right-pad (and truncate) to `n` **display columns**.
///
/// The phase 1 `jq` renderer counted characters, because it had no way to
/// measure anything else, and the port inherited that. It under-pads every CJK
/// string -- and both the Jira status names and half the PR titles here are CJK
/// -- so a row with a Japanese status sheared the column after it. Now that the
/// row format has moved on from phase 1 there is nothing left to keep parity
/// with, so alignment is measured the way the terminal actually draws it.
pub fn pad_cols(s: &str, n: usize) -> String {
    crate::view::pad_fit(s, n)
}

/// Truncate to `n` characters with a trailing `…` when it does not fit.
pub fn truncate(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let len = s.chars().count();
    if len <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn or<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() { fallback } else { s }
}

// ---------------------------------------------------------------- list sections

/// The three list groups, in render order, with the headers spelled verbatim.
pub const SECTIONS: [Section; 3] = [Section::Review, Section::Mine, Section::Jira];

impl Section {
    pub fn header(&self) -> &'static str {
        match self {
            Section::Review => "REVIEW REQUESTED",
            Section::Mine => "MY PULL REQUESTS",
            Section::Jira => "MY JIRA ISSUES",
            Section::Other(_) => "OTHER",
        }
    }

    /// Which collector leg fills this section -- the key into `sources`.
    pub fn source(&self) -> &'static str {
        match self {
            Section::Jira => "jira",
            _ => "github",
        }
    }

    /// Shown in place of the rows when the section is empty and its source is
    /// healthy. Same wording as phase 1.
    pub fn empty_message(&self) -> &'static str {
        match self {
            Section::Review => "no PRs are waiting on your review",
            Section::Mine => "you have no open pull requests",
            Section::Jira => "no unresolved issues are assigned to you",
            Section::Other(_) => "nothing here",
        }
    }
}

// ----------------------------------------------------------------------- kanban

/// The three kanban boards, cycled with Tab (and back with shift+Tab). Each is
/// one list section reshaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Board {
    #[default]
    Review,
    Mine,
    Jira,
}

/// List or kanban. Lives here rather than in `app` because it is persisted in
/// `config.json` and so needs the same serde treatment the rest of the schema
/// gets; `app` re-exports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    #[default]
    List,
    Kanban,
}

pub const BOARDS: [Board; 3] = [Board::Review, Board::Mine, Board::Jira];

/// Kanban columns. Headers are English and uppercase on every board, including
/// the Jira one whose underlying status strings are Japanese.
///
/// `Approved` is shared by boards 1 and 2 -- same meaning, same header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    // board 1
    NeedsReview,
    ChangesRequested,
    Approved,
    // board 2
    Draft,
    InReview,
    CiFailed,
    // board 3
    ToDo,
    InProgress,
    Done,
}

impl Column {
    pub fn header(&self) -> &'static str {
        match self {
            Column::NeedsReview => "NEEDS REVIEW",
            Column::ChangesRequested => "CHANGES REQUESTED",
            Column::Approved => "APPROVED",
            Column::Draft => "DRAFT",
            Column::InReview => "IN REVIEW",
            Column::CiFailed => "CI FAILED",
            Column::ToDo => "TO DO",
            Column::InProgress => "IN PROGRESS",
            Column::Done => "DONE",
        }
    }
}

impl Board {
    pub fn section(&self) -> Section {
        match self {
            Board::Review => Section::Review,
            Board::Mine => Section::Mine,
            Board::Jira => Section::Jira,
        }
    }

    pub fn header(&self) -> &'static str {
        self.section().header()
    }

    /// Columns in display order, left to right. Fixed by the contract -- there
    /// is no config file, on purpose: the one thing that would have needed
    /// configuring (Jira status names) is solved by keying on `status_category`.
    pub fn columns(&self) -> &'static [Column] {
        match self {
            Board::Review => &[Column::NeedsReview, Column::ChangesRequested, Column::Approved],
            Board::Mine => &[
                Column::Draft,
                Column::InReview,
                Column::Approved,
                Column::CiFailed,
            ],
            Board::Jira => &[Column::ToDo, Column::InProgress, Column::Done],
        }
    }

    /// Which column an item lands in. **Total**: every item of this board's
    /// section gets exactly one column, so no card can vanish. The fallbacks are
    /// deliberate, not accidental:
    ///
    /// * board 1 -- `null` and `REVIEW_REQUIRED` are both "needs review", and so
    ///   is any decision string this build does not recognise.
    /// * board 2 -- precedence is **CI FAILED > DRAFT > APPROVED > IN REVIEW**.
    ///   The contract makes CI FAILED explicit ("takes precedence"), and IN
    ///   REVIEW's own definition ("not draft, ...") puts DRAFT above it. IN
    ///   REVIEW is the fallback, which is what absorbs a non-draft
    ///   CHANGES_REQUESTED -- otherwise it would have nowhere to go.
    /// * board 3 -- an unrecognised `status_category` (or a missing one) reads
    ///   as TO DO: an unstarted-looking issue is the safer wrong guess than
    ///   claiming it is finished.
    pub fn column_of(&self, it: &Item) -> Column {
        match self {
            Board::Review => match &it.review_decision {
                Some(ReviewDecision::ChangesRequested) => Column::ChangesRequested,
                Some(ReviewDecision::Approved) => Column::Approved,
                _ => Column::NeedsReview,
            },
            Board::Mine => {
                if it.checks.as_ref().is_some_and(|c| c.is_failed()) {
                    Column::CiFailed
                } else if it.draft {
                    Column::Draft
                } else if it.review_decision == Some(ReviewDecision::Approved) {
                    Column::Approved
                } else {
                    Column::InReview
                }
            }
            Board::Jira => match &it.status_category {
                Some(StatusCategory::InProgress) => Column::InProgress,
                Some(StatusCategory::Done) => Column::Done,
                _ => Column::ToDo,
            },
        }
    }
}

// -------------------------------------------------------------------- staleness

/// What the header and the per-section banner need to know about one leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceState {
    pub name: &'static str,
    pub ok: bool,
    pub note: String,
    /// Last successful fetch for this leg; falls back to the cache's top-level
    /// `fetched_unix` when the collector predates per-leg timestamps.
    pub fetched_unix: i64,
    pub item_count: usize,
}

impl SourceState {
    /// The warning banner for a failed leg, or `None` when it is healthy.
    ///
    /// * with retained items: `jira: stale, 41m ago — <note>`
    /// * with no items:       `<note>`
    pub fn banner(&self, now: i64) -> Option<String> {
        if self.ok {
            return None;
        }
        // A leg can die without leaving a note; an empty warning row reads as a
        // rendering glitch rather than a diagnostic. Same fallback as ui.sh.
        let note = if self.note.trim().is_empty() {
            format!("{}: failed for an unknown reason", self.name)
        } else {
            self.note.clone()
        };
        if self.item_count > 0 {
            Some(format!(
                "{}: stale, {} — {}",
                self.name,
                age_str(now, self.fetched_unix),
                note
            ))
        } else {
            Some(note)
        }
    }
}

/// Cache age in English, mirroring `age_str` in ui.sh exactly: anything under a
/// minute -- including a clock that went backwards -- reads "just now", because
/// "0m ago" looks like a bug.
pub fn age_str(now: i64, then: i64) -> String {
    // A leg that has never fetched successfully carries 0, and "20677d ago" is a
    // worse lie than a missing number. ui.sh has no equivalent because it only
    // ever formatted the top-level timestamp of a cache that had, by definition,
    // been written at least once.
    if then <= 0 {
        return "never".to_string();
    }
    let d = now - then;
    if d < 60 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

/// Is this an item this build knows how to render?
///
/// An item whose `kind` is neither `pr` nor `jira` is dropped everywhere: the
/// row renderer would draw it in PR format and the actions would treat its URL
/// as a PR's. A future kind is a thing to ignore, not to guess at.
///
/// **This is the single definition of "an item".** It has to be, or the counts
/// disagree with the rows: `--dump` and the header would say 2 while one row is
/// drawn. Every count and every item list in this module goes through it, and so
/// does `App::section_items`.
pub fn kind_is_known(it: &Item) -> bool {
    matches!(it.kind, Kind::Pr | Kind::Jira)
}

impl Cache {
    /// Every item this build can render, in cache order.
    pub fn known(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| kind_is_known(i))
    }

    /// Items of one section, in cache order. The collector already sorts.
    pub fn items_in(&self, section: &Section) -> Vec<&Item> {
        self.known().filter(|i| &i.section == section).collect()
    }

    pub fn count_in(&self, section: &Section) -> usize {
        self.known().filter(|i| &i.section == section).count()
    }

    /// One board's columns, each with its items in cache order. Every item of
    /// the board's section appears in exactly one column.
    pub fn board_columns<'a>(&'a self, board: Board) -> Vec<(Column, Vec<&'a Item>)> {
        let mut out: Vec<(Column, Vec<&Item>)> =
            board.columns().iter().map(|c| (*c, Vec::new())).collect();
        let section = board.section();
        for it in self.known().filter(|i| i.section == section) {
            let col = board.column_of(it);
            if let Some(slot) = out.iter_mut().find(|(c, _)| *c == col) {
                slot.1.push(it);
            }
        }
        out
    }

    pub fn source_status(&self, name: &str) -> &SourceStatus {
        match name {
            "jira" => &self.sources.jira,
            _ => &self.sources.github,
        }
    }

    /// Staleness for the leg behind a section. `item_count` is the section's
    /// count, not the leg's -- the github leg fills two sections and each gets
    /// its own banner.
    pub fn source_state(&self, section: &Section) -> SourceState {
        let name = section.source();
        let s = self.source_status(name);
        SourceState {
            name: if name == "jira" { "jira" } else { "github" },
            ok: s.ok,
            note: s.note.clone(),
            fetched_unix: s.fetched_unix.unwrap_or(self.fetched_unix),
            item_count: self.count_in(section),
        }
    }

    /// Overall age line for the header, e.g. `updated 4m ago`.
    pub fn age(&self, now: i64) -> String {
        age_str(now, self.fetched_unix)
    }

    /// The counts phase 1 puts in the header, in its order: jira / review / mine.
    pub fn header_counts(&self) -> (usize, usize, usize) {
        (
            self.count_in(&Section::Jira),
            self.count_in(&Section::Review),
            self.count_in(&Section::Mine),
        )
    }
}

// ---------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(section: &str, draft: bool, rd: Option<&str>, checks: Option<&str>) -> Item {
        Item {
            kind: Kind::Pr,
            section: Section::from(section.to_string()),
            r#ref: "acme/api#12".into(),
            url: "https://example.invalid/12".into(),
            title: "Fix the thing".into(),
            updated: "2026-08-11T04:12:33Z".into(),
            author: "someone".into(),
            draft,
            review_decision: rd.map(|s| ReviewDecision::from(s.to_string())),
            checks: checks.map(|s| Checks::from(s.to_string())),
            ..Default::default()
        }
    }

    fn jira(cat: Option<&str>, status: &str) -> Item {
        Item {
            kind: Kind::Jira,
            section: Section::Jira,
            r#ref: "ACME-123".into(),
            key: "ACME-123".into(),
            title: "Ship it".into(),
            updated: "2026-08-11T04:12:33Z".into(),
            status: status.into(),
            status_category: cat.map(|s| StatusCategory::from(s.to_string())),
            r#type: "Task".into(),
            priority: "Medium".into(),
            ..Default::default()
        }
    }

    // ---- parsing --------------------------------------------------------

    #[test]
    fn parses_phase1_cache_without_per_leg_timestamps() {
        let c: Cache = serde_json::from_str(
            r#"{"version":1,"fetched_unix":1786541389,
                "sources":{"github":{"ok":true,"note":""},"jira":{"ok":true,"note":""}},
                "items":[]}"#,
        )
        .unwrap();
        assert_eq!(c.fetched_unix, 1786541389);
        assert_eq!(c.sources.github.fetched_unix, None);
        // falls back to the top-level value
        assert_eq!(c.source_state(&Section::Review).fetched_unix, 1786541389);
    }

    #[test]
    fn parses_phase2_per_leg_timestamps() {
        let c: Cache = serde_json::from_str(
            r#"{"version":1,"fetched_unix":200,
                "sources":{"github":{"ok":true,"note":"","fetched_unix":200},
                           "jira":{"ok":false,"note":"jira: unreachable","fetched_unix":100}},
                "items":[]}"#,
        )
        .unwrap();
        assert_eq!(c.sources.jira.fetched_unix, Some(100));
        assert_eq!(c.source_state(&Section::Jira).fetched_unix, 100);
    }

    #[test]
    fn tolerates_null_missing_and_unknown() {
        // sources null, items missing, unknown top-level key, unknown enum
        // values, a fractional timestamp, and a null string field.
        let c: Cache = serde_json::from_str(
            r#"{"version":1,"fetched_unix":1786541389.75,"sources":null,"whats_this":[1,2],
                "items":null}"#,
        )
        .unwrap();
        assert_eq!(c.fetched_unix, 1786541389);
        assert!(c.items.is_empty());
        assert!(!c.sources.jira.ok);

        let it: Item = serde_json::from_str(
            r#"{"kind":"pr","section":"review","ref":"a/b#1","title":null,
                "review_decision":"SOMETHING_NEW","checks":"NEUTRAL","author":null,
                "future_field":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(it.title, "");
        assert_eq!(
            it.review_decision,
            Some(ReviewDecision::Other("SOMETHING_NEW".into()))
        );
        assert_eq!(it.checks, Some(Checks::Other("NEUTRAL".into())));
        assert!(!it.checks.as_ref().unwrap().is_failed());
    }

    #[test]
    fn unparseable_json_is_an_error_not_a_panic() {
        assert!(serde_json::from_str::<Cache>("{ not json").is_err());
    }

    // ---- status glyphs ---------------------------------------------------

    #[test]
    fn glyph_precedence_puts_the_actionable_state_first() {
        // CHANGES_REQUESTED outranks draft: it is the one somebody is waiting
        // on you for.
        let it = pr("mine", true, Some("CHANGES_REQUESTED"), Some("FAILURE"));
        assert_eq!(it.status_glyphs(), (G_CHANGES_REQUESTED, G_CI_FAILED));

        // draft outranks APPROVED: an approved draft still cannot merge.
        let it = pr("mine", true, Some("APPROVED"), None);
        assert_eq!(it.status_glyphs(), (G_DRAFT, G_NONE));

        let it = pr("review", false, Some("APPROVED"), Some("EXPECTED"));
        assert_eq!(it.status_glyphs(), (G_APPROVED, G_CI_PENDING));

        // nothing decided yet, CI green: waiting on a reviewer, quiet slot 2
        let it = pr("review", false, Some("REVIEW_REQUIRED"), Some("SUCCESS"));
        assert_eq!(it.status_glyphs(), (G_NEEDS_REVIEW, G_NONE));

        // ERROR reads as failed, PENDING as pending
        assert_eq!(pr("mine", false, None, Some("ERROR")).status_glyphs().1, G_CI_FAILED);
        assert_eq!(pr("mine", false, None, Some("PENDING")).status_glyphs().1, G_CI_PENDING);

        // Jira keys on the category, and has no CI axis at all
        assert_eq!(jira(Some("To Do"), "To Do").status_glyphs(), (G_TODO, G_NONE));
        assert_eq!(
            jira(Some("In Progress"), "進行中").status_glyphs(),
            (G_IN_PROGRESS, G_NONE)
        );
        // an unknown category is blank rather than a wrong guess
        assert_eq!(jira(Some("Blocked"), "止まってる").status_glyphs(), (G_NONE, G_NONE));
    }

    /// The load-bearing assumption of the whole status column: every glyph is
    /// exactly two display columns. A narrow one would shear every row under it,
    /// and the shear would only show up on the machine whose font disagreed.
    #[test]
    fn glyphs_are_two_columns_wide() {
        for g in [
            G_CHANGES_REQUESTED,
            G_DRAFT,
            G_APPROVED,
            G_NEEDS_REVIEW,
            G_TODO,
            G_IN_PROGRESS,
            G_DONE,
            G_CI_FAILED,
            G_CI_PENDING,
            G_NONE,
        ] {
            assert_eq!(crate::view::width(g), 2, "{g:?} is not two columns wide");
            // No variation selector: VS16 makes the width depend on the font
            // rather than on the code point, which is what we are avoiding.
            assert!(
                !g.chars().any(|c| c == '\u{fe0e}' || c == '\u{fe0f}'),
                "{g:?} carries a variation selector"
            );
        }
    }

    /// Every combination of slots leads with the same prefix width, so the refs
    /// after them line up whatever state the rows are in.
    #[test]
    fn every_row_has_the_same_status_prefix_width() {
        let rows = [
            pr("mine", true, Some("CHANGES_REQUESTED"), Some("FAILURE")),
            pr("mine", false, Some("APPROVED"), Some("PENDING")),
            pr("review", false, None, None),
            jira(Some("To Do"), "To Do"),
            jira(Some("In Progress"), "進行中"),
        ];
        // Measured up to where the ref begins, which is the thing that has to
        // line up. Slicing by character count would not work: an emoji slot is
        // one char and a blank slot is two spaces.
        let widths: Vec<usize> = rows
            .iter()
            .map(|it| {
                let r = it.list_row();
                let key = if it.r#ref.is_empty() { &it.key } else { &it.r#ref };
                let at = r.find(key.as_str()).expect("row contains its ref");
                crate::view::width(&r[..at])
            })
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "prefix widths differ: {widths:?}"
        );
    }

    #[test]
    fn rows_lead_with_the_status_glyphs() {
        // A draft that is also approved: draft wins slot 1, because a draft is
        // the thing stopping it, and CI is quiet so slot 2 is blank.
        let it = pr("review", true, Some("APPROVED"), None);
        assert_eq!(
            it.list_row(),
            "📝     acme/api#12                    Fix the thing  (@someone, 08-11)"
        );
        // The Jira badge is padded in DISPLAY columns: `[進行中]` is 8 wide, so
        // it is followed by 5 spaces, not by 13 - 5 chars worth.
        let it = jira(Some("In Progress"), "進行中");
        assert_eq!(
            it.list_row(),
            "🔵     ACME-123                       [進行中]      Ship it  (Task, Medium, 08-11)"
        );
        // a missing author renders as ghost, like the jq default
        let mut it = pr("mine", false, None, None);
        it.author = String::new();
        assert!(it.list_row().contains("(@ghost, 08-11)"));
        // a malformed timestamp must not panic or slice mid-character
        let mut it = pr("mine", false, None, None);
        it.updated = "２".into();
        assert_eq!(it.day(), "");
    }

    // ---- bucketing ------------------------------------------------------

    #[test]
    fn board_review_buckets_by_decision() {
        let c = Cache {
            items: vec![
                pr("review", false, None, None),
                pr("review", false, Some("REVIEW_REQUIRED"), None),
                pr("review", false, Some("WHO_KNOWS"), None),
                pr("review", false, Some("CHANGES_REQUESTED"), None),
                pr("review", false, Some("APPROVED"), None),
                pr("review", false, Some("APPROVED"), None),
            ],
            ..Default::default()
        };
        let cols = c.board_columns(Board::Review);
        assert_eq!(cols[0].0, Column::NeedsReview);
        assert_eq!(cols[0].1.len(), 3); // null + REVIEW_REQUIRED + unknown
        assert_eq!(cols[1].1.len(), 1);
        assert_eq!(cols[2].1.len(), 2);
        assert_total(&c, Board::Review);
    }

    #[test]
    fn board_mine_precedence_is_ci_then_draft_then_approved() {
        // draft + FAILURE -> CI FAILED (CI wins over draft)
        assert_eq!(
            Board::Mine.column_of(&pr("mine", true, None, Some("FAILURE"))),
            Column::CiFailed
        );
        // draft + APPROVED -> DRAFT (draft wins over approved)
        assert_eq!(
            Board::Mine.column_of(&pr("mine", true, Some("APPROVED"), Some("SUCCESS"))),
            Column::Draft
        );
        // non-draft CHANGES_REQUESTED has no column of its own -> IN REVIEW
        assert_eq!(
            Board::Mine.column_of(&pr("mine", false, Some("CHANGES_REQUESTED"), None)),
            Column::InReview
        );
        // an unknown decision also lands in the fallback
        assert_eq!(
            Board::Mine.column_of(&pr("mine", false, Some("NEW_STATE"), None)),
            Column::InReview
        );
        assert_eq!(
            Board::Mine.column_of(&pr("mine", false, Some("APPROVED"), Some("SUCCESS"))),
            Column::Approved
        );
        // ERROR counts as a failed run
        assert_eq!(
            Board::Mine.column_of(&pr("mine", false, Some("APPROVED"), Some("ERROR"))),
            Column::CiFailed
        );
    }

    #[test]
    fn board_jira_buckets_by_category_not_status() {
        let c = Cache {
            items: vec![
                jira(Some("To Do"), "To Do"),
                jira(Some("In Progress"), "進行中"),
                jira(Some("In Progress"), "レビュー中"),
                jira(Some("Done"), "完了"),
                jira(None, "???"),
                jira(Some("Blocked"), "ブロック"),
            ],
            ..Default::default()
        };
        let cols = c.board_columns(Board::Jira);
        assert_eq!(cols[0].0, Column::ToDo);
        assert_eq!(cols[0].1.len(), 3); // To Do + missing + unknown category
        assert_eq!(cols[1].1.len(), 2); // both Japanese in-progress statuses
        assert_eq!(cols[2].1.len(), 1);
        assert_total(&c, Board::Jira);
    }

    /// The invariant that matters most: bucketing is total, so no card is ever
    /// dropped off a board.
    fn assert_total(c: &Cache, b: Board) {
        let sum: usize = c.board_columns(b).iter().map(|(_, v)| v.len()).sum();
        assert_eq!(sum, c.count_in(&b.section()), "cards vanished from {b:?}");
    }

    #[test]
    fn every_board_is_total_over_a_mixed_cache() {
        let c = Cache {
            items: vec![
                pr("review", false, None, Some("PENDING")),
                pr("review", true, Some("CHANGES_REQUESTED"), Some("FAILURE")),
                pr("mine", true, Some("APPROVED"), Some("SUCCESS")),
                pr("mine", false, Some("NOPE"), None),
                pr("mine", true, None, Some("ERROR")),
                jira(Some("Done"), "完了"),
                jira(None, "?"),
                pr("somewhere-else", false, None, None),
            ],
            ..Default::default()
        };
        for b in BOARDS {
            assert_total(&c, b);
        }
        // the stray section is invisible to every board and to every list section
        assert_eq!(c.count_in(&Section::Other("somewhere-else".into())), 1);
        assert_eq!(c.count_in(&Section::Review) + c.count_in(&Section::Mine) + c.count_in(&Section::Jira), 7);
    }

    // ---- staleness ------------------------------------------------------

    #[test]
    fn age_reads_like_phase1() {
        assert_eq!(age_str(100, 100), "just now");
        assert_eq!(age_str(100, 159), "just now"); // clock went backwards
        assert_eq!(age_str(1059, 1000), "just now");
        assert_eq!(age_str(1060, 1000), "1m ago");
        assert_eq!(age_str(1000 + 3600, 1000), "1h ago");
        assert_eq!(age_str(1000 + 86400 * 3, 1000), "3d ago");
        // a leg that never succeeded, e.g. the first ever run with jira down
        assert_eq!(age_str(1786542189, 0), "never");
    }

    #[test]
    fn banner_distinguishes_stale_from_empty() {
        let mut c = Cache {
            fetched_unix: 1000,
            items: vec![jira(Some("To Do"), "To Do")],
            ..Default::default()
        };
        c.sources.jira = SourceStatus {
            ok: false,
            note: "jira: unreachable after 3 tries (curl exit 56)".into(),
            fetched_unix: Some(1000),
        };
        // retained items -> stale banner with the leg's own age
        assert_eq!(
            c.source_state(&Section::Jira).banner(1000 + 60 * 41).unwrap(),
            "jira: stale, 41m ago — jira: unreachable after 3 tries (curl exit 56)"
        );

        // no items -> just the note
        c.items.clear();
        assert_eq!(
            c.source_state(&Section::Jira).banner(2000).unwrap(),
            "jira: unreachable after 3 tries (curl exit 56)"
        );

        // a failed leg with no note still says something
        c.sources.jira.note = "".into();
        assert_eq!(
            c.source_state(&Section::Jira).banner(2000).unwrap(),
            "jira: failed for an unknown reason"
        );

        // ok:true with a note is NOT a banner -- the note is a warning the view
        // shows separately, and hiding it behind .ok would drop the one
        // diagnostic that must never be silently lost.
        c.sources.github = SourceStatus {
            ok: true,
            note: "github: credential file is group-readable".into(),
            fetched_unix: Some(1000),
        };
        assert_eq!(c.source_state(&Section::Review).banner(2000), None);
        assert!(!c.source_state(&Section::Review).note.is_empty());
    }

    #[test]
    fn search_is_case_insensitive_over_ref_and_title() {
        let it = pr("mine", false, None, None);
        assert!(it.matches("acme/api"));
        assert!(it.matches("FIX THE".to_lowercase().as_str()));
        assert!(it.matches("")); // empty query matches everything
        assert!(!it.matches("nope"));
    }

    #[test]
    fn truncation_and_padding_are_character_safe() {
        assert_eq!(truncate("進行中です", 3), "進行…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abc", 0), "");
    }

    /// Padding is in display columns now, not characters -- the whole reason a
    /// Japanese status no longer shears the column beside it.
    #[test]
    fn padding_counts_display_columns() {
        assert_eq!(pad_cols("ab", 5), "ab   ");
        // 進行中 is 6 columns, so 5 is a truncation, not a pad
        assert_eq!(crate::view::width(&pad_cols("進行中", 8)), 8);
        assert_eq!(crate::view::width(&pad_cols("進行中", 6)), 6);
        // a char-counting pad would have made this 3 columns short
        assert_eq!(crate::view::width(&pad_cols("[進行中]", STATUS_BADGE_COLS)), STATUS_BADGE_COLS);
        assert_eq!(pad_cols("abcdef", 0), "");
    }
}

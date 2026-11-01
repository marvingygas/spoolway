//! The searchable, scrolling list a popup picks one entry from.
//!
//! The overrides tab's promote picker was the first of these, and every list
//! a screen offers to pick from draws the same way: a `search:` row `f`
//! types into, at most [`PICKER_ROWS`] entries at once, and a row above and
//! below the list counting what is out of sight. One module draws them all,
//! so the lists cannot drift apart in how they scroll, search or count.
//!
//! A list that names `origin`'s branches can fetch `origin` as it opens.
//! The fetch runs on a thread of its own, through [`crate::mux::git_bounded`]
//! with its deadline and with git's credentials prompt off, so the popup
//! opens at once on a `fetching origin…` row and keeps reading keys: `esc`
//! closes it while the fetch runs. When the fetch ends the list is read
//! again. A fetch that fails leaves the list as the last fetch saw it,
//! under rows giving git's reason.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use anyhow::Result;

use crate::mux::{GitCapture, GitWithin};
use crate::screen::{Key, Wake, keys, pad_to};

/// How many entries the picker shows at once at most. A list longer than
/// that scrolls, with a row above and below it counting what is out of
/// sight.
const PICKER_ROWS: usize = 7;

/// How long the fetch on open may take before it is stopped. It fetches
/// every branch of `origin` rather than one ref, so it gets longer than the
/// dispatcher's fetch of one base, and nothing waits on it but this popup's
/// list.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// One entry the picker offers.
pub(crate) struct Entry<T> {
    /// What the row names, and what the search matches.
    pub(crate) label: String,
    /// A second column, drawn at [`Look::column`].
    pub(crate) column: Option<String>,
    /// What picking the row hands back.
    pub(crate) value: T,
}

/// How one picker is drawn: the parts each list words its own way.
pub(crate) struct Look {
    /// The columns of text between the popup's side margins.
    pub(crate) width: usize,
    /// The column a row's second column starts at, counted from the cursor
    /// mark. A longer label pushes only its own second column along, one
    /// space past its end.
    pub(crate) column: usize,
    /// The key row, in the popup and on the line under the frame.
    pub(crate) keys: &'static [(&'static str, &'static str)],
    /// What one entry and several are called in the rows counting what is
    /// out of sight, such as `branch` and `branches`.
    pub(crate) noun: (&'static str, &'static str),
    /// The row an empty list shows when nothing is searched.
    pub(crate) empty: &'static str,
}

/// Where the fetch on open stands.
enum Fetch<T> {
    /// None was started, since none was asked for or the checkout has no
    /// `origin`, or it succeeded and the list was read again.
    Off,
    /// Running on its own thread, which answers on `answer` and then
    /// signals `wake`.
    Running {
        answer: Receiver<Fetched<T>>,
        wake: Option<Arc<Wake>>,
    },
    /// It failed, for git's reason.
    Failed(String),
    /// It succeeded, but the list could not be read again after it, for
    /// this reason, so the list is still the one from before the fetch.
    Unread(String),
}

/// What the fetch's thread answers: why the fetch failed, if it did, and
/// the list read again after it, or why that read failed.
struct Fetched<T> {
    failure: Option<String>,
    entries: std::result::Result<Vec<Entry<T>>, String>,
}

/// A picker over a list of entries.
pub(crate) struct Picker<T> {
    title: String,
    entries: Vec<Entry<T>>,
    look: Look,
    filter: String,
    /// Whether `f` was pressed and the query is being typed.
    pub(crate) searching: bool,
    /// Which of [`Picker::matches`] the cursor is on.
    cursor: usize,
    /// The first match drawn, kept between keys so the list scrolls only
    /// when the cursor would leave it.
    offset: usize,
    fetch: Fetch<T>,
}

/// What a key over the picker leads to.
pub(crate) enum Picked<T> {
    /// Boxed, since a picker is many times the size of anything else here.
    Open(Box<Picker<T>>),
    Closed,
    /// `enter` on an entry: its value.
    Chosen(T),
}

impl<T: Clone + Send + 'static> Picker<T> {
    pub(crate) fn new(title: impl Into<String>, entries: Vec<Entry<T>>, look: Look) -> Picker<T> {
        Picker {
            title: title.into(),
            entries,
            look,
            filter: String::new(),
            searching: false,
            cursor: 0,
            offset: 0,
            fetch: Fetch::Off,
        }
    }

    /// The picker, fetching `origin` in the checkout at `root` and then
    /// listing what `reload` reads. The entries it was made with stay when
    /// `reload` fails, under a row saying so. A checkout with no `origin`
    /// has nothing to fetch, so its picker lists at once rather than failing
    /// a fetch every time.
    pub(crate) fn fetching_origin(
        mut self,
        root: &Path,
        reload: impl FnOnce() -> Result<Vec<Entry<T>>> + Send + 'static,
    ) -> Picker<T> {
        if crate::repo::run(root, "git", &["remote", "get-url", "origin"]).is_err() {
            return self;
        }
        let (send, answer) = std::sync::mpsc::channel();
        let wake = Wake::new().map(Arc::new);
        let signal = wake.clone();
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            let failure = fetch_origin(&root);
            let entries = reload().map_err(|err| format!("{err:#}"));
            // A picker closed while the fetch ran has dropped its end, so
            // the answer goes nowhere and nothing is signalled.
            if send.send(Fetched { failure, entries }).is_ok()
                && let Some(signal) = signal
            {
                signal.notify();
            }
        });
        self.fetch = Fetch::Running { answer, wake };
        self
    }

    /// What the fetch signals once it has answered, for a screen to wait on
    /// beside its keys. `None` with no fetch running.
    pub(crate) fn wake(&self) -> Option<Arc<Wake>> {
        match &self.fetch {
            Fetch::Running { wake, .. } => wake.clone(),
            _ => None,
        }
    }

    /// Whether the fetch on open is still running.
    #[cfg(test)]
    pub(crate) fn fetching(&self) -> bool {
        matches!(self.fetch, Fetch::Running { .. })
    }

    /// Take the fetch's answer, if it has come: the list read after it, and
    /// why the fetch or the read failed when one did.
    pub(crate) fn settle(&mut self) {
        let Fetch::Running { answer, .. } = &self.fetch else {
            return;
        };
        let fetched = match answer.try_recv() {
            Ok(fetched) => fetched,
            Err(TryRecvError::Empty) => return,
            // The thread ended without answering, which only a panic does.
            Err(TryRecvError::Disconnected) => Fetched {
                failure: Some("the fetch stopped without an answer".to_string()),
                entries: Err("the fetch stopped without an answer".to_string()),
            },
        };
        // A failed fetch is named first: the list from before it is then
        // what the last fetch saw too, so its rows hold whether or not the
        // read after it worked.
        self.fetch = match (fetched.failure, fetched.entries) {
            (Some(reason), entries) => {
                if let Ok(entries) = entries {
                    self.entries = entries;
                }
                Fetch::Failed(reason)
            }
            (None, Ok(entries)) => {
                self.entries = entries;
                Fetch::Off
            }
            (None, Err(reason)) => Fetch::Unread(reason),
        };
        self.cursor = 0;
        self.offset = 0;
    }

    /// The entries the search leaves, in list order. None while the fetch
    /// runs, since the list it would show is about to be read again.
    fn matches(&self) -> Vec<&Entry<T>> {
        if matches!(self.fetch, Fetch::Running { .. }) {
            return Vec::new();
        }
        let query = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| entry.label.to_lowercase().contains(&query))
            .collect()
    }

    /// One key over the picker or its search row.
    pub(crate) fn handle_key(mut self, key: Key) -> Picked<T> {
        // Taken first, so a key typed the moment the fetch ends acts on the
        // list it brought rather than on the waiting row.
        self.settle();
        let len = self.matches().len();
        match key {
            Key::Up | Key::Down => {
                self.cursor = crate::screen::move_cursor(self.cursor, len, key);
            }
            Key::Enter if self.searching => self.searching = false,
            Key::Esc if self.searching => {
                self.filter.clear();
                self.searching = false;
            }
            other if self.searching => crate::screen::edit(&mut self.filter, other),
            Key::Enter => {
                let Some(value) = self.matches().get(self.cursor).map(|e| e.value.clone()) else {
                    return Picked::Open(Box::new(self));
                };
                return Picked::Chosen(value);
            }
            Key::Esc => return Picked::Closed,
            Key::Char('f') => self.searching = true,
            _ => {}
        }
        let len = self.matches().len();
        self.cursor = self.cursor.min(len.saturating_sub(1));
        self.offset = scrolled(self.offset, self.cursor, PICKER_ROWS);
        Picked::Open(Box::new(self))
    }

    /// The popup, its list cut to fit the `rows` the list box has.
    pub(crate) fn popup(&self, rows: Option<usize>) -> Vec<String> {
        let width = self.look.width;
        let mut body = vec![
            pad_to(&format!("search: {}▏", self.filter), width),
            String::new(),
        ];
        if matches!(self.fetch, Fetch::Running { .. }) {
            body.push(pad_to("  fetching origin…", width));
            return crate::screen::panel(&self.title, &body, &keys(self.look.keys));
        }
        let (one, many) = self.look.noun;
        // Indented two columns, so every wrapped row of the reason lines up
        // under the entries' names rather than under their cursor mark. It
        // says branches whatever noun the list uses, since what a fetch
        // brings is branches.
        let failure: Vec<String> = match &self.fetch {
            Fetch::Failed(reason) => crate::screen::wrap(
                &format!(
                    "  fetch failed: {}. showing the last fetch's branches. esc and reopen to \
                     try again.",
                    reason.trim_end_matches('.')
                ),
                width,
            ),
            Fetch::Unread(reason) => crate::screen::wrap(
                &format!(
                    "  branches not read again: {}. showing the branches from before the \
                     fetch. esc and reopen to try again.",
                    reason.trim_end_matches('.')
                ),
                width,
            ),
            _ => Vec::new(),
        };
        if !failure.is_empty() {
            body.extend(failure.iter().cloned());
            body.push(String::new());
        }
        let matches = self.matches();
        if matches.is_empty() {
            body.push(match self.filter.is_empty() {
                true => format!("  {}", self.look.empty),
                false => format!("  no {many} match this search"),
            });
        }
        // The rows the list may take: the box's two borders, less the row
        // `overlay` keeps above a popup and the popup's own borders, search
        // row, blank rows, key row and the two rows counting what is out of
        // sight, and the failure's rows with the blank one under them.
        let fixed = 1 + 8 + failure.len() + usize::from(!failure.is_empty());
        let room = rows.map_or(PICKER_ROWS, |rows| (rows + 2).saturating_sub(fixed));
        let view = PICKER_ROWS.min(room).max(1);
        let cut = matches.len() > view;
        let offset = match cut {
            true => scrolled(self.offset, self.cursor, view).min(matches.len() - view),
            false => 0,
        };
        let shown = &matches[offset..matches.len().min(offset + view)];
        // Spelled here: `plural` adds a bare `s`.
        let marker = |n: usize, arrow: &str, word: &str| match n {
            0 => String::new(),
            1 => format!("  {arrow} 1 {one} {word}"),
            n => format!("  {arrow} {n} {many} {word}"),
        };
        if cut {
            body.push(marker(offset, "↑", "above"));
        }
        for (at, entry) in shown.iter().enumerate() {
            let mark = if offset + at == self.cursor {
                "> "
            } else {
                "  "
            };
            let row = format!("{mark}{}", entry.label);
            let row = match &entry.column {
                Some(column) => format!(
                    "{}{column}",
                    pad_to(&row, self.look.column.max(row.chars().count() + 1))
                ),
                None => row,
            };
            body.push(pad_to(&row, width.max(row.chars().count())));
        }
        if cut {
            body.push(marker(matches.len() - offset - shown.len(), "↓", "below"));
        }
        crate::screen::panel(&self.title, &body, &keys(self.look.keys))
    }
}

/// `offset` moved as little as it takes for a view `view` rows tall to
/// show `cursor`.
fn scrolled(offset: usize, cursor: usize, view: usize) -> usize {
    if cursor < offset {
        cursor
    } else if cursor >= offset + view {
        cursor + 1 - view
    } else {
        offset
    }
}

/// `git fetch origin` in `root`, bounded by [`FETCH_TIMEOUT`] with git's
/// credentials prompt off. `None` when it succeeded, or why it failed, in
/// git's words where git gave any.
fn fetch_origin(root: &Path) -> Option<String> {
    let ran = crate::mux::git_bounded(
        root,
        &["fetch", "origin"],
        &[("GIT_TERMINAL_PROMPT", "0")],
        FETCH_TIMEOUT,
        &GitCapture {
            stdout: false,
            stderr: true,
            at_deadline: &|_| {},
        },
    );
    match ran {
        GitWithin::Finished { success: true, .. } => None,
        GitWithin::Finished { stderr, status, .. } => {
            Some(git_reason(&String::from_utf8_lossy(&stderr)).unwrap_or(status))
        }
        GitWithin::TimedOut => Some(format!(
            "git fetch origin did not finish within {}s and was stopped",
            FETCH_TIMEOUT.as_secs()
        )),
        GitWithin::NotRun => Some("git could not be started".to_string()),
    }
}

/// The reason in a failed fetch's stderr: its first line, without git's
/// `fatal:` or `error:` and without the `unable to access '<url>':` an
/// https remote puts before what went wrong, so an offline fetch reads
/// `Could not resolve host: github.com`. `None` when git printed nothing.
fn git_reason(stderr: &str) -> Option<String> {
    let line = stderr.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line
        .strip_prefix("fatal: ")
        .or_else(|| line.strip_prefix("error: "))
        .unwrap_or(line);
    let line = match line.strip_prefix("unable to access '") {
        Some(rest) => rest.split_once("': ").map_or(line, |(_, reason)| reason),
        None => line,
    };
    Some(line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::shell;

    const LOOK: Look = Look {
        width: 44,
        column: 30,
        keys: &[("enter", "confirm"), ("f", "search"), ("esc", "back")],
        noun: ("pipeline", "pipelines"),
        empty: "no pipeline to pick",
    };

    fn git(dir: &Path, args: &[&str]) -> String {
        crate::repo::run(dir, "git", args).unwrap()
    }

    /// The popup's rows between its borders, the padding taken off.
    fn inside(picker: &Picker<String>) -> Vec<String> {
        let drawn: Vec<String> = picker.popup(None).iter().map(|l| shell::plain(l)).collect();
        drawn[1..drawn.len() - 1]
            .iter()
            .map(|l| {
                l.trim_start_matches('│')
                    .trim_end_matches('│')
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// `keys` pressed in turn, the picker still open after each.
    fn press(picker: Picker<String>, keys: &[Key]) -> Picker<String> {
        keys.iter()
            .fold(picker, |picker, key| match picker.handle_key(*key) {
                Picked::Open(picker) => *picker,
                _ => panic!("the picker closed on {key:?}"),
            })
    }

    fn entries(names: &[&str]) -> Vec<Entry<String>> {
        names
            .iter()
            .map(|name| Entry {
                label: name.to_string(),
                column: None,
                value: name.to_string(),
            })
            .collect()
    }

    /// The fetch's answer, waited for.
    fn settled(mut picker: Picker<String>) -> Picker<String> {
        let start = std::time::Instant::now();
        while picker.fetching() {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "the fetch never answered"
            );
            std::thread::sleep(Duration::from_millis(10));
            picker.settle();
        }
        picker
    }

    /// A checkout `proj` on `main` whose `origin` is the bare `origin.git`
    /// beside it, pushed to once, and the scratch folder holding both.
    fn with_origin(name: &str) -> (std::path::PathBuf, crate::scratch::ScratchRoot) {
        let scratch = crate::scratch::root(&format!("picker-{name}"));
        let root = scratch.join("proj");
        let origin = scratch.join("origin.git");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&origin).unwrap();
        crate::scratch::git_init(&origin, &["--bare", "-b", "main"]);
        crate::scratch::git_init(&root, &["-b", "main"]);
        git(&root, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(&root, &["remote", "add", "origin", "../origin.git"]);
        git(&root, &["push", "-q", "origin", "main"]);
        (root, scratch)
    }

    /// The branches `origin` had at the last fetch, as entries.
    fn remote_branches(root: &Path) -> Result<Vec<Entry<String>>> {
        let listing = crate::repo::run(
            root,
            "git",
            &[
                "for-each-ref",
                "--format=%(refname:lstrip=3)",
                "refs/remotes/origin",
            ],
        )?;
        Ok(listing
            .lines()
            .filter(|name| *name != "HEAD")
            .map(|name| Entry {
                label: name.to_string(),
                column: None,
                value: name.to_string(),
            })
            .collect())
    }

    // A second column starts at the look's column, or one space past a
    // longer label, and the rows counting what is out of sight use the
    // look's noun, one and several.
    #[test]
    fn rows_draw_a_second_column_and_count_in_the_looks_noun() {
        let mut list = entries(&["alpha", "a-label-long-enough-to-pass-it"]);
        list[0].column = Some("first".to_string());
        list[1].column = Some("second".to_string());
        for n in 0..7 {
            list.extend(entries(&[&format!("p{n}")]));
        }
        let picker = Picker::new("pipeline for wire", list, LOOK);
        let rows = inside(&picker);
        assert_eq!(rows[3], "  > alpha                       first");
        assert_eq!(rows[4], "    a-label-long-enough-to-pass-it second");
        assert_eq!(rows[10], "    ↓ 2 pipelines below");
        let picker = press(picker, &[Key::Down; 7]);
        let rows = inside(&picker);
        assert_eq!(rows[2], "    ↑ 1 pipeline above");
        assert_eq!(rows[10], "    ↓ 1 pipeline below");
        assert_eq!(
            rows[rows.len() - 1],
            "  [enter] confirm   [f] search   [esc] back"
        );
    }

    // With a fetch, the popup opens on the waiting row and lists nothing,
    // so `enter` picks nothing. Once the fetch answers it lists what the
    // reload read, a branch only the fetch brought included.
    #[test]
    fn a_fetch_waits_on_its_row_then_lists_the_fetched_branches() {
        let (root, _guard) = with_origin("fetched");
        git(&root, &["push", "-q", "origin", "main:feature/new"]);
        git(
            &root,
            &["update-ref", "-d", "refs/remotes/origin/feature/new"],
        );
        let reload = root.clone();
        let picker = Picker::new("lands in for wire", entries(&["main"]), LOOK)
            .fetching_origin(&root, move || remote_branches(&reload));
        assert_eq!(
            inside(&picker),
            [
                "  search: ▏".to_string(),
                String::new(),
                "    fetching origin…".to_string(),
                String::new(),
                "  [enter] confirm   [f] search   [esc] back".to_string(),
            ]
        );
        let picker = match picker.handle_key(Key::Enter) {
            Picked::Open(picker) => *picker,
            _ => panic!("enter picked from the waiting row"),
        };
        let picker = settled(picker);
        let rows = inside(&picker);
        assert_eq!(
            rows[2..4],
            ["  > feature/new".to_string(), "    main".to_string()]
        );
    }

    // A failed fetch keeps the list the reload read from the last fetch,
    // under the rows giving git's reason, wrapped to the popup's text.
    #[test]
    fn a_failed_fetch_lists_the_last_fetchs_branches_under_why() {
        let (root, _guard) = with_origin("failed");
        git(&root, &["fetch", "-q", "origin"]);
        git(&root, &["remote", "set-url", "origin", "../nowhere.git"]);
        let reload = root.clone();
        let picker = settled(
            Picker::new("lands in for wire", Vec::new(), LOOK)
                .fetching_origin(&root, move || remote_branches(&reload)),
        );
        let rows = inside(&picker);
        let blank = rows.iter().skip(2).position(String::is_empty).unwrap() + 2;
        let said: Vec<&str> = rows[2..blank].iter().map(|l| l.trim()).collect();
        // The popup's own margin, then the reason's indent.
        assert!(rows[2].starts_with("    fetch failed: '../nowhere.git' does not"));
        assert!(rows[2..blank].iter().all(|l| l.starts_with("    ")));
        assert!(rows[2..blank].iter().all(|l| l.chars().count() <= 2 + 44));
        assert!(
            said.join(" ")
                .ends_with("showing the last fetch's branches. esc and reopen to try again."),
            "{said:?}"
        );
        assert_eq!(rows[blank + 1], "  > main");
    }

    // A failed fetch's rows, drawn over three branches as the lands-in
    // picker draws them offline.
    #[test]
    fn a_failed_fetch_draws_its_reason_over_the_list() {
        let mut picker = Picker::new(
            "lands in for wire",
            entries(&[
                "task/gh-1397-release-spoolway-12",
                "task/gh-1425-focused-pane-brackets",
                "task/gh-1427-numbered-tabs-strip",
            ]),
            LOOK,
        );
        picker.fetch = Fetch::Failed("Could not resolve host: github.com".to_string());
        let drawn: Vec<String> = picker.popup(None).iter().map(|l| shell::plain(l)).collect();
        assert_eq!(
            drawn,
            [
                "╭─ lands in for wire ────────────────────────────╮",
                "│  search: ▏                                     │",
                "│                                                │",
                "│    fetch failed: Could not resolve host:       │",
                "│    github.com. showing the last fetch's        │",
                "│    branches. esc and reopen to try again.      │",
                "│                                                │",
                "│  > task/gh-1397-release-spoolway-12            │",
                "│    task/gh-1425-focused-pane-brackets          │",
                "│    task/gh-1427-numbered-tabs-strip            │",
                "│                                                │",
                "│  [enter] confirm   [f] search   [esc] back     │",
                "╰────────────────────────────────────────────────╯",
            ]
        );
    }

    // A fetch that succeeds with a read after it that fails keeps the list
    // from before the fetch, under rows saying the list was not read again
    // and why.
    #[test]
    fn a_failed_read_after_the_fetch_says_the_list_is_from_before_it() {
        let (root, _guard) = with_origin("unread");
        let picker = settled(
            Picker::new("lands in for wire", entries(&["main"]), LOOK)
                .fetching_origin(&root, || anyhow::bail!("git for-each-ref broke")),
        );
        let rows = inside(&picker);
        let blank = rows.iter().skip(2).position(String::is_empty).unwrap() + 2;
        let said: Vec<&str> = rows[2..blank].iter().map(|l| l.trim()).collect();
        assert_eq!(
            said.join(" "),
            "branches not read again: git for-each-ref broke. showing the branches from \
             before the fetch. esc and reopen to try again."
        );
        assert_eq!(rows[blank + 1], "  > main");
    }

    // `esc` closes the picker while the fetch still runs, and the fetch's
    // answer, when it comes, goes nowhere.
    #[test]
    fn esc_closes_the_picker_while_the_fetch_runs() {
        let (root, _guard) = with_origin("esc");
        let reload = root.clone();
        let picker = Picker::new("lands in for wire", Vec::new(), LOOK)
            .fetching_origin(&root, move || remote_branches(&reload));
        assert!(picker.fetching());
        assert!(matches!(picker.handle_key(Key::Esc), Picked::Closed));
    }

    // A checkout with no `origin` lists at once, with no fetch to fail.
    #[test]
    fn a_checkout_with_no_origin_lists_without_fetching() {
        let scratch = crate::scratch::root("picker-no-origin");
        std::fs::create_dir_all(&scratch).unwrap();
        crate::scratch::git_init(&scratch, &["-b", "main"]);
        let picker = Picker::new("lands in for wire", entries(&["main"]), LOOK)
            .fetching_origin(&scratch, || panic!("nothing to reload"));
        assert!(!picker.fetching());
        assert_eq!(inside(&picker)[2], "  > main");
    }

    // An https remote's reason is what went wrong, without the url before
    // it, and a reason over several lines is its first one.
    #[test]
    fn the_reason_is_gits_first_line_without_its_prefixes() {
        assert_eq!(
            git_reason(
                "fatal: unable to access 'https://github.com/a/b.git/': Could not resolve \
                 host: github.com\n"
            )
            .as_deref(),
            Some("Could not resolve host: github.com")
        );
        assert_eq!(
            git_reason("\nssh: connect to host x port 22: refused\nfatal: Could not read\n")
                .as_deref(),
            Some("ssh: connect to host x port 22: refused")
        );
        assert_eq!(git_reason("  \n"), None);
    }
}

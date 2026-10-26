//! `promote selected` in the active section: whole files from the layer,
//! written into the tracked setup of a branch the person picks.
//!
//! A promote changes the project's setup, and that change usually goes
//! through a feature branch and a pull request. The board runs in the main
//! checkout, so a write there would land on whatever branch it has out.
//! The picker lists every local branch and every branch `origin` had at the
//! last fetch, and the files go into whichever checkout holds the picked
//! one. When none does, a worktree is made for it beside the main checkout
//! first, with a local branch tracking `origin`'s for one only the remote
//! has. Nothing is committed, and the layer keeps every promoted entry, as
//! `override promote` keeps them.
//!
//! The write itself is `override promote`'s, file by file, into the folder
//! [`Repo::tracked_write_root`] answers for that checkout, so the tab
//! refuses what the command run there would refuse, in the same words.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::edit;
use crate::config::Config;
use crate::pipeline::Pipelines;
use crate::repo::Repo;
use crate::screen::{Key, Notice, keys, pad_to, plural};

/// One branch the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Branch {
    pub(super) name: String,
    /// Whether only `origin` has it, so promoting onto it first makes a
    /// local branch tracking `origin`'s.
    pub(super) remote_only: bool,
}

/// One file a promote writes, with what the confirm popup lists under it.
#[derive(Debug, Clone)]
pub(super) struct Promoted {
    /// The file as `override list` prints it, which is also what `override
    /// promote` takes.
    pub(super) target: String,
    /// Each key with the layer's value, as the active section spells it.
    /// Empty for a prompt, which is promoted as a whole file.
    pub(super) keys: Vec<(String, String)>,
}

/// Every branch the picker offers: the local ones and `origin`'s, in one
/// list sorted by name, each named once.
///
/// Task branches are left out. Lanes push many of them, and the checkout of
/// one is a lane's, which [`Repo::tracked_write_root`] refuses. `origin`'s
/// branches are the ones the last fetch saw; nothing here fetches.
pub(super) fn branches(repo: &Repo) -> Result<Vec<Branch>> {
    let listing = repo.git(&[
        "for-each-ref",
        "--format=%(refname)",
        "refs/heads",
        "refs/remotes/origin",
    ])?;
    // Keyed by name, so a branch both sides have is listed once, as local.
    let mut found: BTreeMap<String, bool> = BTreeMap::new();
    for line in listing.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("refs/heads/") {
            found.insert(name.to_string(), false);
        } else if let Some(name) = line.strip_prefix("refs/remotes/origin/")
            // `origin/HEAD` names the remote's default branch, which is
            // listed under its own name already.
            && name != "HEAD"
        {
            found.entry(name.to_string()).or_insert(true);
        }
    }
    Ok(found
        .into_iter()
        .filter(|(name, _)| !name.starts_with("task/"))
        .map(|(name, remote_only)| Branch { name, remote_only })
        .collect())
}

/// The text inside both popups, so each is 60 columns wide inside its
/// border, as the mockups draw them.
const PROMOTE_TEXT: usize = 54;

/// The column `origin` starts at on a remote-only branch's row, counted
/// from the cursor mark. A longer name pushes only its own `origin` along,
/// one space past its end.
const BRANCH_FIELD: usize = 46;

/// How many branches the picker shows at once at most. A list longer than
/// that scrolls, with a row above and below it counting what is out of
/// sight.
const PICKER_ROWS: usize = 7;

/// The picker's keys, in the popup and on the line under the frame.
pub(super) const PICK_KEYS: &[(&str, &str)] = &[("enter", "pick"), ("f", "search"), ("esc", "close")];

/// The branch picker `promote selected` opens.
pub(super) struct Picker {
    files: Vec<Promoted>,
    branches: Vec<Branch>,
    filter: String,
    /// Whether `f` was pressed and the query is being typed.
    pub(super) searching: bool,
    /// Which of [`Picker::matches`] the cursor is on.
    cursor: usize,
    /// The first match drawn, kept between keys so the list scrolls only
    /// when the cursor would leave it.
    offset: usize,
}

/// What a key over the picker leads to.
pub(super) enum Picked {
    Open(Picker),
    Closed,
    /// `enter` on a branch: the files and the branch they go to.
    Branch(Vec<Promoted>, Branch),
}

impl Picker {
    pub(super) fn new(files: Vec<Promoted>, branches: Vec<Branch>) -> Picker {
        Picker {
            files,
            branches,
            filter: String::new(),
            searching: false,
            cursor: 0,
            offset: 0,
        }
    }

    /// The branches the search leaves, in name order.
    fn matches(&self) -> Vec<&Branch> {
        let query = self.filter.to_lowercase();
        self.branches
            .iter()
            .filter(|b| b.name.to_lowercase().contains(&query))
            .collect()
    }

    /// One key over the picker or its search row.
    pub(super) fn handle_key(mut self, key: Key) -> Picked {
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
            other if self.searching => edit(&mut self.filter, other),
            Key::Enter => {
                let Some(branch) = self.matches().get(self.cursor).map(|b| (*b).clone()) else {
                    return Picked::Open(self);
                };
                return Picked::Branch(self.files, branch);
            }
            Key::Esc => return Picked::Closed,
            Key::Char('f') => self.searching = true,
            _ => {}
        }
        let len = self.matches().len();
        self.cursor = self.cursor.min(len.saturating_sub(1));
        self.offset = scrolled(self.offset, self.cursor, PICKER_ROWS);
        Picked::Open(self)
    }

    /// The popup, its list cut to fit the `rows` the list box has.
    pub(super) fn popup(&self, rows: Option<usize>) -> Vec<String> {
        let mut body = vec![
            pad_to(&format!("search: {}▏", self.filter), PROMOTE_TEXT),
            String::new(),
        ];
        let matches = self.matches();
        if matches.is_empty() {
            body.push(match self.filter.is_empty() {
                true => "  no branch to promote onto".to_string(),
                false => "  no branches match this search".to_string(),
            });
        }
        // The rows the list may take: the box's two borders, less the row
        // `overlay` keeps above a popup and the popup's own borders, search
        // row, blank rows, key row and the two rows counting what is out of
        // sight.
        let room = rows.map_or(PICKER_ROWS, |rows| (rows + 2).saturating_sub(1 + 8));
        let view = PICKER_ROWS.min(room).max(1);
        let cut = matches.len() > view;
        let offset = match cut {
            true => scrolled(self.offset, self.cursor, view).min(matches.len() - view),
            false => 0,
        };
        let shown = &matches[offset..matches.len().min(offset + view)];
        let marker = |n: usize, arrow: &str, word: &str| match n {
            0 => String::new(),
            n => format!("  {arrow} {} {word}", plural(n, "branch")),
        };
        if cut {
            body.push(marker(offset, "↑", "above"));
        }
        for (at, branch) in shown.iter().enumerate() {
            let mark = if offset + at == self.cursor { "> " } else { "  " };
            let row = format!("{mark}{}", branch.name);
            let row = match branch.remote_only {
                true => format!(
                    "{}origin",
                    pad_to(&row, BRANCH_FIELD.max(row.chars().count() + 1))
                ),
                false => row,
            };
            body.push(pad_to(&row, PROMOTE_TEXT.max(row.chars().count())));
        }
        if cut {
            body.push(marker(matches.len() - offset - shown.len(), "↓", "below"));
        }
        crate::screen::panel(&format!("{} to", title(&self.files)), &body, &keys(PICK_KEYS))
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

/// What both popups' titles name: the one file, or how many.
fn title(files: &[Promoted]) -> String {
    match files {
        [one] => format!("promote {}", one.target),
        _ => format!("promote {}", plural(files.len(), "file")),
    }
}

/// Where a promote writes.
#[derive(Debug, Clone)]
enum Place {
    /// A checkout git lists with the branch out.
    Checkout(PathBuf),
    /// No checkout has it: a worktree is made here first.
    New(PathBuf),
}

/// A promote waiting on its confirm popup.
pub(super) struct Promotion {
    files: Vec<Promoted>,
    branch: Branch,
    place: Place,
    /// The checkout the confirm popup names each file's path from. A new
    /// worktree's files come from the branch, which has the main
    /// checkout's layout as far as this can tell before it exists.
    layout: PathBuf,
}

impl Promotion {
    /// The promote of `files` onto `branch`, written wherever git has it
    /// out, or into a new worktree at [`worktree_path`] when nowhere does.
    pub(super) fn new(repo: &Repo, files: Vec<Promoted>, branch: Branch) -> Result<Promotion> {
        let place = match branch.remote_only {
            true => None,
            false => repo.worktree_for(&branch.name)?,
        };
        let (place, layout) = match place {
            Some(checkout) => (Place::Checkout(checkout.clone()), checkout),
            None => (
                Place::New(worktree_path(&repo.root, &branch.name)),
                repo.root.clone(),
            ),
        };
        Ok(Promotion {
            files,
            branch,
            place,
            layout,
        })
    }

    /// The folder the files go into.
    fn folder(&self) -> &Path {
        match &self.place {
            Place::Checkout(path) | Place::New(path) => path,
        }
    }

    /// The confirm popup: where the files go and on which branch, that
    /// nothing is committed, each key with the value written, and that the
    /// layer keeps them. Cut to the `rows` the list box has.
    pub(super) fn popup(&self, rows: Option<usize>) -> Vec<String> {
        let folder = crate::repo::shorten_home(self.folder());
        let branch = &self.branch.name;
        let layout = self.layout.as_path();
        let what = match self.files.as_slice() {
            [one] => tracked_name(layout, &one.target),
            files => plural(files.len(), "file"),
        };
        let lead = match &self.place {
            Place::Checkout(_) => format!("{branch} is checked out at"),
            Place::New(_) => format!("makes a worktree for {branch} at"),
        };
        let mut body = vec![String::new()];
        for line in [
            lead,
            format!("  {folder}"),
            match &self.place {
                Place::Checkout(_) => format!("and this writes {what} there,"),
                Place::New(_) => format!("and writes {what} there,"),
            },
            "not committed".to_string(),
        ] {
            body.extend(crate::screen::wrap(&line, PROMOTE_TEXT));
        }
        body.push(String::new());

        // One file lists its keys under the lines above, as the mockup draws
        // it. Several each get a heading naming the file, with their keys
        // under it, so every key is still named with the file it lands in.
        let indent = match self.files.len() {
            1 => "  ",
            _ => "    ",
        };
        let label = self
            .files
            .iter()
            .flat_map(|file| &file.keys)
            .map(|(key, _)| key.chars().count() + KEY_GAP)
            .max()
            .unwrap_or(0);
        let blocks: Vec<Vec<String>> = self
            .files
            .iter()
            .map(|file| {
                let mut block = Vec::new();
                if self.files.len() > 1 {
                    block.push(format!("  {}", tracked_name(layout, &file.target)));
                }
                match file.keys.is_empty() {
                    true => block.push(format!("{indent}whole file")),
                    false => block.extend(
                        file.keys
                            .iter()
                            .map(|(key, value)| format!("{indent}{key:label$}{value}")),
                    ),
                }
                block
            })
            .collect();
        // The rows the keys may take, as the delete popup counts them, less
        // the two rows under them: a blank one and the line saying the
        // layer keeps the entries.
        let room = rows.map(|rows| (rows + 2).saturating_sub(1 + 4 + body.len() + 2));
        body.extend(super::pipeline::fit_blocks(blocks, room, 2, "file"));
        body.push(String::new());
        body.push("the overrides stay in the layer until you delete them".to_string());
        let body: Vec<String> = body
            .iter()
            .map(|line| pad_to(line, PROMOTE_TEXT.max(line.chars().count())))
            .collect();
        crate::screen::ask(&title(&self.files), &body, &["promote".to_string()], 0)
    }

    /// Make the worktree when one is needed, then write every file into
    /// the checkout. Answers the notice to show either way.
    ///
    /// Nothing is written unless the worktree was made, and nothing is
    /// written anywhere [`Repo::tracked_write_root`] refuses: a checkout
    /// of a lane, on a detached HEAD, or of a branch with no `.spoolway/`.
    pub(super) fn run(&self, repo: &Repo) -> Notice {
        match self.write(repo) {
            Ok(notice) | Err(notice) => notice,
        }
    }

    fn write(&self, repo: &Repo) -> std::result::Result<Notice, Notice> {
        let refused = |err: anyhow::Error| Notice::new("not promoted", format!("{err:#}"));
        let typed = format!("override promote {}", self.files[0].target);
        if let Place::New(path) = &self.place {
            // A branch with no `.spoolway/` would make a worktree only for
            // the write into it to be refused. Asked of the branch itself,
            // so that refusal comes first and nothing is made.
            if !carries_setup(repo, &self.branch) {
                checkout_repo(repo, path)
                    .tracked_write_root("tracked files", &typed)
                    .map_err(refused)?;
            }
            make_worktree(repo, &self.branch, path).map_err(|err| {
                Notice::new(
                    "not promoted",
                    format!(
                        "no worktree was made for {} at {}, so nothing was written: {err:#}. \
                         fix what stopped it, or check {} out somewhere yourself, and promote \
                         again.",
                        self.branch.name,
                        path.display(),
                        self.branch.name
                    ),
                )
            })?;
        }
        let checkout = self.folder();
        let target = checkout_repo(repo, checkout);
        let write = target
            .tracked_write_root("tracked files", &typed)
            .map_err(refused)?;
        for (at, file) in self.files.iter().enumerate() {
            let Err(err) = crate::commands::promote_override(repo, write.root, &file.target) else {
                continue;
            };
            let names = |files: &[Promoted]| {
                files
                    .iter()
                    .map(|file| file.target.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let (done, left) = self.files.split_at(at);
            return Err(match done.is_empty() {
                true => Notice::new(
                    "not promoted",
                    format!(
                        "nothing was written to {}: {err:#}. fix what stopped it and promote \
                         again.",
                        write.root.display()
                    ),
                ),
                false => Notice::new(
                    "promoted in part",
                    format!(
                        "{} {} written to {}, but not {}: {err:#}. fix what stopped it and \
                         promote what is left again.",
                        names(done),
                        if done.len() == 1 { "was" } else { "were" },
                        write.root.display(),
                        names(left)
                    ),
                ),
            });
        }
        let mut text = format!(
            "wrote {} in {} on {}. nothing is committed, and the overrides stay in the layer \
             until you delete them.",
            self.files
                .iter()
                .map(|file| tracked_name(write.root, &file.target))
                .collect::<Vec<_>>()
                .join(", "),
            crate::repo::shorten_home(write.root),
            self.branch.name
        );
        if let Some(note) = write.note() {
            text.push_str(&format!("\n{}.", note.trim()));
        }
        Ok(Notice::new("promoted", text))
    }
}

/// The columns between a key and its value in the confirm popup.
const KEY_GAP: usize = 3;

/// Where a worktree for `branch` is made when no checkout has it: beside
/// the main checkout `root`, named after it and the branch, with each `/`
/// in the branch as `-` so the name is one folder.
pub(super) fn worktree_path(root: &Path, branch: &str) -> PathBuf {
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let folder = format!("{name}-{}", branch.replace('/', "-"));
    root.parent().unwrap_or(root).join(folder)
}

/// Whether `branch`'s own tree holds a `.spoolway/`, asked of git so no
/// checkout has to exist to find out.
fn carries_setup(repo: &Repo, branch: &Branch) -> bool {
    let start = match branch.remote_only {
        true => format!("origin/{}", branch.name),
        false => branch.name.clone(),
    };
    repo.git(&[
        "cat-file",
        "-e",
        &format!("{start}:{}", crate::config::STATE_DIR),
    ])
    .is_ok()
}

/// `git worktree add` for `branch` at `path`: the local branch, or a new
/// local branch tracking `origin`'s for one only the remote has. Git's own
/// error, naming what stopped it, is the error.
fn make_worktree(repo: &Repo, branch: &Branch, path: &Path) -> Result<()> {
    let Some(at) = path.to_str() else {
        bail!("{} is not a path git can be given", path.display());
    };
    let name = branch.name.as_str();
    let upstream = format!("origin/{name}");
    let args: Vec<&str> = match branch.remote_only {
        true => vec!["worktree", "add", "--track", "-b", name, at, &upstream],
        false => vec!["worktree", "add", at, name],
    };
    repo.git(&args).map(|_| ())
}

/// `repo` as a command run in `checkout` would find it, so
/// [`Repo::tracked_write_root`] decides that checkout's write as it would
/// there. The main checkout is itself. Another one is a worktree, which
/// reads its own setup when it has one, and borrows the main checkout's
/// when it has none, as a worktree not made yet has none.
fn checkout_repo(repo: &Repo, checkout: &Path) -> Repo {
    let main = checkout
        .canonicalize()
        .is_ok_and(|path| repo.root.canonicalize().is_ok_and(|root| root == path));
    let own_setup = crate::config::tracked_setup_dir_in(checkout).is_dir();
    Repo {
        checkout: match main || !own_setup {
            true => repo.root.clone(),
            false => checkout.to_path_buf(),
        },
        borrowed: !main && !own_setup,
        ..repo.clone()
    }
}

/// `target`'s tracked file under `checkout`, named from the checkout, such
/// as `.spoolway/pipelines/impl.yml`. A home-mode setup lives outside the
/// checkout and is named in full.
fn tracked_name(checkout: &Path, target: &str) -> String {
    let path = if target == crate::config::CONFIG_FILE {
        Config::path_in(checkout)
    } else if let Some(name) = target
        .strip_prefix("pipelines/")
        .and_then(|rest| rest.strip_suffix(".yml"))
    {
        Pipelines::file_in(checkout, name)
    } else if let Some(name) = target.strip_prefix("prompts/") {
        crate::prompt::tracked_path_in(checkout, name)
    } else {
        return target.to_string();
    };
    match path.strip_prefix(checkout) {
        Ok(rest) => rest.display().to_string(),
        Err(_) => crate::repo::shorten_home(&path),
    }
}

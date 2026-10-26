//! `promote selected` in the active section: whole files from the layer,
//! written into the tracked setup of a branch the person picks.
//!
//! A promote changes the project's setup, and that change usually goes
//! through a feature branch and a pull request. The board runs in the main
//! checkout, so a write there would land on whatever branch it has out.
//! The picker lists the local branches and the branches `origin` had at
//! the last fetch, task branches left out, and the files go into whichever
//! checkout holds the picked one. When none does, a worktree is made for it
//! beside the main checkout first, with a local branch tracking `origin`'s
//! for one only the remote has. Nothing is committed, and the layer keeps
//! every promoted entry, as `override promote` keeps them.
//!
//! The write itself is `override promote`'s, file by file, into the folder
//! [`Repo::tracked_write_root`] answers for that checkout, so the tab
//! refuses what the command run there would refuse, in the same words.
//! Home mode is the one exception: its setup is the workspace's, which no
//! branch carries, so the tab refuses before the picker opens and points at
//! `override promote` instead.

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

/// The notice refusing a promote from the tab in home mode, or `None` in
/// repo mode.
///
/// A home-mode project's setup is its workspace's `config/`, which no branch
/// carries and every checkout reads. `override promote` run in any checkout
/// writes that one folder, so a picked branch would change nothing but the
/// notice's claim of where the files landed.
pub(super) fn home_mode_refusal(repo: &Repo, files: &[Promoted]) -> Option<Notice> {
    crate::repo::workspace_clone(&repo.root)?;
    let commands: Vec<String> = files
        .iter()
        .map(|file| format!("  spoolway override promote {}", file.target))
        .collect();
    Some(Notice::new(
        "not promoted",
        format!(
            "this project's setup lives in its workspace, not on a branch, so there is no \
             branch to promote onto. write the workspace's setup instead, which every \
             checkout reads:\n{}",
            commands.join("\n")
        ),
    ))
}

/// The text inside both popups, so each is 60 columns wide with its
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
pub(super) const PICK_KEYS: &[(&str, &str)] =
    &[("enter", "pick"), ("f", "search"), ("esc", "close")];

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
        // Spelled here: `plural` adds a bare `s`.
        let marker = |n: usize, arrow: &str, word: &str| match n {
            0 => String::new(),
            1 => format!("  {arrow} 1 branch {word}"),
            n => format!("  {arrow} {n} branches {word}"),
        };
        if cut {
            body.push(marker(offset, "↑", "above"));
        }
        for (at, branch) in shown.iter().enumerate() {
            let mark = if offset + at == self.cursor {
                "> "
            } else {
                "  "
            };
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
        crate::screen::panel(
            &format!("{} to", title(&self.files)),
            &body,
            &keys(PICK_KEYS),
        )
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
    /// The notice to show instead when git cannot say, or names a checkout
    /// that is gone.
    pub(super) fn new(
        repo: &Repo,
        files: Vec<Promoted>,
        branch: Branch,
    ) -> std::result::Result<Promotion, Notice> {
        let place = match branch.remote_only {
            true => None,
            false => listed_checkout(repo, &branch.name)?,
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
    /// Nothing is written unless the worktree was made, nothing is written
    /// into a checkout that no longer has the branch out, and nothing is
    /// written anywhere [`Repo::tracked_write_root`] refuses: a checkout
    /// of a lane, on a detached HEAD, or of a branch with no `.spoolway/`.
    pub(super) fn run(&self, repo: &Repo) -> Notice {
        match self.write(repo) {
            Ok(notice) | Err(notice) => notice,
        }
    }

    fn write(&self, repo: &Repo) -> std::result::Result<Notice, Notice> {
        let refused = |err: anyhow::Error| Notice::new("not promoted", format!("{err:#}"));
        // `tracked_write_root` puts what it is given after `spoolway -C
        // <root>` on its refusal's last line. `override promote` takes one
        // file, so each further file gets a line of its own, spelled in full
        // in the same indent, and pasting the lines promotes every file.
        let typed = self
            .files
            .iter()
            .map(|file| format!("override promote {}", file.target))
            .collect::<Vec<_>>()
            .join(&format!("\n  spoolway -C {} ", repo.root.display()));
        match &self.place {
            Place::Checkout(path) => self.still_out(repo, path)?,
            Place::New(path) => {
                // A branch with no `.spoolway/` would make a worktree only
                // for the write into it to be refused. Asked of the branch
                // itself, so that refusal comes first and nothing is made.
                // The worktree would borrow the main checkout's setup, as
                // `Repo::discover` finds a worktree with none of its own.
                if !carries_setup(repo, &self.branch) {
                    let borrowed = Repo {
                        checkout: repo.root.clone(),
                        borrowed: true,
                        ..repo.clone()
                    };
                    borrowed
                        .tracked_write_root("tracked files", &typed)
                        .map_err(refused)?;
                }
                self.make(repo, path)?;
            }
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

    /// Refuses the write when `checkout` no longer has the picked branch
    /// out. The popup named the checkout when the branch was picked, and a
    /// `git switch` there while it was open would otherwise put the files
    /// on whatever branch it has now, under a notice naming the old one.
    fn still_out(&self, repo: &Repo, checkout: &Path) -> std::result::Result<(), Notice> {
        let wanted = self.branch.name.as_str();
        // Asked through the same listing as the pick, so a checkout whose
        // folder was deleted since is refused as `Promotion::new` refuses it.
        let listed = listed_checkout(repo, wanted)?;
        if listed.as_deref() == Some(checkout) {
            return Ok(());
        }
        let now = match crate::repo::current_branch_at(checkout) {
            Ok(Some(branch)) => branch,
            Ok(None) => "a detached HEAD".to_string(),
            Err(_) => "something git cannot name".to_string(),
        };
        Err(Notice::new(
            "not promoted",
            format!(
                "{} has {now} out now, not {wanted}, so nothing was written. pick {wanted} \
                 again to promote onto it where it is checked out now.",
                checkout.display()
            ),
        ))
    }

    /// `git worktree add` for the branch at `path`, or the notice saying
    /// nothing was written because it failed, with git's error and the path.
    fn make(&self, repo: &Repo, path: &Path) -> std::result::Result<(), Notice> {
        make_worktree(repo, &self.branch, path).map_err(|err| {
            Notice::new(
                "not promoted",
                format!(
                    "no worktree was made for {} at {}, so nothing was written: {err:#}. fix \
                     what stopped it, or check {} out somewhere yourself, and promote again.",
                    self.branch.name,
                    path.display(),
                    self.branch.name
                ),
            )
        })
    }
}

/// The checkout git lists with `branch` out, or `None` when no checkout has
/// it. The notice to show instead when git cannot say, or when the listed
/// folder is gone: git keeps listing a worktree whose folder was deleted
/// until it is pruned, and writing there would make the folder afresh with
/// no checkout in it.
fn listed_checkout(repo: &Repo, branch: &str) -> std::result::Result<Option<PathBuf>, Notice> {
    let listed = repo.worktree_for(branch).map_err(|err| {
        Notice::new(
            "not promoted",
            format!(
                "git could not say which checkout has {branch} out: {err:#}. nothing was \
                 written; fix what stopped it and promote again."
            ),
        )
    })?;
    match listed {
        Some(path) if !path.exists() => Err(Notice::new(
            "not promoted",
            format!(
                "git still lists {branch} as checked out at {}, which no longer exists, so \
                 nothing was written. run git worktree prune in {} and promote again.",
                path.display(),
                repo.root.display()
            ),
        )),
        listed => Ok(listed),
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
/// there: through the rule [`Repo::discover`] uses, which reads a worktree's
/// own setup when its branch has one and borrows the main checkout's when
/// it has none. `checkout` exists: it is listed with the branch out, or was
/// just made.
fn checkout_repo(repo: &Repo, checkout: &Path) -> Repo {
    let start = checkout
        .canonicalize()
        .unwrap_or_else(|_| checkout.to_path_buf());
    let main = crate::repo::main_checkout(&start);
    let (checkout, borrowed) = crate::repo::checkout_for(&start, &repo.root, main.as_deref());
    Repo {
        checkout,
        borrowed,
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

#[cfg(test)]
mod tests {
    use super::super::tests::{key_line, type_keys};
    use super::super::{Section, State};
    use crate::pipeline::Pipelines;
    use crate::repo::Repo;
    use crate::screen::shell;
    use std::path::Path;

    /// The tracked pipeline: two agent steps on sonnet.
    const IMPL: &str = "steps:
  - id: implement
    agent: claude
    model: claude-sonnet-5-5
    on_pass: fix-spec-review
  - id: fix-spec-review
    agent: claude
    model: claude-sonnet-5-5
    on_pass: done
";

    /// The layer's patch for it: both steps on opus.
    const PATCH: &str = "steps:\n  implement:\n    model: claude-opus-5-5\n  fix-spec-review:\n    model: \
         claude-opus-5-5\n";

    fn git(dir: &Path, args: &[&str]) -> String {
        crate::repo::run(dir, "git", args).unwrap()
    }

    /// A main checkout `proj` on `main` tracking pipeline `impl`, inside a
    /// scratch folder, so a worktree made beside it is removed with the
    /// folder. It has a local branch `feature/config-update` with no
    /// checkout, a branch `bare` from before `.spoolway/` was committed, a
    /// task branch, and `origin`'s `feature/remote`, `main` and `HEAD`.
    /// The layer sets both steps of `impl` to opus.
    fn branched(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let scratch = crate::scratch::root(&format!("promote-{name}"));
        let root = scratch.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        git(&root, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(&root, &["branch", "bare"]);
        let file = Pipelines::file_in(&root, "impl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, IMPL).unwrap();
        git(&root, &["add", ".spoolway"]);
        git(&root, &["commit", "-q", "-m", "setup"]);
        for branch in ["feature/config-update", "task/some-lane"] {
            git(&root, &["branch", branch]);
        }
        git(&root, &["remote", "add", "origin", "../origin.git"]);
        for (remote, local) in [("feature/remote", "main"), ("main", "main")] {
            git(
                &root,
                &[
                    "update-ref",
                    &format!("refs/remotes/origin/{remote}"),
                    local,
                ],
            );
        }
        git(
            &root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        let repo = Repo {
            borrowed: false,
            home: scratch.join("home"),
            checkout: root.clone(),
            root,
            config: crate::config::Config::default(),
        };
        let patch = crate::overrides::pipeline_patch_path(&repo.overrides_dir(), "impl");
        std::fs::create_dir_all(patch.parent().unwrap()).unwrap();
        std::fs::write(patch, PATCH).unwrap();
        (repo, scratch)
    }

    /// The tab, opened on its active section, the cursor on
    /// `pipelines/impl.yml`.
    fn on_active(repo: &Repo) -> State {
        let mut state = State::new(repo);
        type_keys(repo, &mut state, "\t\t\t");
        assert_eq!(state.section, Section::Active);
        state
    }

    /// The popup over the list, with the colour of its key row taken out.
    fn popup(state: &State) -> Vec<String> {
        super::super::popup(state, None)
            .unwrap()
            .iter()
            .map(|l| shell::plain(l))
            .collect()
    }

    /// The popup's rows between its borders, the padding taken off.
    fn inside(state: &State) -> Vec<String> {
        let drawn = popup(state);
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

    /// The popup's text as one line, for a notice wrapped over rows.
    fn text(state: &State) -> String {
        inside(state)
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The tracked `impl` pipeline under `checkout`.
    fn tracked(checkout: &Path) -> String {
        std::fs::read_to_string(Pipelines::file_in(checkout, "impl")).unwrap_or_default()
    }

    /// The layer's patch for `impl`, as the file holds it.
    fn layered(repo: &Repo) -> String {
        std::fs::read_to_string(crate::overrides::pipeline_patch_path(
            &repo.overrides_dir(),
            "impl",
        ))
        .unwrap()
    }

    /// Down `n` times, then `enter`.
    fn down_enter(n: usize) -> String {
        format!("{}\r", "\x1b[B".repeat(n))
    }

    // The picker lists local branches and `origin`'s in one list sorted by
    // name, each once. Task branches and `origin/HEAD` are left out, the
    // default branch is kept, and only a branch `origin` alone has carries
    // `origin`.
    #[test]
    fn the_picker_lists_local_and_origin_branches_without_task_ones() {
        let (repo, _guard) = branched("list");
        let names: Vec<(String, bool)> = super::branches(&repo)
            .unwrap()
            .into_iter()
            .map(|b| (b.name, b.remote_only))
            .collect();
        assert_eq!(
            names,
            [
                ("bare".to_string(), false),
                ("feature/config-update".to_string(), false),
                ("feature/remote".to_string(), true),
                ("main".to_string(), false),
            ]
        );
    }

    // `promote selected` is offered first when everything selected is whole
    // files, and opens the picker. On one key of a two-key file, selected or
    // under the cursor, it is left out: it would write the other key too.
    #[test]
    fn promote_is_offered_only_when_every_selected_key_has_its_whole_file() {
        let (repo, _guard) = branched("menu");
        let mut state = on_active(&repo);

        // One key selected out of two.
        type_keys(&repo, &mut state, "\x1b[B o");
        assert_eq!(
            inside(&state)[1..3],
            [
                "  > delete selected".to_string(),
                "    delete all overrides".to_string()
            ]
        );
        type_keys(&repo, &mut state, "\x1b ");

        // Nothing selected, the cursor on that key.
        type_keys(&repo, &mut state, "o");
        assert!(!popup(&state).join("\n").contains("promote selected"));
        type_keys(&repo, &mut state, "\x1b");

        // Both keys selected, the file whole.
        type_keys(&repo, &mut state, "\x1b[B o");
        assert_eq!(
            popup(&state),
            [
                "╭─ 2 selected ─────────────────────────╮",
                "│                                      │",
                "│  > promote selected                  │",
                "│    delete selected                   │",
                "│    delete all overrides              │",
                "│                                      │",
                "│  [enter] confirm   [esc] close       │",
                "╰──────────────────────────────────────╯",
            ]
        );
        type_keys(&repo, &mut state, "\r");
        assert!(popup(&state)[0].starts_with("╭─ promote pipelines/impl.yml to ─"));
    }

    // The picker draws each branch with `origin` on the remote-only one, a
    // search row, and its keys. `f` searches, `esc` in the search clears
    // it, and `esc` closes the picker.
    #[test]
    fn the_picker_draws_its_branches_and_f_searches() {
        let (repo, _guard) = branched("picker");
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r");
        assert_eq!(
            popup(&state),
            [
                "╭─ promote pipelines/impl.yml to ──────────────────────────╮",
                "│  search: ▏                                               │",
                "│                                                          │",
                "│  > bare                                                  │",
                "│    feature/config-update                                 │",
                "│    feature/remote                              origin    │",
                "│    main                                                  │",
                "│                                                          │",
                "│  [enter] pick   [f] search   [esc] close                 │",
                "╰──────────────────────────────────────────────────────────╯",
            ]
        );
        assert_eq!(key_line(&state), " [enter] pick   [f] search   [esc] close");
        type_keys(&repo, &mut state, "fREM");
        assert_eq!(
            inside(&state)[..3],
            [
                "  search: REM▏".to_string(),
                String::new(),
                "  > feature/remote                              origin".to_string(),
            ]
        );
        assert_eq!(inside(&state)[3], "");
        assert_eq!(key_line(&state), " [enter] leave search   [esc] cancel");
        type_keys(&repo, &mut state, "x");
        assert!(text(&state).contains("no branches match this search"));
        type_keys(&repo, &mut state, "\x1b");
        assert_eq!(inside(&state)[2], "  > bare");
        type_keys(&repo, &mut state, "\x1b");
        assert!(super::super::popup(&state, None).is_none());
    }

    // A list longer than the picker scrolls, counting what is out of sight
    // above and below it.
    #[test]
    fn a_long_branch_list_scrolls_with_a_count_above_and_below() {
        let (repo, _guard) = branched("scroll");
        for n in 0..10 {
            git(&repo.root, &["branch", &format!("fix/{n:02}")]);
        }
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r");
        let rows = inside(&state);
        assert_eq!(rows[2], "");
        assert_eq!(rows[3], "  > bare");
        assert_eq!(rows[10], "    ↓ 7 branches below");
        type_keys(&repo, &mut state, &"\x1b[B".repeat(9));
        let rows = inside(&state);
        assert_eq!(rows[2], "    ↑ 3 branches above");
        assert_eq!(rows[9], "  > fix/06");
        assert_eq!(rows[10], "    ↓ 4 branches below");

        // Back up, the list stays where it is until the cursor reaches its
        // top row.
        type_keys(&repo, &mut state, &"\x1b[A".repeat(6));
        let rows = inside(&state);
        assert_eq!(rows[2], "    ↑ 3 branches above");
        assert_eq!(rows[3], "  > fix/00");
    }

    // Picking a branch no checkout has opens the confirm popup naming the
    // worktree to make, the file, the branch, `not committed` and each key.
    // `enter` makes the worktree, writes the file there and nowhere else,
    // commits nothing, and keeps the layer's entry.
    #[test]
    fn promote_onto_a_branch_with_no_checkout_makes_a_worktree_beside_it() {
        let (repo, scratch) = branched("worktree");
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r");
        type_keys(&repo, &mut state, &down_enter(1));
        let made = scratch.join("proj-feature-config-update");
        // A scratch path is longer than the popup's text, so it wraps.
        let folder = crate::screen::wrap(
            &format!("  {}", crate::repo::shorten_home(&made)),
            super::PROMOTE_TEXT,
        );
        let mut expected = vec![
            String::new(),
            "  makes a worktree for feature/config-update at".to_string(),
        ];
        expected.extend(folder.iter().map(|line| format!("  {line}")));
        assert_eq!(
            inside(&state),
            [
                expected,
                vec![
                    "  and writes .spoolway/pipelines/impl.yml there,".to_string(),
                    "  not committed".to_string(),
                    String::new(),
                    "    fix-spec-review.model   claude-opus-5-5".to_string(),
                    "    implement.model         claude-opus-5-5".to_string(),
                    String::new(),
                    "  the overrides stay in the layer until you delete them".to_string(),
                    String::new(),
                    "  [enter] confirm   [esc] close".to_string(),
                ]
            ]
            .concat()
        );
        assert!(popup(&state)[0].starts_with("╭─ promote pipelines/impl.yml ─"));
        assert_eq!(popup(&state)[0].chars().count(), 60);
        assert!(!made.exists(), "nothing is made before enter");

        let head = git(&repo.root, &["rev-parse", "feature/config-update"]);
        type_keys(&repo, &mut state, "\r");
        assert!(
            popup(&state)[0].starts_with("╭─ promoted ─"),
            "{:#?}",
            popup(&state)
        );
        assert!(
            text(&state).contains("nothing is committed"),
            "{}",
            text(&state)
        );
        assert_eq!(
            git(&made, &["branch", "--show-current"]).trim(),
            "feature/config-update"
        );
        assert!(
            tracked(&made).contains("claude-opus-5-5"),
            "{}",
            tracked(&made)
        );
        assert_eq!(tracked(&repo.root), IMPL, "the main checkout is untouched");
        assert_eq!(
            git(&made, &["rev-parse", "HEAD"]),
            head,
            "nothing committed"
        );
        assert_eq!(layered(&repo), PATCH, "the layer keeps the entry");
        type_keys(&repo, &mut state, "\r");
        assert!(super::super::popup(&state, None).is_none());
    }

    // A branch only `origin` has gets a local branch tracking `origin`'s in
    // the worktree made for it.
    #[test]
    fn promote_onto_a_remote_only_branch_makes_a_local_branch_tracking_it() {
        let (repo, scratch) = branched("remote");
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r");
        type_keys(&repo, &mut state, &down_enter(2));
        type_keys(&repo, &mut state, "\r");
        let made = scratch.join("proj-feature-remote");
        assert!(text(&state).contains("wrote"), "{}", text(&state));
        assert_eq!(
            git(&made, &["rev-parse", "--abbrev-ref", "@{upstream}"]).trim(),
            "origin/feature/remote"
        );
        assert!(tracked(&made).contains("claude-opus-5-5"));
    }

    // The branch the main checkout has out is written there, with no
    // worktree made.
    #[test]
    fn promote_onto_the_main_checkouts_branch_writes_the_main_checkout() {
        let (repo, scratch) = branched("main");
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r");
        type_keys(&repo, &mut state, &down_enter(3));
        let rows = inside(&state);
        assert_eq!(rows[1], "  main is checked out at");
        assert_eq!(
            rows[2],
            format!("    {}", crate::repo::shorten_home(&repo.root))
        );
        assert_eq!(
            rows[3],
            "  and this writes .spoolway/pipelines/impl.yml there,"
        );
        type_keys(&repo, &mut state, "\r");
        assert!(tracked(&repo.root).contains("claude-opus-5-5"));
        assert!(!scratch.join("proj-main").exists());
        assert_eq!(layered(&repo), PATCH);
    }

    // A folder already in the worktree's place stops `git worktree add`.
    // The notice gives git's own error and the path, and nothing is
    // written into the folder or anywhere else.
    #[test]
    fn a_folder_in_the_worktrees_place_writes_nothing() {
        let (repo, scratch) = branched("in-the-way");
        let made = scratch.join("proj-feature-config-update");
        std::fs::create_dir_all(&made).unwrap();
        std::fs::write(made.join("notes.txt"), "mine\n").unwrap();
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r");
        type_keys(&repo, &mut state, &down_enter(1));
        type_keys(&repo, &mut state, "\r");

        assert!(popup(&state)[0].starts_with("╭─ not promoted ─"));
        // The notice wraps the path across rows, so it is read back with no
        // spaces at all.
        let said = text(&state).replace(' ', "");
        assert!(said.contains("alreadyexists"), "{said}");
        assert!(said.contains(&made.display().to_string()), "{said}");
        let left: Vec<_> = std::fs::read_dir(&made)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["notes.txt"]);
        assert_eq!(tracked(&repo.root), IMPL);
        assert_eq!(layered(&repo), PATCH);
    }

    // A branch with no `.spoolway/` is refused in `tracked_write_root`'s
    // own words, before any worktree is made.
    #[test]
    fn a_branch_with_no_setup_is_refused_before_a_worktree_is_made() {
        let (repo, scratch) = branched("no-setup");
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r\r");
        assert!(text(&state).contains("makes a worktree for bare at"));
        type_keys(&repo, &mut state, "\r");
        assert!(popup(&state)[0].starts_with("╭─ not promoted ─"));
        let said = text(&state);
        assert!(said.contains("has no .spoolway/ of its own"), "{said}");
        assert!(!scratch.join("proj-bare").exists());
        assert!(git(&repo.root, &["worktree", "list"]).lines().count() == 1);
    }

    // A branch checked out in a lane's worktree is refused in
    // `tracked_write_root`'s own words, and the lane's file is untouched.
    #[test]
    fn a_branch_a_lane_has_out_is_refused_and_nothing_is_written() {
        let (repo, _guard) = branched("lane");
        let lane = repo.worktree_root().join("lane");
        git(
            &repo.root,
            &[
                "worktree",
                "add",
                "-q",
                lane.to_str().unwrap(),
                "feature/config-update",
            ],
        );
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r");
        type_keys(&repo, &mut state, &down_enter(1));
        assert!(text(&state).contains("feature/config-update is checked out at"));
        type_keys(&repo, &mut state, "\r");
        assert!(popup(&state)[0].starts_with("╭─ not promoted ─"));
        let said = text(&state);
        assert!(
            said.contains("a lane does not change the project's setup"),
            "{said}"
        );
        assert_eq!(tracked(&lane), IMPL);
    }

    /// A second pipeline `review`, one step on sonnet, in the main
    /// checkout only, with the layer setting that step to opus. Every other
    /// branch lacks the file, so a promote of it there fails.
    fn add_review(repo: &Repo) {
        std::fs::write(
            Pipelines::file_in(&repo.root, "review"),
            "steps:\n  - id: check\n    agent: claude\n    model: claude-sonnet-5-5\n    \
             on_pass: done\n",
        )
        .unwrap();
        std::fs::write(
            crate::overrides::pipeline_patch_path(&repo.overrides_dir(), "review"),
            "steps:\n  check:\n    model: claude-opus-5-5\n",
        )
        .unwrap();
    }

    // Two whole files: the popup counts them in its title and lead, and
    // names each file with its keys under it. `enter` writes both, and the
    // notice names both.
    #[test]
    fn promote_of_two_files_names_each_and_writes_both() {
        let (repo, _guard) = branched("two-files");
        add_review(&repo);
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "ao\r");
        assert!(popup(&state)[0].starts_with("╭─ promote 2 files to ─"));
        type_keys(&repo, &mut state, &down_enter(3));
        assert!(popup(&state)[0].starts_with("╭─ promote 2 files ─"));
        let rows = inside(&state);
        assert_eq!(rows[1], "  main is checked out at");
        assert_eq!(
            rows[3..],
            [
                "  and this writes 2 files there,",
                "  not committed",
                "",
                "    .spoolway/pipelines/impl.yml",
                "      fix-spec-review.model   claude-opus-5-5",
                "      implement.model         claude-opus-5-5",
                "    .spoolway/pipelines/review.yml",
                "      check.model             claude-opus-5-5",
                "",
                "  the overrides stay in the layer until you delete them",
                "",
                "  [enter] confirm   [esc] close",
            ]
        );

        type_keys(&repo, &mut state, "\r");
        assert!(popup(&state)[0].starts_with("╭─ promoted ─"));
        let said = text(&state);
        assert!(
            said.contains(".spoolway/pipelines/impl.yml, .spoolway/pipelines/review.yml"),
            "{said}"
        );
        assert!(tracked(&repo.root).contains("claude-opus-5-5"));
        let review = std::fs::read_to_string(Pipelines::file_in(&repo.root, "review")).unwrap();
        assert!(review.contains("claude-opus-5-5"), "{review}");
        assert_eq!(layered(&repo), PATCH);
    }

    // When the second of two files fails, the notice says the first was
    // written, where, and which was not, with the error. The first file
    // stays written.
    #[test]
    fn a_second_file_failing_says_the_first_was_promoted_in_part() {
        let (repo, scratch) = branched("in-part");
        add_review(&repo);
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "ao\r");
        type_keys(&repo, &mut state, &down_enter(1));
        type_keys(&repo, &mut state, "\r");

        let made = scratch.join("proj-feature-config-update");
        assert!(
            popup(&state)[0].starts_with("╭─ promoted in part ─"),
            "{:#?}",
            popup(&state)
        );
        let said = text(&state).replace(' ', "");
        assert!(said.contains("pipelines/impl.ymlwaswrittento"), "{said}");
        assert!(said.contains("butnotpipelines/review.yml"), "{said}");
        assert!(said.contains(&made.display().to_string()), "{said}");
        assert!(tracked(&made).contains("claude-opus-5-5"));
        assert!(!Pipelines::file_in(&made, "review").exists());
    }

    // A refusal of several files gives one `-C` line per file, since
    // `override promote` takes one file and pasting a single line would
    // promote only the first.
    #[test]
    fn a_refusal_of_two_files_advises_a_line_for_each() {
        let (repo, _guard) = branched("two-refused");
        add_review(&repo);
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "ao\r\r\r");
        assert!(popup(&state)[0].starts_with("╭─ not promoted ─"));
        let said = text(&state).replace(' ', "");
        let root = repo.root.display().to_string();
        for file in ["pipelines/impl.yml", "pipelines/review.yml"] {
            let line = format!("spoolway-C{root}overridepromote{file}");
            assert!(said.contains(&line), "{line} in {said}");
        }
    }

    // The main checkout switched to another branch while the confirm popup
    // was open: `enter` writes nothing and names both branches.
    #[test]
    fn a_checkout_switched_after_the_pick_is_refused() {
        let (repo, _guard) = branched("switched");
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r");
        type_keys(&repo, &mut state, &down_enter(3));
        assert_eq!(inside(&state)[1], "  main is checked out at");
        git(&repo.root, &["switch", "-q", "feature/config-update"]);
        type_keys(&repo, &mut state, "\r");

        assert!(popup(&state)[0].starts_with("╭─ not promoted ─"));
        let said = text(&state);
        assert!(
            said.contains("has feature/config-update out now, not main"),
            "{said}"
        );
        assert_eq!(tracked(&repo.root), IMPL);
        assert_eq!(git(&repo.root, &["status", "--porcelain"]), "");
    }

    // A worktree whose folder was deleted is still listed by git until it
    // is pruned. Picking its branch says so instead of writing there.
    #[test]
    fn a_deleted_worktree_folder_is_named_with_prune() {
        let (repo, scratch) = branched("pruned");
        let gone = scratch.join("gone");
        git(
            &repo.root,
            &[
                "worktree",
                "add",
                "-q",
                gone.to_str().unwrap(),
                "feature/config-update",
            ],
        );
        std::fs::remove_dir_all(&gone).unwrap();
        let mut state = on_active(&repo);
        type_keys(&repo, &mut state, "o\r");
        type_keys(&repo, &mut state, &down_enter(1));

        assert!(popup(&state)[0].starts_with("╭─ not promoted ─"));
        let said = text(&state).replace(' ', "");
        assert!(said.contains("nolongerexists"), "{said}");
        assert!(said.contains("gitworktreeprune"), "{said}");
        assert!(said.contains(&gone.display().to_string()), "{said}");
        assert!(!gone.exists());
        assert!(!scratch.join("proj-feature-config-update").exists());
    }

    // In home mode the setup is the workspace's, on no branch, so `promote
    // selected` refuses before the picker and names `override promote`.
    #[test]
    fn home_mode_refuses_before_the_picker() {
        let scratch = crate::scratch::root("promote-home-mode");
        let root = scratch.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        let home = scratch.join("ws-home");
        let workspace = home.join(".spoolway").join("promote-ws");
        let pipelines = workspace.join("config").join("pipelines");
        std::fs::create_dir_all(&pipelines).unwrap();
        std::fs::write(pipelines.join("impl.yml"), IMPL).unwrap();
        std::fs::write(
            workspace.join(crate::repo::BINDING_FILE),
            format!(
                "id = \"promote-ws\"\nclones = [{{ root = {:?}, dispatcher = \"api\" }}]\n",
                root.display(),
            ),
        )
        .unwrap();
        let repo = Repo {
            borrowed: false,
            home: scratch.join("home"),
            checkout: root.clone(),
            root,
            config: crate::config::Config::default(),
        };
        let patch = crate::overrides::pipeline_patch_path(&repo.overrides_dir(), "impl");
        std::fs::create_dir_all(patch.parent().unwrap()).unwrap();
        std::fs::write(patch, PATCH).unwrap();

        crate::platform::test_home::with_home(&home, || {
            let mut state = on_active(&repo);
            type_keys(&repo, &mut state, "o\r");
            assert!(popup(&state)[0].starts_with("╭─ not promoted ─"));
            let said = text(&state);
            assert!(said.contains("no branch to promote onto"), "{said}");
            assert!(
                said.contains("spoolway override promote pipelines/impl.yml"),
                "{said}"
            );
            assert_eq!(
                std::fs::read_to_string(pipelines.join("impl.yml")).unwrap(),
                IMPL
            );
        });
    }

    // The worktree is named after the main checkout and the branch, each
    // `/` in the branch a `-`, beside the main checkout.
    #[test]
    fn the_worktree_sits_beside_the_main_checkout() {
        assert_eq!(
            super::worktree_path(Path::new("/home/me/dev/spoolway"), "feature/config-update"),
            Path::new("/home/me/dev/spoolway-feature-config-update")
        );
    }
}

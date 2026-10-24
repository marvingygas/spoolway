//! Whether a newer spoolway has been published, and how to take it.
//!
//! Two halves of one question, kept in one module because they must agree.
//! The **published version** is what the board header shows when a release is
//! out; the **install** is what happens when somebody acts on it. [`Channel`]
//! is resolved once for the install, so a binary that cannot be upgraded from
//! here is told so rather than handed a command that refuses — the bug pi
//! carries (earendil-works/pi#5607, where a Nix install is told to run
//! `pi update` and gets "cannot self-update this installation"). The header
//! names the version, and only on an npm or Homebrew install, with no command
//! for that reason.
//!
//! Nothing here opens a socket. spoolway ships no HTTP client and gains none:
//! an npm install asks npm what the latest version is and npm is what installs
//! it — integrity, platform selection and the atomic swap are all already its
//! job, and hand-rolling a rename over a running executable is the one thing
//! [`crate::config`]'s own documentation warns kills a dispatcher mid-pass.
//!
//! Only a global npm install is upgraded through npm. A Homebrew install
//! ([`Channel::Homebrew`]) learns of a release from brew instead, is upgraded
//! by `brew upgrade`, and never reaches npm. A binary anywhere else
//! ([`Channel::Other`]) is standalone: it never starts npm or brew, never
//! spawns the refresh child and never looks for a newer release. `spoolway
//! update` on it prints [`INSTALL_LINE`] and stops. The channel is checked
//! before any lookup, so there is no path on which a standalone binary
//! reaches npm.
//!
//! The passive check reads a **cache and only a cache**. A lookup older than
//! [`MAX_AGE`] spawns a detached child that refreshes it for the *next* read;
//! this one carries on. That is the whole reason a version check can sit
//! behind the board without making anything slower, and it means the worst
//! case is an answer a day stale rather than a command that hangs on a plane.
//!
//! [`upgrade`] is the one exception, because it is the one caller somebody
//! ran specifically to get the newer binary: it asks npm directly (or, on a
//! Homebrew install, refreshes brew's tap and asks brew), bounded by
//! [`LOOKUP_DEADLINE`] or [`BREW_LOOKUP_DEADLINE`] rather than left to the
//! tool's own unbounded retries, and
//! only falls back to the cache's (possibly stale) answer when that lookup
//! itself fails or does not finish in time — saying so on the way. Answering
//! `spoolway update` from a cache that has not caught up yet is the bug this
//! module exists to have fixed — installing nothing on the one run somebody
//! asked for the upgrade, and only catching up on a second, unasked-for run.
//!
//! Every failure in here is swallowed, and deliberately. A machine offline, an
//! npm that was never installed, a registry that answers slowly — none of that
//! is worth failing a command over, and a version check that can break `queue
//! add` is a version check nobody should have shipped.
//!
//! One question here is not about the registry at all. [`installed_newer`]
//! asks what the `spoolway` on `PATH` is *now*, because an install swaps that
//! file under a dispatcher that goes on running the code it started with. It
//! opens nothing, reaches no network, and answers off a cache a background
//! thread refills — the board asks it on every redraw. [`published_newer`] is
//! [`newer`] behind the same kind of cache, for the board's header, which
//! says when a newer release is out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;

/// The npm package the binary ships as: the unscoped wrapper a person installs,
/// which resolves one `@spoolway/<plat>` package underneath.
pub const PACKAGE: &str = "spoolway";

/// How stale a cached answer may be before a refresh is spawned behind the
/// command. A day: releases are not hourly, and the cost of being wrong is a
/// version that is announced one read late.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Turns the check off for one machine, whatever a project's config says.
///
/// The per-machine half of `housekeeping.update_check`, and the same escape hatch pi spells
/// `PI_SKIP_VERSION_CHECK`. A project setting cannot serve here: the config
/// belongs to the repository and is committed, and "I do not want to hear
/// about releases on this laptop" is not a fact about the repository.
pub const ENV_SKIP: &str = "SPOOLWAY_SKIP_VERSION_CHECK";

/// Set to the old binary's version on the process an upgrade re-execs, so it
/// does its files, can select the exact release range, and stops.
///
/// Without it an install that somehow still saw a newer version would install,
/// exec, install, exec. See [`upgrade`].
pub const ENV_UPGRADED: &str = "SPOOLWAY_UPGRADED";

/// The one line that fetches the latest standalone release.
///
/// What `spoolway update` prints on a binary npm did not install. Defined once
/// so the text a person is told to run cannot drift between callers.
pub const INSTALL_LINE: &str =
    "curl -fsSL https://github.com/marvingygas/spoolway/releases/latest/download/install.sh | sh";

/// The version this binary was built as.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// How this binary got onto the machine, and therefore what can be done about
/// it being out of date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Installed by npm: the wrapper resolved a platform package, so the
    /// executable sits under `node_modules/@spoolway/<platform>/bin/`. This is
    /// the one channel spoolway can upgrade, because npm does the work.
    Npm,
    /// Installed by Homebrew: the executable sits under
    /// `<prefix>/Cellar/spoolway/<version>/bin/`. The daily check asks brew for
    /// the formula's version instead of npm, and `spoolway update` runs
    /// `brew upgrade` for it.
    Homebrew,
    /// Anything else — a release download, `cargo install`, a binary copied
    /// onto `PATH` by hand, a Nix store path, a build under `target/`. Standalone:
    /// it never looks for updates and never starts npm, and `spoolway update`
    /// tells the person the install line instead.
    Other,
}

impl Channel {
    /// Where this binary lives, resolved once.
    ///
    /// Keyed off the path rather than off anything npm sets in the
    /// environment, because a lane's `spoolway report` is launched by the
    /// dispatcher and inherits none of an install's environment — the path is
    /// the only signal that survives.
    ///
    /// Symlinks are resolved first: on macOS `current_exe` can return the
    /// `<prefix>/bin/spoolway` link brew makes rather than the Cellar path it
    /// points at, and the link alone looks like a hand-placed binary. When the
    /// resolution fails the raw path is judged instead. Only this decision
    /// resolves symlinks; the folder lanes get on `PATH` is still the one
    /// `current_exe` names.
    pub fn detect() -> Channel {
        match std::env::current_exe() {
            Ok(exe) => Channel::of(&std::fs::canonicalize(&exe).unwrap_or(exe)),
            Err(_) => Channel::Other,
        }
    }

    /// The same decision, against a path handed in. Split out so it is
    /// testable without an npm install to run under.
    ///
    /// `Npm` means a *global* install — the one layout `npm install -g
    /// spoolway@<v>` can actually replace. A bare `node_modules` component is
    /// not enough: a project-local `npm install spoolway`, an `npx` run, or a
    /// pnpm/yarn layout all have one, and for those `upgrade()` would install a
    /// global copy the project never sees while the terminal says a new version
    /// landed. A global npm prefix always puts its packages under
    /// `<prefix>/lib/node_modules/` (Unix, including nvm and Homebrew) or
    /// `<prefix>/npm/node_modules/` (the Windows `%AppData%\npm` prefix), so the
    /// signal is a `node_modules` component immediately preceded by `lib` or
    /// `npm`. A project directory happening to be named exactly `lib` or `npm`
    /// is the one false positive, and it is a fine one: such a checkout really
    /// is one `npm install -g` can stand in for.
    ///
    /// `Homebrew` is a `Cellar` component followed by `spoolway`, which is the
    /// layout under `/opt/homebrew`, `/usr/local` and
    /// `/home/linuxbrew/.linuxbrew` alike. The npm test runs first, so every
    /// path that was `Npm` stays `Npm`.
    pub fn of(exe: &Path) -> Channel {
        let parts: Vec<String> = exe
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        let global = parts
            .windows(2)
            .any(|pair| pair[1] == "node_modules" && (pair[0] == "lib" || pair[0] == "npm"));
        let cellar = parts
            .windows(2)
            .any(|pair| pair[0] == "Cellar" && pair[1] == BREW_FORMULA_NAME);
        match (global, cellar) {
            (true, _) => Channel::Npm,
            (false, true) => Channel::Homebrew,
            (false, false) => Channel::Other,
        }
    }
}

/// What a cached lookup left behind.
///
/// Read by hand rather than through a serde-derived struct: this file is
/// written by one process and read by another, and a record of two flat fields
/// means a partially written or hand-mangled file is a parse that returns
/// `None` rather than a dependency between two spoolway versions' struct
/// shapes.
///
/// The file is the machine's, and an npm install and a Homebrew install can
/// share it, for example while somebody moves from one to the other. It keeps
/// one record per channel. npm's stays in the top-level `version` and
/// `checked` fields, where every npm binary already released reads it, and
/// Homebrew's sits in a nested `homebrew` object that those binaries never
/// look at. A refresh from this version on rewrites only its own channel's
/// record. An npm binary released before it rewrites the whole file and drops
/// Homebrew's, which costs the Homebrew install one extra lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    pub version: String,
    /// Unix seconds at which the lookup was made.
    pub checked: u64,
}

impl Cached {
    fn from_value(value: &serde_json::Value) -> Option<Cached> {
        Some(Cached {
            version: value.get("version")?.as_str()?.trim().to_string(),
            checked: value.get("checked")?.as_u64()?,
        })
    }

    fn stale(&self, now: u64) -> bool {
        // A cache stamped in the future is a clock that moved, not an answer
        // from tomorrow — and left alone it would never expire again on this
        // machine. Refreshed rather than trusted.
        self.checked > now || now - self.checked >= MAX_AGE.as_secs()
    }
}

/// `$XDG_STATE_HOME/spoolway/latest.json`, or the default state directory.
///
/// Beside `projects.json`, and for the same reason [`crate::usage::registry`]
/// puts that there: this is something spoolway observed about a machine, not
/// something a project configured. A project cannot own it — every checkout on
/// the machine runs the same binary, and asking npm once per repository would
/// be asking the same question several times a day.
pub fn cache_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => crate::platform::home_dir()?.join(".local/state"),
    };
    Some(base.join("spoolway").join("latest.json"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Is `latest` a version after `running`?
///
/// Numeric dotted components, compared left to right. A build suffix (`+…`) on
/// either side is not a difference and is dropped. A pre-release suffix (`-…`)
/// on `latest` is not offered as an upgrade at all: `0.2.0-beta.1` is not a
/// release, so this returns `false` for it even against an older `running`.
/// `npm view spoolway version` returns the `latest` dist-tag today, so this
/// only bites if a pre-release is ever published without `--tag` — but then it
/// is `npm install -g` on every user's machine, so the guard belongs here and
/// not only in the comment. Anything that does not parse is not newer — this
/// decides whether to nag somebody, so the ambiguous answer is silence.
pub fn is_newer(latest: &str, running: &str) -> bool {
    let parts = |v: &str| -> Option<Vec<u64>> {
        let core = v.trim().trim_start_matches('v');
        let core = core.split(['-', '+']).next()?;
        let parsed: Vec<u64> = core
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<_>>()?;
        match parsed.is_empty() {
            true => None,
            false => Some(parsed),
        }
    };

    // A pre-release `latest` is never an upgrade, whatever its core compares
    // to. Build metadata (`+…`) is dropped first so a `+`-only suffix is not
    // mistaken for one.
    if latest
        .trim()
        .trim_start_matches('v')
        .split('+')
        .next()
        .unwrap_or_default()
        .contains('-')
    {
        return false;
    }

    let (Some(latest), Some(running)) = (parts(latest), parts(running)) else {
        return false;
    };
    for index in 0..latest.len().max(running.len()) {
        let a = latest.get(index).copied().unwrap_or(0);
        let b = running.get(index).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

/// The published version that is newer than this binary, if the cache knows of
/// one — and a refresh spawned in the background when the cache is old.
///
/// Reads one file. Everything that can go wrong — no home directory, no cache,
/// a corrupt cache, a clock that moved — comes back `None`.
///
/// A binary neither npm nor Homebrew installed answers `None` before it reads
/// anything, so the refresh child, and the lookup behind it, never start for
/// it.
pub fn newer() -> Option<String> {
    newer_at(Channel::detect(), cache_path())
}

/// [`newer`] for a given channel and cache file, so a test can pick the
/// channel without the real executable's path and seed a cache without
/// setting the process-global state directory other tests read.
fn newer_at(channel: Channel, path: Option<PathBuf>) -> Option<String> {
    if channel == Channel::Other {
        return None;
    }
    let path = path?;
    let cached = read_cache(&path, channel);

    // Spawned whether or not the cache had a usable answer, and before the
    // answer is returned: a machine that has never checked is exactly the one
    // that needs the first lookup started.
    if cached.as_ref().map(|c| c.stale(now())).unwrap_or(true) {
        spawn_refresh();
    }

    let cached = cached?;
    match is_newer(&cached.version, current()) {
        true => Some(cached.version),
        false => None,
    }
}

/// Start a detached child to refresh the cache, and do not wait for it.
///
/// The child is spoolway itself, on a hidden subcommand — the same trick npm's
/// own update-notifier uses: it needs a process that outlives this command,
/// runs the channel's lookup (`npm view`, or `brew info` on a Homebrew
/// install), and writes a file, which this binary already knows how to do
/// without shelling out to a script of its own.
///
/// Through `libc::setsid()`, called in the child between fork and exec, for
/// the reason [`crate::headless`] detaches lanes the same way: a child that
/// merely has no terminal on its file descriptors is still in this session,
/// and gets `SIGHUP` when the terminal closes. A board opened and closed
/// within seconds is exactly the case: the lookup takes seconds and the
/// window does not wait for it. Without a session of its own the cache would never
/// fill on such a machine, and the header would never show a newer version.
///
/// Nothing is waited on, so the child is reaped by init once this process
/// exits. A failure to spawn at all is the same as a failure to look up:
/// silence, and the board's next reading tries again.
fn spawn_refresh() {
    // Not from inside the refresher itself, or a stale cache would fork
    // forever.
    if std::env::var_os(ENV_REFRESHING).is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    let mut command = Command::new(exe);
    // SAFETY: `setsid()` only detaches the child into its own session; it
    // touches nothing this process holds, and runs after `fork` so a failure
    // in it cannot affect this process either.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let _ = command
        .arg(REFRESH_COMMAND)
        .env(ENV_REFRESHING, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Marks the refreshing child, so it never spawns a refresher of its own.
const ENV_REFRESHING: &str = "SPOOLWAY_VERSION_REFRESH";

/// The hidden subcommand [`spawn_refresh`] starts. Named with a leading
/// underscore pair so it cannot be confused for a verb anybody is meant to
/// type.
pub const REFRESH_COMMAND: &str = "__version-check";

/// Ask npm, or brew on a Homebrew install, what the latest published version
/// is, and write it to the cache.
///
/// The whole body of the hidden subcommand. Runs in a process nobody is
/// waiting on, so it may block on the network for as long as the lookup takes.
pub fn refresh() {
    let Some(path) = cache_path() else { return };
    let channel = Channel::detect();
    let Some(version) = (match channel {
        Channel::Homebrew => brew_published(),
        _ => published(),
    }) else {
        // Nothing published, no npm or brew, no network. The cache is still stamped,
        // so a machine that cannot answer is asked once a day rather than on
        // every reading — which is the difference between a quiet failure and
        // a fork on every board refresh.
        let stamp = Cached {
            version: current().to_string(),
            checked: now(),
        };
        write_cache(&path, channel, &stamp);
        return;
    };
    write_cache(
        &path,
        channel,
        &Cached {
            version,
            checked: now(),
        },
    );
}

/// The key the Homebrew record sits under in the cache file. Every other
/// channel's record is the top-level one; see [`Cached`].
const BREW_RECORD: &str = "homebrew";

/// `channel`'s own record in `path`, when it parses. The other channel's
/// record is invisible here, so it can neither answer for this one nor hide
/// that this one has never asked.
fn read_cache(path: &Path, channel: Channel) -> Option<Cached> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    match channel {
        Channel::Homebrew => Cached::from_value(value.get(BREW_RECORD)?),
        _ => Cached::from_value(&value),
    }
}

/// Put `cached` in `path` as `channel`'s record, leaving the other channel's
/// record as it was. A file that is missing or does not parse starts over.
///
/// Two refresh children finishing together can each read the file before the
/// other writes it, and one record is then lost. It costs that channel one
/// extra lookup on its next reading and nothing else, so no lock is taken.
fn write_cache(path: &Path, channel: Channel, cached: &Cached) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = match std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    let record = serde_json::json!({"version": cached.version, "checked": cached.checked});
    match channel {
        Channel::Homebrew => {
            file.insert(BREW_RECORD.to_string(), record);
        }
        _ => {
            file.insert("version".to_string(), record["version"].clone());
            file.insert("checked".to_string(), record["checked"].clone());
        }
    }
    let _ = crate::task::write_atomic(path, format!("{}\n", serde_json::Value::Object(file)));
}

/// The latest version npm knows about, or `None` for anything at all going
/// wrong — including the case that matters today, which is a package that has
/// never been published.
fn published() -> Option<String> {
    let out = npm(&["view", PACKAGE, "version"])?;
    let version = out.trim();
    match version.is_empty() {
        true => None,
        false => Some(version.to_string()),
    }
}

/// The formula's own name, which is also the folder it gets under `Cellar`.
/// Kept apart from [`PACKAGE`], the npm package name, so renaming one does not
/// quietly turn every install of the other into [`Channel::Other`].
const BREW_FORMULA_NAME: &str = "spoolway";

/// The formula this binary was installed from, as brew names it: the tap, then
/// [`BREW_FORMULA_NAME`].
pub const BREW_FORMULA: &str = "marvingygas/tap/spoolway";

/// The version brew's copy of the formula is at, or `None` for anything at all
/// going wrong — no brew, a failed call, output that is not the JSON expected.
///
/// Reads what brew already knows and never runs `brew update`, which would
/// refresh every tap on the person's machine behind their back. A tap that
/// has not been fetched lately therefore reports an older version, which
/// costs a late notice and nothing else. `HOMEBREW_NO_AUTO_UPDATE` is set
/// because brew otherwise runs that update itself before some commands.
fn brew_published() -> Option<String> {
    let out = Command::new("brew")
        .args(["info", "--json=v2", BREW_FORMULA])
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_brew_version(&String::from_utf8_lossy(&out.stdout))
}

/// [`brew_published`] after `brew update`, for the one caller that was asked to
/// get the newer release: brew's copy of the tap can be days old, and the
/// daily check's answer would then name the release the person already has.
///
/// The update's own result is not read. A failed update leaves the tap as it
/// was, and `brew info` then answers from that, which is the best answer there
/// is. Its output is dropped because the notice `update` prints is all the
/// person needs while it runs.
///
/// `deadline` is the same bound [`bounded`] waits for, and the update is
/// killed when it passes. [`bounded`] abandons the thread this runs on but
/// cannot stop a child, and a `brew update` left fetching would be working on
/// the taps while the caller, falling back to the cache, starts `brew upgrade`
/// on them. A killed update answers `None`, so the caller falls back to the
/// cache as it does for any lookup that did not finish.
fn brew_published_after_update(deadline: Duration) -> Option<String> {
    let mut update = Command::new("brew");
    update
        .arg("update")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    status_within(&mut update, deadline)?;
    brew_published()
}

/// Run `command` and wait for it for at most `deadline`. Past that the child is
/// killed and reaped, and the answer is `None`, the same as for a command that
/// could not be started.
fn status_within(command: &mut Command, deadline: Duration) -> Option<std::process::ExitStatus> {
    let mut child = command.spawn().ok()?;
    let end = std::time::Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if std::time::Instant::now() < end => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Run `brew upgrade` for the formula with its output going straight to the
/// terminal, so the person sees brew's own progress and, on failure, brew's own
/// error. Capturing it the way [`npm`] does would hide both.
pub fn brew_upgrade() -> std::io::Result<std::process::ExitStatus> {
    Command::new("brew")
        .args(["upgrade", BREW_FORMULA])
        .status()
}

/// `formulae[0].versions.stable` out of `brew info --json=v2`, when it is a
/// non-empty string.
fn parse_brew_version(json: &str) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(json).ok()?;
    let version = doc.pointer("/formulae/0/versions/stable")?.as_str()?.trim();
    match version.is_empty() {
        true => None,
        false => Some(version.to_string()),
    }
}

/// Run npm, and hand back stdout when it succeeded.
fn npm(args: &[&str]) -> Option<String> {
    let out = Command::new("npm")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    match out.status.success() {
        true => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        false => None,
    }
}

/// What an attempt to take the newer binary did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upgrade {
    /// Nothing newer is known, or this process is already the upgraded one.
    Current,
    /// Installed, and the caller should hand over to the new binary.
    Installed(String),
    /// A newer version exists and the npm install of it failed.
    Unmanaged(String),
    /// A Homebrew install, a newer version exists and no dispatcher is
    /// running. Nothing is installed yet: the caller runs [`brew_upgrade`]
    /// itself, after telling the person what it is about to do, because brew's
    /// progress is printed live rather than captured.
    Homebrew(String),
    /// This binary was installed by neither npm nor Homebrew, so nothing was
    /// looked up and nothing can be installed from here.
    Standalone,
    /// A dispatcher is running, so the binary it is executing is not being
    /// rewritten under it.
    Dispatching(String, u32),
}

/// Take the newer release, when there is one and it is safe to.
///
/// Asks npm, or brew on a Homebrew install, directly rather than trusting
/// [`newer`]'s cache — see the module doc for why this is the one caller that
/// does. A binary neither of them installed asks nothing and answers
/// [`Upgrade::Standalone`].
///
/// `lock_file` is the project's own dispatch lock —
/// [`crate::repo::Repo::lock_file`] for every real caller.
///
/// Deliberately does not re-exec: the caller does that, because only the
/// caller knows what it wanted to do next. See [`hand_over`].
pub fn upgrade(lock_file: &Path) -> Upgrade {
    // The process an upgrade already exec'd. Its files are what it is here to
    // write; installing again would be a loop.
    if std::env::var_os(ENV_UPGRADED).is_some() {
        return Upgrade::Current;
    }
    let channel = Channel::detect();
    let live = move || match channel {
        Channel::Homebrew => brew_published_after_update(BREW_LOOKUP_DEADLINE),
        _ => published(),
    };
    decide_upgrade(channel, lock_file, live, newer, |target| {
        npm(&["install", "-g", target, "--ignore-scripts"])
    })
}

/// How long a lookup made on `update`'s behalf may run before it is treated
/// as a failure.
///
/// Short, and deliberately not configurable — the task this bound exists for
/// rules that out. [`refresh`]'s detached child is where an open-ended wait
/// belongs; this one runs in front of somebody who just typed `spoolway
/// update` and is watching the terminal, and npm does not fail fast against a
/// registry it cannot reach — it retries first. Three seconds is long enough
/// for a real answer and short enough that the retries of an unreachable
/// registry never finish inside it.
const LOOKUP_DEADLINE: Duration = Duration::from_secs(3);

/// [`LOOKUP_DEADLINE`] for a Homebrew install, whose lookup starts with `brew
/// update`. That fetches the tap over the network and takes seconds on a good
/// connection, so npm's three would give up on nearly every run.
const BREW_LOOKUP_DEADLINE: Duration = Duration::from_secs(30);

/// What `update` asks, and how long it waits, on `channel`: the tool's name as
/// the notice says it, and the lookup's bound. `None` for a binary that asks
/// nothing.
fn lookup_for(channel: Channel) -> Option<(&'static str, Duration)> {
    match channel {
        Channel::Npm => Some(("npm", LOOKUP_DEADLINE)),
        Channel::Homebrew => Some(("Homebrew", BREW_LOOKUP_DEADLINE)),
        Channel::Other => None,
    }
}

/// Run `live` on its own thread and stop waiting on it after `deadline`.
///
/// The thread is never joined: a `live` that is still blocked in npm's own
/// retries, or in `brew update`, when the deadline passes is left to finish (or
/// not) on its own, the same way [`spawn_refresh`]'s detached child is never
/// waited on. Its eventual answer, if any, is simply never read. What `live`
/// starts is not stopped from here, so a `live` that runs a child must bound it
/// itself, as [`brew_published_after_update`] does.
fn bounded(
    live: impl FnOnce() -> Option<String> + Send + 'static,
    deadline: Duration,
) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    // The fallible builder, not `thread::spawn`: this module's whole contract
    // is that every failure is swallowed, and `thread::spawn` panics when the
    // OS cannot start a thread — which would abort `update` on exactly the
    // kind of resource exhaustion this function exists to be quiet about.
    let spawned = std::thread::Builder::new().spawn(move || {
        // The receiver may already be gone (deadline passed) — a send into a
        // dropped channel is exactly the "nobody is listening any more" case
        // and is not an error worth doing anything about.
        let _ = tx.send(live());
    });
    if spawned.is_err() {
        return None;
    }
    rx.recv_timeout(deadline).ok().flatten()
}

/// The decision half of [`upgrade`], with the channel, the live lookup, the
/// cache fallback and the install passed in rather than called directly.
///
/// Split out so a test can hand in a stubbed `live` answer and a `cached`
/// closure that panics if ever called — proving the cache is bypassed
/// outright, and only reached on a failed lookup — without seeding a real
/// cache file, standing up a fake `npm` on `PATH`, or touching the process
/// environment at all. A test binary sits under `target/`, which is
/// [`Channel::Other`], so the channel is an argument for the same reason.
/// `live` runs bounded by [`LOOKUP_DEADLINE`] (or [`BREW_LOOKUP_DEADLINE`] on a
/// Homebrew install), on its own thread, so a stub that never answers proves
/// the bound rather than hanging the test. `install` is only called on the npm
/// channel: a Homebrew install answers [`Upgrade::Homebrew`] and leaves the
/// upgrade to the caller.
fn decide_upgrade(
    channel: Channel,
    lock_file: &Path,
    live: impl FnOnce() -> Option<String> + Send + 'static,
    cached: impl FnOnce() -> Option<String>,
    install: impl FnOnce(&str) -> Option<String>,
) -> Upgrade {
    // Before the notice and before `live`: a standalone binary must not so
    // much as announce a lookup, and a check placed after `bounded` would
    // already have started npm or brew. A Homebrew binary gets its own lookup
    // and must never reach `npm install -g`, which would put a second copy
    // beside the one brew owns.
    let Some((tool, deadline)) = lookup_for(channel) else {
        return Upgrade::Standalone;
    };
    println!("Checking {tool} for a newer spoolway...");
    // Asked live, because this is the one call somebody made specifically to
    // get the newer binary — a cache the passive check would happily wait a
    // day on is exactly what left `update` installing nothing on the run
    // that mattered. `live()` returning `None` is "the tool could not answer"
    // (offline, npm or brew not installed or erroring, or the deadline
    // passing) rather than "nothing newer", so that and only that falls back
    // to the cache's own answer; npm or brew actually answering settles it
    // either way, cache or no cache.
    let version = match bounded(live, deadline) {
        Some(latest) if is_newer(&latest, current()) => Some(latest),
        Some(_) => None,
        None => {
            println!("No answer from {tool}, carrying on with the version already known.");
            println!();
            cached()
        }
    };
    let Some(version) = version else {
        return Upgrade::Current;
    };
    // Refuse rather than rely on a remembered release rule: a running dispatcher
    // is executing the file the upgrade is about to replace, and a half-written
    // binary is a dispatcher that dies mid-pass.
    if let Ok(Some(pid)) = crate::lock::Lock::holder(lock_file) {
        return Upgrade::Dispatching(version, pid);
    }
    if channel == Channel::Homebrew {
        return Upgrade::Homebrew(version);
    }

    // `--ignore-scripts` costs nothing here and takes lifecycle scripts out of
    // the upgrade path: the packaging deliberately has no postinstall, which
    // is what makes the npm install work under `--ignore-scripts` in the first
    // place; the wrapper contains no lifecycle install script.
    let target = format!("{PACKAGE}@{version}");
    match install(&target) {
        Some(_) => Upgrade::Installed(version),
        None => Upgrade::Unmanaged(version),
    }
}

/// How long a reading of the `spoolway` on `PATH` is trusted before another is
/// taken.
///
/// An install is a thing somebody does by hand, so a minute-old answer is
/// never meaningfully wrong; what it buys is that a board redrawing once a
/// second spawns one child a minute rather than sixty.
const PATH_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// How long the `PATH` executable gets to answer `--version` before its
/// reading is given up on.
///
/// Nothing waits on this — the reading runs on its own thread — so the bound
/// is not there to keep a caller fast. It is there so an executable that
/// never answers loses its turn instead of holding the one in-flight slot for
/// the life of the run, which would freeze the hint at whatever it last said.
const PATH_CHECK_DEADLINE: Duration = Duration::from_secs(5);

/// The last reading taken through [`cached_reading`], and whether one is in
/// flight — of the `spoolway` on `PATH`, or of the published version.
#[derive(Debug, Default)]
struct Reading {
    /// What the reader answered, or `None` where the reading failed or had
    /// nothing to say — no `spoolway` on `PATH`, a non-zero exit, nothing
    /// newer published, or the deadline passing.
    version: Option<String>,
    /// When the last reading finished, either way. `None` until one has.
    read_at: Option<Instant>,
    /// A reading is running right now, so a second call does not start a
    /// second one behind it.
    reading: bool,
}

static PATH_VERSION: OnceLock<Mutex<Reading>> = OnceLock::new();

fn path_version_cell() -> &'static Mutex<Reading> {
    PATH_VERSION.get_or_init(|| Mutex::new(Reading::default()))
}

/// The version the `spoolway` on `PATH` reports, when it is newer than the one
/// this process is running.
///
/// `None` covers every other case, and deliberately the same way: an equal or
/// older executable, one that reports something that does not parse, one that
/// fails or never answers, and no `spoolway` on `PATH` at all. This decides
/// whether to tell somebody to restart their dispatcher, so the ambiguous
/// answer is silence — the same rule [`is_newer`] follows.
///
/// Never waits for the executable. The board asks this on every redraw, about
/// once a second, and a process spawn is not a thing to put on that path; the
/// answer comes off a cache that a background thread refills, so the first
/// call of a run says `None` and a later one carries the reading.
pub fn installed_newer() -> Option<String> {
    cached_reading(
        path_version_cell(),
        path_reported_version,
        PATH_CHECK_INTERVAL,
        PATH_CHECK_DEADLINE,
    )
    .filter(|version| is_newer(version, current()))
}

/// How long a reading of the published version is trusted before another is
/// taken.
///
/// The cache file it reads is refreshed at most once per [`MAX_AGE`], so
/// reading it more often would only read the same answer again; a refresh
/// that has just landed waits at most a minute before the board shows it.
/// What it buys is the same as [`PATH_CHECK_INTERVAL`]'s: a board redrawing
/// once a second reads the file, and may spawn a refresh, once a minute.
const PUBLISHED_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// How long [`newer`] gets before its reading is given up on. It reads one
/// small file, so this only ever matters for a filesystem that has stopped
/// answering — and then, as with [`PATH_CHECK_DEADLINE`], so the stalled
/// reading hands the slot back rather than freezing the header.
const PUBLISHED_CHECK_DEADLINE: Duration = Duration::from_secs(5);

static PUBLISHED_VERSION: OnceLock<Mutex<Reading>> = OnceLock::new();

fn published_version_cell() -> &'static Mutex<Reading> {
    PUBLISHED_VERSION.get_or_init(|| Mutex::new(Reading::default()))
}

/// [`newer`], answered off memory: the published version newer than this
/// binary, as of the last reading.
///
/// For the board, which asks on every redraw. [`newer`] reads the cache file
/// and may spawn a refresh child, neither of which belongs on a path that
/// runs once a second, so the reading is taken on a background thread once
/// per [`PUBLISHED_CHECK_INTERVAL`] and every call in between answers from
/// what it left. The first call of a run therefore says `None`, the same as
/// [`installed_newer`]'s. A binary neither npm nor Homebrew installed says
/// `None` without starting that thread.
///
/// Gates nothing itself: whether the header wants it at all —
/// `housekeeping.update_check` and [`ENV_SKIP`] — is the caller's to decide,
/// before it calls, so a check that is turned off never starts a reading.
pub fn published_newer() -> Option<String> {
    if Channel::detect() == Channel::Other {
        return None;
    }
    cached_reading(
        published_version_cell(),
        newer,
        PUBLISHED_CHECK_INTERVAL,
        PUBLISHED_CHECK_DEADLINE,
    )
}

/// The cache behind [`installed_newer`] and [`published_newer`], with its
/// state, its reader and its two durations passed in.
///
/// Split out so a test can drive it with a stub reader and durations short
/// enough to watch — including a reader that never answers, which proves the
/// deadline rather than hanging the test. `read` is a plain function pointer
/// because the reading is handed to a thread that outlives this call.
fn cached_reading(
    cell: &'static Mutex<Reading>,
    read: fn() -> Option<String>,
    interval: Duration,
    deadline: Duration,
) -> Option<String> {
    // A panic inside the refresh thread would poison this, and a poisoned
    // lock is not a reason to lose the hint: the state behind it is two
    // fields that are overwritten wholesale, never half-updated.
    let mut state = cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let stale = state
        .read_at
        .is_none_or(|read_at| read_at.elapsed() >= interval);
    if stale && !state.reading {
        // Claimed before the thread starts, so two callers racing here cannot
        // both spawn one.
        state.reading = true;
        let spawned = std::thread::Builder::new().spawn(move || {
            let reading = bounded(read, deadline);
            let mut state = cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.version = reading;
            // Stamped on a failed reading too, so an executable that is
            // missing or slow is retried on the next interval rather than on
            // every redraw in between.
            state.read_at = Some(Instant::now());
            state.reading = false;
        });
        // The same swallowed failure as everywhere else here: an OS that
        // cannot start a thread leaves the slot free for the next call rather
        // than failing the frame.
        if spawned.is_err() {
            state.reading = false;
        }
    }
    state.version.clone()
}

/// The folder a lane's or command step's `PATH` starts with, so the
/// `spoolway` they call is the dispatcher's own.
///
/// A lane reports with whatever `spoolway` comes first on its `PATH`. One
/// older than the dispatcher cannot read the pipeline snapshot the dispatcher
/// wrote, so every `report` fails and the task stays `running` for ever.
///
/// Worked out once, at the first call. `spoolway update` can replace the file
/// in this folder under a running dispatcher, and the answer is cached so the
/// folder stays first on the `PATH` of every later lane. Asked afresh, a
/// folder whose binary was replaced would be dropped and a lane would fall
/// back to whatever `spoolway` the person's own `PATH` holds, which may be
/// older. The lanes then run the folder's new install, which is the one the
/// person asked for. The answer is:
///
/// - `Ok(Some(dir))`: `dir/spoolway` reports this build's version.
/// - `Ok(None)`: this process is not a binary named `spoolway`, as under
///   `cargo test`, so there is no folder to claim.
/// - `Err(why)`: the folder's `spoolway` is gone or is another version.
///   `PATH` is left alone, and `why` is shown among the startup settings.
pub fn dispatcher_bin_dir() -> &'static Result<Option<PathBuf>, String> {
    #[cfg(test)]
    if let Some(answer) = test_dir::current() {
        return answer;
    }
    static DIR: std::sync::OnceLock<Result<Option<PathBuf>, String>> = std::sync::OnceLock::new();
    DIR.get_or_init(dispatcher_bin_dir_now)
}

/// [`dispatcher_bin_dir`]'s question asked afresh, for the process that is
/// about to start a dispatcher and has to say what that dispatcher will find.
///
/// The board's dispatch tab is such a process. It starts a child from its own
/// binary and lives across installs, so its cached answer may be from before
/// an update. The child judges the binary at startup, and the board's own
/// executable is the one the child runs, so a fresh look here is the child's
/// answer.
pub fn dispatcher_bin_dir_now() -> Result<Option<PathBuf>, String> {
    #[cfg(test)]
    if let Some(answer) = test_dir::current() {
        return answer.clone();
    }
    match std::env::current_exe() {
        Ok(exe) => resolve_bin_dir(&exe, current()),
        Err(_) => Ok(None),
    }
}

/// [`dispatcher_bin_dir`]'s answer for one executable path and version.
///
/// `current_exe` on Linux names a replaced binary `…/spoolway (deleted)`, so
/// that suffix is dropped before the name is judged. The folder is still the
/// right one to look in, because the install writes the new file beside the
/// old name. Whatever is there is then asked its version, since a file at the
/// name is no proof it is this build.
fn resolve_bin_dir(exe: &Path, version: &str) -> Result<Option<PathBuf>, String> {
    let name = exe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(dir) = exe.parent() else {
        return Ok(None);
    };
    if name.trim_end_matches(" (deleted)") != PACKAGE {
        return Ok(None);
    }
    let program = dir.join(PACKAGE);
    // Bounded, because the board asks this synchronously on every dispatch
    // start, and a binary that hangs on `--version` must not freeze it.
    let reported = {
        let program = program.clone();
        bounded(move || reported_version_of(&program), LOOKUP_DEADLINE)
    };
    match reported {
        Some(found) if found == version => Ok(Some(dir.to_path_buf())),
        Some(found) => Err(format!(
            "lanes use the `{PACKAGE}` on PATH: {} is version {found}, not this \
             dispatcher's {version}. Restart the dispatcher to pick up the new install",
            program.display()
        )),
        None => Err(format!(
            "lanes use the `{PACKAGE}` on PATH: {} is gone or does not answer `--version`. \
             Restart the dispatcher to pick up the new install",
            program.display()
        )),
    }
}

/// Put `dir` first on the `PATH` in `env`, ahead of `path`.
///
/// Without a `dir` the map is left alone, so the lane inherits whatever
/// `PATH` its shell already has.
pub fn put_first_on_path(
    env: &mut BTreeMap<String, String>,
    dir: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
) {
    let Some(dir) = dir else { return };
    let mut dirs = vec![dir.to_path_buf()];
    dirs.extend(
        path.map(|path| std::env::split_paths(path).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    if let Ok(joined) = std::env::join_paths(dirs) {
        env.insert("PATH".to_string(), joined.to_string_lossy().into_owned());
    }
}

/// The folder a lane is started with first on its `PATH`, if there is one.
///
/// A lane gets it as a prefix on the `PATH` its own shell has, which is how a
/// herdr pane keeps the shell's `PATH` and a headless turn keeps the
/// dispatcher's.
pub fn lane_path_prefix() -> Option<PathBuf> {
    dispatcher_bin_dir().as_ref().ok().cloned().flatten()
}

/// Make the `spoolway` a command step calls the dispatcher's own.
///
/// A command step's environment is a map of values rather than a shell to
/// prefix, and a shown one already replays the dispatcher's whole
/// environment, so `PATH` is written out in full.
pub fn use_dispatcher_binary(env: &mut BTreeMap<String, String>) {
    let dir = dispatcher_bin_dir()
        .as_ref()
        .ok()
        .and_then(Option::as_deref);
    put_first_on_path(env, dir, std::env::var_os("PATH").as_deref());
}

/// Ask the `spoolway` on `PATH` what version it is.
///
/// Resolved through `PATH` rather than `current_exe` for the reason
/// [`hand_over`] gives: after an install the name resolves to a different file
/// than the inode this process is executing, and that difference is the whole
/// question.
fn path_reported_version() -> Option<String> {
    let program =
        std::env::var_os("PATH").and_then(|path| crate::platform::which(PACKAGE, &path))?;
    reported_version_of(&program)
}

/// What `program --version` says its version is, or `None` when it does not
/// run, exits non-zero or prints nothing.
fn reported_version_of(program: &Path) -> Option<String> {
    let out = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    match out.status.success() {
        true => reported_version(&String::from_utf8_lossy(&out.stdout)),
        false => None,
    }
}

/// The version token out of a `spoolway --version` line.
///
/// clap prints `spoolway 0.5.0`, so the token is the last word of the first
/// line that has one. Whatever comes back is handed to [`is_newer`], which
/// treats anything that does not parse as not newer — so this does not have to
/// decide what a version looks like, only which word to hand over.
fn reported_version(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.split_whitespace().next_back())
        .map(str::to_string)
}

/// Hand over to the binary that was just installed, so what runs from here on
/// is its own.
///
/// The release-notes digest `update` prints once the handover returns is
/// generated from the running binary's own `include_str!` tables — the exact
/// reason `spoolway sync`'s files carry the same rule — so a process that
/// installs 0.2.0 and carries on rather than handing over would report 0.2.0
/// as current while still reading 0.1.0's compiled-in notes.
///
/// Resolved through `PATH` rather than through `current_exe`. After an npm
/// install, `current_exe` on Unix is the *inode* this process is executing and
/// npm has just written a different file at the name. After a Homebrew
/// upgrade, it names `Cellar/spoolway/<old version>/`, which brew has removed.
///
/// `expect_version` is what the install just put down. `PATH` is not proof that
/// the name now resolves to it — under `npx` or `node_modules/.bin` the first
/// `spoolway` on `PATH` is still the old binary, which would run with the old
/// `include_str!` tables and exit 0. So the resolved binary is asked its
/// version *before* it is handed the relaunch: a mismatch refuses here,
/// rather than letting the wrong binary answer for the handover and only
/// then reporting it went astray.
pub fn hand_over(args: &[String], expect_version: &str) -> Result<std::process::ExitStatus> {
    let program = std::env::var_os("PATH")
        .and_then(|path| crate::platform::which(PACKAGE, &path))
        .unwrap_or_else(|| PathBuf::from(PACKAGE));

    let reported = Command::new(&program)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    match reported {
        // `spoolway --version` prints `spoolway <version>`; match on the
        // version token anywhere in the line rather than the whole string.
        Some(line) if line.split_whitespace().any(|word| word == expect_version) => {}
        Some(line) => anyhow::bail!(
            "the `spoolway` on PATH resolves to `{}`, which reports `{line}` rather than the \
             installed {expect_version}, so it is a different install from the one just \
             upgraded (this one ran from `{}`); put the upgraded spoolway's folder first on \
             PATH, or remove the other copy (an npm, `npx` or project-local one), then run \
             spoolway update again",
            program.display(),
            std::env::current_exe()
                .map_or_else(|_| "an unknown path".into(), |p| p.display().to_string())
        ),
        None => anyhow::bail!(
            "the `spoolway` on PATH (`{}`) did not report a version — cannot confirm the \
             installed {expect_version} is what the file work would run",
            program.display()
        ),
    }

    Ok(upgraded_command(&program, args).status()?)
}

fn upgraded_command(program: &Path, args: &[String]) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    // The old process is the only authoritative source for the lower end of a
    // skipped-release range. Carry it across the executable swap; the new
    // process cannot reconstruct it from npm's latest-version cache, which now
    // contains only its own version.
    command.env(ENV_UPGRADED, current());
    command
}

/// A test's stand-in for [`dispatcher_bin_dir`], since the real answer is
/// worked out once per process from the test binary, which claims nothing.
#[cfg(test)]
pub(crate) mod test_dir {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    thread_local! {
        static DIR: RefCell<Option<&'static Result<Option<PathBuf>, String>>> =
            const { RefCell::new(None) };
    }

    pub(super) fn current() -> Option<&'static Result<Option<PathBuf>, String>> {
        DIR.with(|dir| *dir.borrow())
    }

    /// Make this thread's dispatcher run from `dir` until the thread ends.
    pub(crate) fn pin(dir: &Path) {
        pin_answer(Ok(Some(dir.to_path_buf())));
    }

    /// Make this thread's dispatcher get `answer` until the thread ends.
    /// The answer is leaked, which a test process can afford.
    pub(crate) fn pin_answer(answer: Result<Option<PathBuf>, String>) {
        let answer: &'static _ = Box::leak(Box::new(answer));
        DIR.with(|slot| *slot.borrow_mut() = Some(answer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder holding an executable `spoolway` that prints `version`.
    fn fake_install(name: &str, version: &str) -> crate::scratch::ScratchRoot {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::scratch::root(name);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join(PACKAGE);
        std::fs::write(&program, format!("#!/bin/sh\necho 'spoolway {version}'\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    /// The dispatcher's folder goes ahead of every entry of the `PATH` it is
    /// put on, so an older `spoolway` further along is never reached first.
    #[test]
    fn the_folder_goes_ahead_of_every_entry_on_the_path_it_is_given() {
        let mut env = BTreeMap::new();
        put_first_on_path(
            &mut env,
            Some(Path::new("/opt/new")),
            Some(std::ffi::OsStr::new("/usr/old:/usr/bin")),
        );
        assert_eq!(env["PATH"], "/opt/new:/usr/old:/usr/bin");
    }

    /// With no folder to claim, the lane keeps the `PATH` it would inherit.
    #[test]
    fn no_dispatcher_folder_leaves_path_alone() {
        let mut env = BTreeMap::new();
        put_first_on_path(&mut env, None, Some(std::ffi::OsStr::new("/usr/bin")));
        assert!(!env.contains_key("PATH"));
    }

    /// A binary of this version, named `spoolway`, is the folder to claim —
    /// also when Linux reports it as replaced.
    #[test]
    fn the_folder_of_a_matching_binary_is_claimed() {
        let dir = fake_install("bin-dir-match", "1.2.3");
        let exe = dir.join(PACKAGE);
        assert_eq!(resolve_bin_dir(&exe, "1.2.3"), Ok(Some(dir.to_path_buf())));
        let replaced = dir.join("spoolway (deleted)");
        assert_eq!(
            resolve_bin_dir(&replaced, "1.2.3"),
            Ok(Some(dir.to_path_buf()))
        );
    }

    /// An update that swapped in another version, or removed the file, leaves
    /// `PATH` alone and says why.
    #[test]
    fn a_replaced_or_missing_binary_is_reported_not_claimed() {
        let dir = fake_install("bin-dir-other", "2.0.0");
        let other = resolve_bin_dir(&dir.join(PACKAGE), "1.2.3").unwrap_err();
        assert!(
            other.contains("2.0.0") && other.contains("1.2.3"),
            "{other}"
        );

        let empty = crate::scratch::root("bin-dir-gone");
        let gone = resolve_bin_dir(&empty.join(PACKAGE), "1.2.3").unwrap_err();
        assert!(gone.contains("gone"), "{gone}");
    }

    /// A process that is not a binary named `spoolway`, such as a test
    /// binary, claims nothing.
    #[test]
    fn a_binary_with_another_name_claims_nothing() {
        let dir = fake_install("bin-dir-name", "1.2.3");
        assert_eq!(
            resolve_bin_dir(&dir.join("spoolway-3f9a"), "1.2.3"),
            Ok(None)
        );
    }

    /// Which installs `update` may touch: only an npm global one, never a
    /// Nix store, a hand-placed binary, a project-local install or an `npx`
    /// run.
    #[test]
    fn only_a_global_npm_install_is_on_the_npm_channel() {
        let npm = Path::new(
            "/home/x/.nvm/versions/node/v20.0.0/lib/node_modules/@spoolway/linux-x64/bin/spoolway",
        );
        // The Windows global prefix is `%AppData%\npm`, so `npm/node_modules`
        // stands in for `lib/node_modules` there. Built from components rather
        // than a `\`-separated literal, which `Path` does not split on Unix.
        let npm_win: PathBuf = [
            "npm",
            "node_modules",
            "@spoolway",
            "win32-x64",
            "bin",
            "spoolway.exe",
        ]
        .iter()
        .collect();
        let hand = Path::new("/home/x/.local/bin/spoolway");
        let nix = Path::new("/nix/store/abc123-spoolway-0.1.0/bin/spoolway");
        // A project-local install and an `npx` run each have a `node_modules`
        // component, but neither is what `npm install -g` replaces — finding 19.
        let local = Path::new("/home/x/webshop/node_modules/@spoolway/linux-x64/bin/spoolway");
        let local_bin = Path::new("/home/x/webshop/node_modules/.bin/spoolway");
        let npx = Path::new("/home/x/.npm/_npx/abc/node_modules/@spoolway/linux-x64/bin/spoolway");

        assert_eq!(Channel::of(npm), Channel::Npm);
        assert_eq!(Channel::of(&npm_win), Channel::Npm);
        assert_eq!(Channel::of(hand), Channel::Other);
        assert_eq!(Channel::of(nix), Channel::Other);
        assert_eq!(Channel::of(local), Channel::Other);
        assert_eq!(Channel::of(local_bin), Channel::Other);
        assert_eq!(Channel::of(npx), Channel::Other);
    }

    /// A Cellar layout under any of brew's three prefixes is Homebrew, and a
    /// path that only resembles one is not.
    #[test]
    fn a_cellar_path_is_on_the_homebrew_channel() {
        for prefix in ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"] {
            let exe = format!("{prefix}/Cellar/spoolway/0.15.0/bin/spoolway");
            assert_eq!(Channel::of(Path::new(&exe)), Channel::Homebrew, "{exe}");
        }
        // The link brew makes is not the Cellar path, so it is not Homebrew
        // until `detect` has resolved it.
        assert_eq!(
            Channel::of(Path::new("/opt/homebrew/bin/spoolway")),
            Channel::Other
        );
        assert_eq!(
            Channel::of(Path::new("/opt/homebrew/Cellar/other/1.0/bin/spoolway")),
            Channel::Other
        );
    }

    /// Each channel reads and rewrites only its own record, and npm's stays at
    /// the top level where a binary released before Homebrew support reads it.
    #[test]
    fn each_channel_keeps_its_own_cache_record() {
        let npm = Cached {
            version: "0.2.0".into(),
            checked: 5,
        };
        let brew = Cached {
            version: "0.3.0".into(),
            checked: 9,
        };
        let dir = crate::scratch::root("release-cache-channel");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("latest.json");

        write_cache(&path, Channel::Homebrew, &brew);
        assert_eq!(read_cache(&path, Channel::Homebrew), Some(brew.clone()));
        assert_eq!(read_cache(&path, Channel::Npm), None);

        // npm's refresh leaves brew's record alone, and brew's leaves npm's.
        write_cache(&path, Channel::Npm, &npm);
        assert_eq!(read_cache(&path, Channel::Homebrew), Some(brew.clone()));
        assert_eq!(read_cache(&path, Channel::Npm), Some(npm.clone()));
        let newer_brew = Cached {
            version: "0.4.0".into(),
            checked: 11,
        };
        write_cache(&path, Channel::Homebrew, &newer_brew);
        assert_eq!(read_cache(&path, Channel::Npm), Some(npm.clone()));
        assert_eq!(read_cache(&path, Channel::Homebrew), Some(newer_brew));

        // A binary released before this change reads only the top level.
        let text = std::fs::read_to_string(&path).unwrap();
        let top: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(Cached::from_value(&top), Some(npm));
    }

    /// The formula spelling brew is asked for ends in the folder name the
    /// channel is keyed off.
    #[test]
    fn the_brew_formula_ends_in_the_cellar_folder_name() {
        assert!(BREW_FORMULA.ends_with(&format!("/{BREW_FORMULA_NAME}")));
    }

    /// Only a non-empty `formulae[0].versions.stable` is an answer.
    #[test]
    fn brew_output_that_does_not_name_a_version_is_no_answer() {
        let ok = r#"{"formulae":[{"versions":{"stable":"0.15.0"}}]}"#;
        assert_eq!(parse_brew_version(ok), Some("0.15.0".to_string()));
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"formulae":[]}"#,
            r#"{"formulae":[{"versions":{"stable":null}}]}"#,
            r#"{"formulae":[{"versions":{"stable":""}}]}"#,
        ] {
            assert_eq!(parse_brew_version(bad), None, "{bad}");
        }
    }

    /// A Homebrew binary never reaches the npm install, and a newer release is
    /// left for the caller to run `brew upgrade` on.
    #[test]
    fn a_homebrew_binary_is_upgraded_through_brew_and_never_npm() {
        let answered = newer_than_current();
        let outcome = decide_upgrade(
            Channel::Homebrew,
            Path::new("/nonexistent/lock"),
            {
                let answered = answered.clone();
                move || Some(answered)
            },
            || panic!("read the cache"),
            |_| panic!("ran npm"),
        );
        assert_eq!(outcome, Upgrade::Homebrew(answered));
    }

    /// A Homebrew lookup that fails falls back to the cache, like npm's, and
    /// brew saying nothing is newer ends it without the cache.
    #[test]
    fn a_homebrew_lookup_falls_back_to_the_cache_only_when_it_fails() {
        let outcome = decide_upgrade(
            Channel::Homebrew,
            Path::new("/nonexistent/lock"),
            || None,
            || Some("0.4.0".to_string()),
            |_| panic!("ran npm"),
        );
        assert_eq!(outcome, Upgrade::Homebrew("0.4.0".to_string()));

        let outcome = decide_upgrade(
            Channel::Homebrew,
            Path::new("/nonexistent/lock"),
            || Some(current().to_string()),
            || panic!("the cache must not get a second vote once brew has answered"),
            |_| panic!("ran npm"),
        );
        assert_eq!(outcome, Upgrade::Current);
    }

    /// `brew update` fetches the tap, so the Homebrew lookup is given thirty
    /// seconds where npm gets three, and a standalone binary is given none.
    #[test]
    fn each_channel_is_given_its_own_lookup_bound() {
        assert_eq!(
            lookup_for(Channel::Homebrew),
            Some(("Homebrew", Duration::from_secs(30)))
        );
        assert_eq!(
            lookup_for(Channel::Npm),
            Some(("npm", Duration::from_secs(3)))
        );
        assert_eq!(lookup_for(Channel::Other), None);
    }

    /// A child still running at the deadline is killed rather than left to
    /// fetch while the caller moves on.
    #[test]
    fn a_command_past_its_deadline_is_killed_and_gives_no_answer() {
        let started = std::time::Instant::now();
        let mut sleeper = Command::new("sleep");
        sleeper.arg("30");
        assert_eq!(
            status_within(&mut sleeper, Duration::from_millis(200)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(10));

        let mut quick = Command::new("true");
        let status = status_within(&mut quick, Duration::from_secs(10));
        assert!(status.is_some_and(|status| status.success()));
        assert_eq!(
            status_within(
                &mut Command::new("/no/such/program"),
                Duration::from_secs(1)
            ),
            None
        );
    }

    #[test]
    fn a_newer_version_is_one_that_sorts_after_this_one() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.10.0", "0.9.0"), "numeric, not lexical");
        assert!(is_newer("0.2", "0.1.9"), "a short version still compares");

        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(
            !is_newer("0.1.0+build", "0.1.0"),
            "a build suffix is not a release"
        );
    }

    /// A pre-release is never offered as an upgrade, however its core compares —
    /// its own doc comment said so while the code stripped the `-` and compared
    /// anyway (finding 68).
    #[test]
    fn a_pre_release_latest_is_not_an_upgrade() {
        assert!(!is_newer("0.2.0-beta.1", "0.1.0"));
        assert!(!is_newer("1.0.0-rc.1", "0.9.0"));
        assert!(!is_newer("v0.2.0-alpha", "0.1.0"));
        // A pre-release of the same core is not one either.
        assert!(!is_newer("0.1.0-beta", "0.1.0"));
        // Build metadata on its own still compares by core.
        assert!(is_newer("0.2.0+build.7", "0.1.0"));
    }

    /// Whatever npm prints that is not a version, nobody is nagged about. This
    /// decides whether to interrupt somebody, so the ambiguous answer is
    /// silence.
    #[test]
    fn nonsense_is_never_newer() {
        for latest in ["", "latest", "not a version", "0.x.0", "npm ERR! 404"] {
            assert!(!is_newer(latest, "0.1.0"), "{latest:?}");
        }
        assert!(!is_newer("0.2.0", "not a version"));
    }

    /// A cache is two fields and survives a round trip; anything else parses
    /// to nothing rather than to a wrong answer.
    #[test]
    fn the_cache_round_trips_and_refuses_rubbish() {
        let cached = Cached {
            version: "0.2.0".into(),
            checked: 1_700_000_000,
        };
        let dir = crate::scratch::root("release-cache-round-trip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("latest.json");
        write_cache(&path, Channel::Npm, &cached);
        assert_eq!(read_cache(&path, Channel::Npm), Some(cached));

        for text in ["", "{}", "not json", "{\"version\":\"0.2.0\"}", "[]"] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(read_cache(&path, Channel::Npm), None, "{text:?}");
        }
    }

    #[test]
    fn a_days_old_answer_is_refreshed_and_a_future_one_is_not_trusted() {
        let day = MAX_AGE.as_secs();
        let cached = |checked| Cached {
            version: "0.2.0".into(),
            checked,
        };
        assert!(!cached(1000).stale(1000));
        assert!(!cached(1000).stale(1000 + day - 1));
        assert!(cached(1000).stale(1000 + day));
        assert!(
            cached(9999).stale(1000),
            "a cache stamped after now is a clock that moved, not a fresh answer"
        );
    }

    #[test]
    fn handover_carries_the_compiling_binarys_actual_version() {
        let command = upgraded_command(Path::new("spoolway"), &["update".into()]);
        let carried = command
            .get_envs()
            .find(|(key, _)| *key == ENV_UPGRADED)
            .and_then(|(_, value)| value)
            .and_then(|value| value.to_str());
        assert_eq!(carried, Some(current()));
    }

    /// The bug this task exists to close: on the one run somebody actually
    /// asked for the upgrade, `upgrade()` still answered from the cache. A
    /// stubbed `live` closure stands in for npm answering `0.3.0`, and a
    /// `cached` closure that panics if called proves the cache is bypassed
    /// outright rather than merely happening not to disagree — the failure
    /// this repro predicted was `upgrade()` never asking npm at all.
    ///
    /// Exercised against [`decide_upgrade`] rather than [`upgrade`] itself,
    /// and with no process environment touched at all: no cache file to
    /// seed, no fake `npm` to place on `PATH`, and nothing to race against
    /// another test's own use of the same ambient variables — the class of
    /// problem a stubbed lookup exists to avoid.
    /// A version this binary can never be, whatever it is bumped to: its own
    /// major component plus one.
    ///
    /// A stub that hands [`decide_upgrade`] a literal "newer" version is
    /// coupled to the crate's own version number and goes off the moment the
    /// crate reaches it. `upgrade_asks_npm_rather_than_trusting_a_cache_that_
    /// has_not_caught_up` hardcoded `"0.3.0"`, which was newer than
    /// [`current`] right up until the release commit that bumped
    /// `Cargo.toml` to 0.3.0 — at which point [`is_newer`] correctly
    /// answered false, `decide_upgrade` correctly answered
    /// [`Upgrade::Current`], and the test failed on the very release it was
    /// meant to be clearing the way for. No gate before that bump could see
    /// it, because every one of them ran at 0.2.0 where the literal was
    /// genuinely newer.
    ///
    /// Deriving it keeps the same test meaning "npm answered with something
    /// newer" at every future version instead of at one.
    fn newer_than_current() -> String {
        // The same core `is_newer` compares on: any pre-release or build
        // suffix dropped, then plain numeric components.
        let core = current().trim_start_matches('v');
        let core = core.split(['-', '+']).next().unwrap_or(core);
        let mut parts: Vec<u64> = core
            .split('.')
            .map(|part| {
                part.parse()
                    .expect("the crate's own version is numeric components")
            })
            .collect();
        parts[0] += 1;
        parts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }

    /// The property the helper exists for, asserted rather than assumed — if
    /// this ever stops holding, every stub built on it is quietly testing
    /// nothing.
    #[test]
    fn the_stub_version_is_newer_than_this_binarys_own() {
        assert!(
            is_newer(&newer_than_current(), current()),
            "{} must be newer than {}",
            newer_than_current(),
            current()
        );
    }

    #[test]
    fn upgrade_asks_npm_rather_than_trusting_a_cache_that_has_not_caught_up() {
        let answered = newer_than_current();
        let result = decide_upgrade(
            Channel::Npm,
            Path::new("/does/not/exist/lock"),
            || Some(newer_than_current()),
            || panic!("the cache must not be consulted once npm has answered"),
            |_| None,
        );

        // The stubbed install fails, so this is `Unmanaged` rather than
        // `Installed` — the point is which version it names: npm's live
        // answer, not a cache that has not caught up.
        assert_eq!(result, Upgrade::Unmanaged(answered));
    }

    /// The other half of the same decision: a lookup that fails — offline, no
    /// npm, npm itself erroring — still has to fall back to whatever the
    /// cache knows, rather than telling `update` there is nothing to do.
    #[test]
    fn a_failed_live_lookup_falls_back_to_the_cache() {
        let result = decide_upgrade(
            Channel::Npm,
            Path::new("/does/not/exist/lock"),
            || None,
            || Some("0.4.0".to_string()),
            |_| None,
        );
        assert_eq!(result, Upgrade::Unmanaged("0.4.0".to_string()));
    }

    /// The third branch, and the one that keeps the cache from getting a
    /// second vote: npm answering with nothing newer settles the question on
    /// its own. Falling back to a cache here would let a stale entry claim an
    /// upgrade npm has just said is not there, so the `cached` closure panics
    /// if it is ever reached.
    #[test]
    fn npm_answering_with_nothing_newer_ends_it_without_the_cache() {
        let result = decide_upgrade(
            Channel::Npm,
            Path::new("/does/not/exist/lock"),
            || Some(current().to_string()),
            || panic!("the cache must not get a second vote once npm has answered"),
            |_| None,
        );
        assert_eq!(result, Upgrade::Current);
    }

    /// The bug this task exists to close: before the fix, `decide_upgrade`
    /// called `live()` directly, with nothing bounding how long it may run —
    /// exactly what turns npm's own retries against a registry it cannot
    /// reach into a command that never returns (`spoolway update` hanging on
    /// a machine with no network). Now it runs `live()` through [`bounded`]
    /// instead. A closure that blocks far longer than any reasonable deadline
    /// stands in for that hang, the same way the task's own acceptance
    /// criteria describe: "the reproduction drives the deadline against a
    /// stubbed lookup that never returns". Falling back to the cache's answer
    /// once the deadline passes is what `update` should do instead of sitting
    /// through the rest of the wait.
    #[test]
    fn a_live_lookup_that_never_answers_gives_up_after_a_bounded_wait() {
        let start = std::time::Instant::now();
        let result = decide_upgrade(
            Channel::Npm,
            Path::new("/does/not/exist/lock"),
            || {
                std::thread::sleep(Duration::from_secs(20));
                Some("0.9.9".to_string())
            },
            || Some("0.4.0".to_string()),
            |_| None,
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "decide_upgrade waited {:?} on a lookup that never answers — it must give up after \
             a short bounded wait instead of hanging the command",
            start.elapsed()
        );
        // A lookup that gave up is exactly a failed lookup: the cache's
        // answer is what `update` should fall back to.
        assert_eq!(result, Upgrade::Unmanaged("0.4.0".to_string()));
    }

    /// A standalone binary asks nothing: none of the lookup, the cache or the
    /// install runs, so each panics if reached. Before the channel was an
    /// argument, `decide_upgrade` ran `live` first and only then looked at the
    /// channel, so these closures were called.
    #[test]
    fn a_standalone_binary_never_looks_anything_up() {
        let result = decide_upgrade(
            Channel::Other,
            Path::new("/does/not/exist/lock"),
            || panic!("a standalone binary must not run the npm lookup"),
            || panic!("a standalone binary must not read the cache"),
            |_| panic!("a standalone binary must not install anything"),
        );
        assert_eq!(result, Upgrade::Standalone);
    }

    /// A standalone binary ignores a fresh cache that names a newer release,
    /// which an npm binary reports from the same file. Without the channel
    /// check in [`newer_at`] the `Other` answer would be the cached version.
    #[test]
    fn a_standalone_binary_ignores_a_cache_an_npm_binary_reads() {
        let dir = crate::scratch::root("release-standalone-cache");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("latest.json");
        write_cache(
            &path,
            Channel::Npm,
            &Cached {
                version: newer_than_current(),
                checked: now(),
            },
        );
        assert_eq!(
            newer_at(Channel::Npm, Some(path.clone())),
            Some(newer_than_current())
        );
        assert_eq!(newer_at(Channel::Other, Some(path)), None);
    }

    /// The state file is the machine's, not the project's: every checkout runs
    /// the same binary, so the answer is looked up once per machine.
    #[test]
    fn the_cache_sits_beside_the_project_registry() {
        let previous = std::env::var_os("XDG_STATE_HOME");
        crate::platform::set_test_env("XDG_STATE_HOME", "/tmp/spoolway-state-test");
        let path = cache_path().unwrap();
        assert_eq!(
            path,
            PathBuf::from("/tmp/spoolway-state-test/spoolway/latest.json")
        );
        assert_eq!(
            path.parent(),
            crate::usage::registry::path().unwrap().parent(),
            "beside projects.json, for the same reason"
        );
        match previous {
            Some(value) => crate::platform::set_test_env("XDG_STATE_HOME", value),
            None => crate::platform::remove_test_env("XDG_STATE_HOME"),
        }
    }

    /// clap prints `spoolway <version>`, and the token wanted is the last word
    /// of it. Nothing here decides what a version looks like — [`is_newer`]
    /// does that — so a line with no words at all is the only `None`.
    #[test]
    fn a_reported_version_is_the_last_word_of_the_first_line_that_has_one() {
        assert_eq!(
            reported_version("spoolway 0.5.0\n").as_deref(),
            Some("0.5.0")
        );
        // A wrapper that prints a blank line first, and one that prints more
        // under the version: only the first line with a word on it is read.
        assert_eq!(
            reported_version("\n\nspoolway 1.2.3\nbuilt from abc123\n").as_deref(),
            Some("1.2.3")
        );
        // A name with no version is handed on as it stands; `is_newer` is what
        // decides it is not an upgrade.
        assert_eq!(reported_version("spoolway\n").as_deref(), Some("spoolway"));
        assert!(!is_newer("spoolway", current()));
        assert_eq!(reported_version(""), None);
        assert_eq!(reported_version("   \n\t\n"), None);
    }

    /// How many times a stub reader has been called, so a test can prove the
    /// cache spawned one reading rather than one per call.
    static STUB_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn stub_reader() -> Option<String> {
        STUB_READS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some("99.0.0".to_string())
    }

    /// A reader that outlives any deadline a test would wait on. Never joined,
    /// the same way [`bounded`]'s own thread is not.
    fn stalled_reader() -> Option<String> {
        std::thread::sleep(Duration::from_secs(30));
        Some("99.0.0".to_string())
    }

    /// Poll `done` for up to two seconds. The reading lands on another thread,
    /// so there is nothing to join — and a fixed sleep would either be flaky
    /// or slower than it has to be.
    fn within_two_seconds(done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        done()
    }

    /// The board asks this once a second, so the reading happens behind the
    /// call and once per interval — never on the caller's thread, and never
    /// once per redraw.
    #[test]
    fn a_path_reading_lands_behind_the_call_and_is_taken_once_per_interval() {
        static CELL: OnceLock<Mutex<Reading>> = OnceLock::new();
        let cell = CELL.get_or_init(|| Mutex::new(Reading::default()));
        let ask = || {
            cached_reading(
                cell,
                stub_reader,
                Duration::from_secs(600),
                Duration::from_secs(5),
            )
        };

        // Nothing has been read yet, so the first call has nothing to say —
        // the first frame of a run draws no hint, by design.
        assert_eq!(ask(), None);
        assert!(within_two_seconds(|| ask().as_deref() == Some("99.0.0")));

        // Every call since the first landed inside the interval, so none of
        // them started a second reading.
        for _ in 0..20 {
            assert_eq!(ask().as_deref(), Some("99.0.0"));
        }
        assert_eq!(STUB_READS.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// An executable that never answers costs the hint, not the board: the
    /// call returns at once, and the deadline hands the in-flight slot back so
    /// the next interval can try again rather than waiting on it forever.
    #[test]
    fn a_stalled_executable_loses_its_turn_rather_than_holding_the_slot() {
        static CELL: OnceLock<Mutex<Reading>> = OnceLock::new();
        let cell = CELL.get_or_init(|| Mutex::new(Reading::default()));

        let started = Instant::now();
        let answer = cached_reading(
            cell,
            stalled_reader,
            Duration::from_secs(600),
            Duration::from_millis(100),
        );
        assert_eq!(answer, None);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the call waited on the reading: {:?}",
            started.elapsed()
        );

        // The reader is still sleeping, but its turn is over: the slot is free
        // and the failure is stamped, which is what makes the next interval a
        // retry instead of a permanent silence.
        assert!(within_two_seconds(|| {
            let state = cell.lock().unwrap_or_else(|p| p.into_inner());
            !state.reading && state.read_at.is_some() && state.version.is_none()
        }));
    }
}

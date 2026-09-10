//! Whether a newer spoolway has been published, and how to take it.
//!
//! Two halves of one question, kept in one module because they must agree.
//! The **notice** is a line telling somebody a release is out; the **install**
//! is what happens when they act on it. Ship those decoupled and the banner
//! advises a command that refuses — which is exactly the bug pi carries
//! (earendil-works/pi#5607, where a Nix install is told to run `pi update` and
//! gets "cannot self-update this installation"). So [`Channel`] is resolved
//! once and gates both: a binary that cannot be upgraded from here is told
//! that, in the same breath as the version.
//!
//! Nothing here opens a socket. spoolway ships no HTTP client and gains none:
//! the binary arrives through npm, so npm is asked what the latest version is
//! and npm is what installs it — integrity, platform selection and the atomic
//! swap are all already its job, and hand-rolling a rename over a running
//! executable is the one thing [`crate::config`]'s own documentation warns
//! kills a dispatcher mid-pass.
//!
//! The check on the command path reads a **cache and only a cache**. A lookup
//! older than [`MAX_AGE`] spawns a detached child that refreshes it for the
//! *next* command; this one carries on. That is the whole reason a version
//! check can sit in front of every subcommand without making any of them
//! slower, and it means the worst case is an answer a day stale rather than a
//! command that hangs on a plane.
//!
//! Every failure in here is swallowed, and deliberately. A machine offline, an
//! npm that was never installed, a registry that answers slowly — none of that
//! is worth failing a command over, and a version check that can break `queue
//! add` is a version check nobody should have shipped.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;

/// The npm package the binary ships as: the unscoped wrapper a person installs,
/// which resolves one `@spoolway/<plat>` package underneath.
pub const PACKAGE: &str = "spoolway";

/// How stale a cached answer may be before a refresh is spawned behind the
/// command. A day: releases are not hourly, and the cost of being wrong is a
/// notice that arrives one command late.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Turns the check off for one machine, whatever a project's config says.
///
/// The per-machine half of `update.check`, and the same escape hatch pi spells
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
    /// Anything else — `cargo install`, an unpacked release archive, a binary
    /// copied onto `PATH` by hand, a Nix store path. Upgradable, just not from
    /// here, and saying so is the whole point of the variant.
    Other,
}

impl Channel {
    /// Where this binary lives, resolved once.
    ///
    /// Keyed off the path rather than off anything npm sets in the
    /// environment, because a lane's `spoolway report` is launched by the
    /// dispatcher and inherits none of an install's environment — the path is
    /// the only signal that survives.
    pub fn detect() -> Channel {
        match std::env::current_exe() {
            Ok(exe) => Channel::of(&exe),
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
    pub fn of(exe: &Path) -> Channel {
        let parts: Vec<String> = exe
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        let global = parts
            .windows(2)
            .any(|pair| pair[1] == "node_modules" && (pair[0] == "lib" || pair[0] == "npm"));
        match global {
            true => Channel::Npm,
            false => Channel::Other,
        }
    }
}

/// What a cached lookup left behind.
///
/// Hand-rolled rather than serde-derived: this file is written by one process
/// and read by another, and keeping it to two flat fields means a partially
/// written or hand-mangled file is a parse that returns `None` rather than a
/// dependency between two spoolway versions' struct shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    pub version: String,
    /// Unix seconds at which the lookup was made.
    pub checked: u64,
}

impl Cached {
    fn parse(text: &str) -> Option<Cached> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        Some(Cached {
            version: value.get("version")?.as_str()?.trim().to_string(),
            checked: value.get("checked")?.as_u64()?,
        })
    }

    fn render(&self) -> String {
        format!(
            "{{\"version\":\"{}\",\"checked\":{}}}\n",
            self.version, self.checked
        )
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
/// one — and a refresh spawned behind the command when the cache is old.
///
/// Reads one file. Everything that can go wrong — no home directory, no cache,
/// a corrupt cache, a clock that moved — comes back `None`.
pub fn newer() -> Option<String> {
    let path = cache_path()?;
    let cached = std::fs::read_to_string(&path)
        .ok()
        .as_deref()
        .and_then(Cached::parse);

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
/// own update-notifier uses, and the only portable one: it needs a process that
/// outlives this command, runs `npm view`, and writes a file, which is a shell
/// script on Unix and something else on Windows unless the program doing it is
/// this one.
///
/// Through `libc::setsid()` on Unix, called in the child between fork and
/// exec, for the reason [`crate::headless`] detaches lanes the same way: a
/// child that merely has no terminal on its file descriptors is still in this
/// session, and gets `SIGHUP` when the terminal closes. `spoolway queue list`
/// in a window somebody shuts a second later is exactly the case — the lookup
/// takes seconds and the window does not wait for it. Without a session of
/// its own the cache would never fill on such a machine, and the notice would
/// never appear.
///
/// On Windows there is no such syscall, and none of this module's other
/// Windows work needs one: a plain child is better than no child, and still
/// refreshes the cache whenever this command was not the last thing a
/// terminal did.
///
/// Nothing is waited on, so the child is reaped by init once this process
/// exits. A failure to spawn at all is the same as a failure to look up:
/// silence, and the next command tries again.
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

/// Ask npm what the latest published version is, and write it to the cache.
///
/// The whole body of the hidden subcommand. Runs in a process nobody is
/// waiting on, so it may block on the network for as long as npm takes.
pub fn refresh() {
    let Some(path) = cache_path() else { return };
    let Some(version) = published() else {
        // Nothing published, no npm, no network. The cache is still stamped,
        // so a machine that cannot answer is asked once a day rather than on
        // every command — which is the difference between a quiet failure and
        // a fork on every invocation.
        let stamp = Cached {
            version: current().to_string(),
            checked: now(),
        };
        write_cache(&path, &stamp);
        return;
    };
    write_cache(
        &path,
        &Cached {
            version,
            checked: now(),
        },
    );
}

fn write_cache(path: &Path, cached: &Cached) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = crate::task::write_atomic(path, cached.render());
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

/// Run npm, and hand back stdout when it succeeded.
fn npm(args: &[&str]) -> Option<String> {
    let out = Command::new(npm_program())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    match out.status.success() {
        true => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        false => None,
    }
}

/// `npm` is a shell script on Unix and a `.cmd` on Windows, and the `.cmd` is
/// not something `CreateProcess` will run directly.
fn npm_program() -> &'static str {
    match cfg!(windows) {
        true => "npm.cmd",
        false => "npm",
    }
}

/// Everything that decides whether the line is printed, gathered so the
/// decision is one pure function.
///
/// Assembled by the caller because every field comes from somewhere different
/// — the environment, the parsed flags, the config, the terminal — and a
/// function that reads all four itself cannot be tested against any of them.
#[derive(Debug, Clone, Copy)]
pub struct Audience {
    /// Set by the dispatcher on every lane it launches. A lane that reads
    /// "Run spoolway update" is a lane that runs it, mid-step, in a worktree.
    pub in_lane: bool,
    /// `--json` was asked for, so something is parsing this.
    pub machine_readable: bool,
    /// stderr is a terminal.
    pub tty: bool,
    /// `update.check`, from the project's config.
    pub enabled: bool,
    /// `SPOOLWAY_SKIP_VERSION_CHECK` is set.
    pub skipped: bool,
}

impl Audience {
    /// Is there a person here who wants to be told?
    pub fn wants_notice(&self) -> bool {
        !self.in_lane && !self.machine_readable && self.tty && self.enabled && !self.skipped
    }
}

/// Print the one line, if there is anybody to print it to.
///
/// On stderr, deliberately: no command's stdout gains a line, so nothing that
/// reads spoolway's output has to learn about this at all.
pub fn notify(audience: Audience) {
    if !audience.wants_notice() {
        return;
    }
    let Some(version) = newer() else { return };
    eprintln!("{}", line(&version, Channel::detect()));
}

/// The notice itself.
///
/// Two spellings, chosen by the same [`Channel`] that decides what `update`
/// will do — so the line never advises a command that would refuse.
pub fn line(version: &str, channel: Channel) -> String {
    match channel {
        Channel::Npm => format!("Update available: {version}. Run \"spoolway update\""),
        Channel::Other => format!(
            "Update available: {version}. This binary was not installed by npm, so upgrade \
             it the way you installed it"
        ),
    }
}

/// What an attempt to take the newer binary did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upgrade {
    /// Nothing newer is known, or this process is already the upgraded one.
    Current,
    /// Installed, and the caller should hand over to the new binary.
    Installed(String),
    /// A newer version exists and this install cannot take it from here.
    Unmanaged(String),
    /// A dispatcher is running, so the binary it is executing is not being
    /// rewritten under it.
    Dispatching(String, u32),
}

/// Take the newer release, when there is one and it is safe to.
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
    let Some(version) = newer() else {
        return Upgrade::Current;
    };
    if Channel::detect() == Channel::Other {
        return Upgrade::Unmanaged(version);
    }
    // Refuse rather than rely on a remembered release rule: a running dispatcher
    // is executing the file npm is about to replace, and a half-written binary is
    // a dispatcher that dies mid-pass.
    if let Ok(Some(pid)) = crate::lock::Lock::holder(lock_file) {
        return Upgrade::Dispatching(version, pid);
    }

    // `--ignore-scripts` costs nothing here and takes lifecycle scripts out of
    // the upgrade path: the packaging deliberately has no postinstall, which
    // is what makes the npm install work under `--ignore-scripts` in the first
    // place; the wrapper contains no lifecycle install script.
    let target = format!("{PACKAGE}@{version}");
    match npm(&["install", "-g", &target, "--ignore-scripts"]) {
        Some(_) => Upgrade::Installed(version),
        None => Upgrade::Unmanaged(version),
    }
}

/// Hand over to the binary that was just installed, so the files it writes are
/// its own.
///
/// Every file `spoolway update` writes is generated from the running binary's
/// `include_str!` tables, so a process that installs 0.2.0 and carries on is a
/// process writing 0.1.0's files under a 0.2.0 install — the exact drift the
/// command exists to close.
///
/// Resolved through `PATH` rather than through `current_exe`, because
/// `current_exe` on Unix is the *inode* this process is executing and npm has
/// just written a different file at the name.
///
/// `expect_version` is what the install just put down. `PATH` is not proof that
/// the name now resolves to it — under `npx` or `node_modules/.bin` the first
/// `spoolway` on `PATH` is still the old binary, which would run the file work
/// with the old `include_str!` tables and exit 0. So the resolved binary is
/// asked its version *before* it is handed the work: a mismatch refuses here,
/// rather than letting the wrong binary rewrite the project's files and only
/// then reporting the handover went astray.
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
             installed {expect_version} — a different install (an `npx` or project-local \
             copy); install spoolway {expect_version} the way this one was installed",
            program.display()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this module exists not to have: the notice and the installer
    /// must read the same signal, or somebody is advised to run a command that
    /// refuses. pi#5607 is that bug, shipped.
    #[test]
    fn the_notice_and_the_installer_agree_on_the_channel() {
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

        // And what each is told matches what each can do.
        assert!(line("0.2.0", Channel::of(npm)).contains("Run \"spoolway update\""));
        for outside in [hand, nix, local, npx] {
            let said = line("0.2.0", Channel::of(outside));
            assert!(
                !said.contains("spoolway update"),
                "an install that cannot self-update must not be told to: {said}"
            );
            assert!(said.contains("the way you installed it"), "{said}");
        }
    }

    /// The sentence is dictated, and a version number is the only thing in it
    /// that varies.
    #[test]
    fn the_npm_line_is_the_one_sentence() {
        assert_eq!(
            line("0.2.0", Channel::Npm),
            "Update available: 0.2.0. Run \"spoolway update\""
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

    /// Every gate, one at a time: each is on its own sufficient to silence the
    /// line, and the lane gate is the one that matters most.
    #[test]
    fn only_a_person_at_a_terminal_is_told() {
        let person = Audience {
            in_lane: false,
            machine_readable: false,
            tty: true,
            enabled: true,
            skipped: false,
        };
        assert!(person.wants_notice());

        assert!(
            !Audience {
                in_lane: true,
                ..person
            }
            .wants_notice(),
            "a lane told to update will update, mid-step"
        );
        assert!(
            !Audience {
                machine_readable: true,
                ..person
            }
            .wants_notice()
        );
        assert!(
            !Audience {
                tty: false,
                ..person
            }
            .wants_notice()
        );
        assert!(
            !Audience {
                enabled: false,
                ..person
            }
            .wants_notice()
        );
        assert!(
            !Audience {
                skipped: true,
                ..person
            }
            .wants_notice()
        );
    }

    /// A cache is two fields and survives a round trip; anything else parses
    /// to nothing rather than to a wrong answer.
    #[test]
    fn the_cache_round_trips_and_refuses_rubbish() {
        let cached = Cached {
            version: "0.2.0".into(),
            checked: 1_700_000_000,
        };
        assert_eq!(Cached::parse(&cached.render()), Some(cached));

        for text in ["", "{}", "not json", "{\"version\":\"0.2.0\"}", "[]"] {
            assert_eq!(Cached::parse(text), None, "{text:?}");
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
}

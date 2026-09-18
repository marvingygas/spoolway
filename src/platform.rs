//! The handful of places spoolway's behaviour forks on the platform.
//!
//! Gathered here rather than left where each one was needed, because they are
//! the same decision made twice — which shell, which home — and two
//! subsystems had already grown copies that could drift apart without
//! anything failing.
//!
//! spoolway builds and runs on Linux and macOS only, both POSIX, so there is
//! one dialect of shell to quote for and one way to find a home directory.
//! What is left here is mostly the test scaffolding shared by every module
//! whose tests need a scratch `$HOME` or a swapped environment variable, plus
//! [`PathExt`]'s one real platform question: what a resolved path compares
//! equal to.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// This user's home directory, from `$HOME`. `None` if it is unset or empty
/// — an empty `HOME` is not a home directory, and taking it as one would put
/// every lane's policy paths at the filesystem root.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(home) = test_home::current() {
        return Some(home);
    }
    non_empty("HOME").map(PathBuf::from)
}

/// An environment variable that is set *and* has something in it.
fn non_empty(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}

/// A repo-relative path, as a string.
///
/// These strings do not stay on the machine that produced them: they go into
/// task files and briefings, which are committed and then read by a lane on
/// whatever machine picks the task up — Linux and macOS already agree on the
/// separator, so nothing is rewritten, but the call sites all go through one
/// function rather than each spelling `strip_prefix().display()` itself.
pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Make a value safe to sit *inside* an already-open single-quoted `sh`
/// string.
///
/// Split out of [`quote`] so the escape itself — the one thing standing
/// between a prompt-supplied string and a shell — has exactly one
/// implementation, whoever ends up calling it. A single-quoted POSIX string
/// cannot contain a quote at all, so the string is closed, an escaped quote
/// emitted, and a new one opened.
fn escape_single_quoted(value: &str) -> String {
    value.replace('\'', r"'\''")
}

/// Quote a value so it arrives as one literal argument.
///
/// Single quotes, because nothing expands inside them — a task id or a
/// branch name carrying `$`, `%` or a backtick is inert. Only the escape for
/// an embedded quote needs any care.
pub(crate) fn quote(value: &str) -> String {
    format!("'{}'", escape_single_quoted(value))
}

/// One assignment, in `sh` syntax — what [`env_export`] and
/// [`env_export_lines`] both build on, so the quoting is written once
/// whichever shape the caller needs.
fn env_assignment(key: &str, value: &str) -> String {
    format!("{key}={}", quote(value))
}

/// The lane's environment, as one line.
///
/// One command, not one per variable: consecutive sends race the shell's
/// readiness, and a line arriving while the previous one is still being read
/// is delivered as a paste — bracketed-paste markers and all — which sets no
/// variable and leaves the lane quietly missing it.
pub fn env_export(env: &BTreeMap<String, String>) -> String {
    let pairs = env
        .iter()
        .map(|(key, value)| env_assignment(key, value))
        .collect::<Vec<_>>()
        .join(" ");
    format!("export {pairs}")
}

/// The same environment as [`env_export`], one assignment per line rather
/// than joined onto one — for a file [`crate::mux::Herdr::hand_environment`]
/// writes rather than types into a pane, where `env_export`'s own reason for
/// staying on one line does not apply: nothing about a file races a shell's
/// paste-readiness, and one `export` per line is what the task's own mockup
/// draws and what a person opening the file actually reads.
pub fn env_export_lines(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(key, value)| format!("export {}", env_assignment(key, value)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The one command that reads an environment [`env_export_lines`] wrote to a
/// file, rather than typing it into a pane directly.
///
/// `. <path>` runs `path`'s lines in the calling shell rather than a subshell
/// that would take the exports nowhere, and needs nothing past that one
/// line — which is the whole point. A herdr pane cuts an `export` line
/// mid-value past some length and then waits forever on the unterminated
/// quote it left behind; a `.` command naming a file is short no matter how
/// large the environment inside it is.
pub fn source_command(path: &Path) -> String {
    format!(". {}", quote(&path.display().to_string()))
}

/// The one export that is not a plain value: the directory is quoted, but the
/// existing `PATH` has to survive as something the shell still expands.
pub fn path_export(prefix: &Path) -> String {
    let dir = quote(&prefix.display().to_string());
    format!("export PATH={dir}:\"$PATH\"")
}

/// A [`Command`] that runs `script` through `sh -c`, which takes the script
/// as one argv entry and is the end of it.
pub fn shell_command(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(script);
    command
}

/// Resolve a program name against a `PATH`, the way a shell would.
pub fn which(program: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    // A path, not a name, is taken as given.
    if program.contains('/') {
        let direct = PathBuf::from(program);
        return direct.is_file().then_some(direct);
    }
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sh` is on every machine this runs on — used rather than a fixture
    /// binary because the interesting behaviour is the PATH walk itself, not
    /// any one binary's presence.
    #[test]
    fn which_finds_a_real_binary_on_path() {
        let path = std::env::var_os("PATH").unwrap();
        assert!(which("sh", &path).is_some());
        assert!(which("spoolway-nothing-named-this", &path).is_none());
    }

    #[test]
    fn a_home_is_found_the_way_the_platform_provides_one() {
        assert!(
            home_dir().is_some(),
            "no home directory on {}",
            std::env::consts::OS
        );
    }

    #[test]
    fn shell_quoting_survives_an_embedded_quote() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn the_source_command_is_a_short_dot_command() {
        assert_eq!(
            source_command(Path::new("/repo/.spoolway/commands/demo.env")),
            ". '/repo/.spoolway/commands/demo.env'"
        );
        assert_eq!(
            source_command(Path::new("/it's here/x.env")),
            r". '/it'\''s here/x.env'"
        );
    }

    #[test]
    fn env_export_lines_is_one_assignment_per_line() {
        let env = BTreeMap::from([
            ("SPOOLWAY_STEP".to_string(), "implement".to_string()),
            ("SPOOLWAY_TASK".to_string(), "add-endpoint".to_string()),
        ]);
        assert_eq!(
            env_export_lines(&env),
            "export SPOOLWAY_STEP='implement'\nexport SPOOLWAY_TASK='add-endpoint'"
        );
    }

    #[test]
    fn the_path_export_quotes_the_directory_but_still_expands_path() {
        assert_eq!(
            path_export(Path::new("/repo/.spoolway/bin")),
            "export PATH='/repo/.spoolway/bin':\"$PATH\""
        );
        // A path a person would never choose, and which would otherwise end
        // the quoting early and run whatever came after it.
        assert_eq!(
            path_export(Path::new("/it's here/bin")),
            "export PATH='/it'\\''s here/bin':\"$PATH\""
        );
    }

    #[test]
    fn the_lane_environment_is_one_line() {
        let env = BTreeMap::from([
            ("SPOOLWAY_STEP".to_string(), "implement".to_string()),
            ("SPOOLWAY_TASK".to_string(), "add-endpoint".to_string()),
        ]);

        assert_eq!(
            env_export(&env),
            "export SPOOLWAY_STEP='implement' SPOOLWAY_TASK='add-endpoint'"
        );
        assert!(!env_export(&env).contains('\n'));
    }

    #[test]
    fn a_hostile_value_cannot_break_out_of_its_quoting() {
        // A task id is checked, but a branch name or a prompt-supplied reason
        // reaches this unfiltered.
        let hostile = "x'; rm -rf /; '";
        let env = BTreeMap::from([("SPOOLWAY_TASK".to_string(), hostile.to_string())]);

        // Every quote in the payload is escaped, so none of them closes the
        // string the value sits in and the semicolons stay data.
        assert_eq!(
            env_export(&env),
            r"export SPOOLWAY_TASK='x'\''; rm -rf /; '\'''"
        );
    }
}

/// Whether the person has asked this process to stop.
///
/// A dispatcher spends nearly all its life asleep between passes, so the run
/// almost always ends with a `Ctrl-C` landing in that sleep. Left to itself
/// that kills the process where it stands, which is fine for the loop and not
/// fine for the run's own accounting — see
/// `crate::dispatch::Dispatcher::sweep_on_stop`, which banks an interrupted
/// lane's spend and forgives its launch counter before the process exits.
///
/// So the signal is caught, turned into a flag, and the loop is left to notice
/// it and unwind normally. A handler may do almost nothing safely — storing to
/// an `AtomicBool` is on the short list of what is allowed — so it does exactly
/// that and no more.
///
/// **A second `Ctrl-C` is not caught.** The handler is installed once and the
/// default is restored the moment it fires, so someone who has decided the
/// cleanup itself is hanging gets the usual kill from pressing it again.
pub mod stop {
    use std::sync::atomic::{AtomicBool, Ordering};

    static ASKED: AtomicBool = AtomicBool::new(false);

    /// Catch the interrupt for the rest of this process's life.
    pub fn catch_interrupt() {
        // SAFETY: `libc::signal` with a handler that only stores to a static
        // `AtomicBool`. Nothing here allocates, locks or reenters the runtime,
        // which is the whole of what makes a handler async-signal-safe.
        unsafe {
            extern "C" fn on_interrupt(_: libc::c_int) {
                // Back to the default first, so a second press kills outright
                // rather than setting a flag that is already set.
                unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
                super::stop::asked_for();
            }
            libc::signal(
                libc::SIGINT,
                on_interrupt as *const () as libc::sighandler_t,
            );
        }
    }

    /// Record that a stop was asked for. Public for the handler above, and for
    /// a test that wants the loop to unwind without raising a real signal.
    pub fn asked_for() {
        ASKED.store(true, Ordering::SeqCst);
    }

    /// Has one been asked for?
    pub fn asked() -> bool {
        ASKED.load(Ordering::SeqCst)
    }
}

/// Takes the terminal for as long as the board is up, and gives it back.
///
/// Held by [`crate::status::Board`] across its whole life: constructed where
/// the board is, restored in `Drop`, so both ways a dispatch loop ends — the
/// queue emptying, and `ctrl-c`, which unwinds through [`stop::catch_interrupt`]
/// rather than killing the process where it stands — reach the same restore
/// once the board holding this goes out of scope.
pub struct TermGuard {
    original: Option<libc::termios>,
    /// An inert guard hides nothing and restores nothing. Test-only: a `Board`
    /// built in a test must not take the process's real terminal raw —
    /// parallel tests each restore in their own order, and the last one to run
    /// decides whether the developer's shell is left without echo (finding 53).
    inert: bool,
}

impl TermGuard {
    /// Hide the cursor and put stdin in raw-enough mode: `ECHO` and `ICANON`
    /// off so a keystroke is discarded rather than echoed under the footer or
    /// held for a line that never comes, `ISIG` deliberately kept so
    /// `ctrl-c` still raises `SIGINT` the way [`stop::catch_interrupt`]
    /// expects to catch it.
    pub fn new() -> TermGuard {
        hide_cursor();
        TermGuard::take()
    }
}

impl Default for TermGuard {
    fn default() -> TermGuard {
        TermGuard::new()
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        if self.inert {
            return;
        }
        // Drained before the mode is restored: anything typed while the tty
        // was deaf is not a command waiting for the shell prompt that lands
        // under the last frame the moment this returns it.
        drain_stdin();
        self.restore();
        show_cursor();
    }
}

impl TermGuard {
    /// A guard that touches nothing. See the `inert` field.
    #[cfg(test)]
    pub fn inert() -> TermGuard {
        TermGuard {
            original: None,
            inert: true,
        }
    }

    fn take() -> TermGuard {
        TermGuard {
            original: raw_mode(),
            inert: false,
        }
    }

    fn restore(&self) {
        if let Some(original) = self.original {
            // SAFETY: `tcsetattr` on stdin's descriptor, restoring exactly
            // what `raw_mode` read off it with `tcgetattr` — the same
            // plain-old-data struct, unmodified.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original);
            }
        }
    }
}

fn hide_cursor() {
    print!("\x1b[?25l");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn show_cursor() {
    print!("\x1b[?25h");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Put stdin in raw-enough mode and hand back what it was, so [`TermGuard`]
/// can restore it exactly. `None` wherever there is nothing to restore —
/// stdin is not a terminal at all, which is true of every run this is still
/// constructed for but that redirects its input.
fn raw_mode() -> Option<libc::termios> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return None;
    }
    // SAFETY: `termios` is a plain-old-data struct and `tcgetattr`/`tcsetattr`
    // are ordinary syscalls against a descriptor — stdin — that is open for
    // the life of the process.
    unsafe {
        let mut original: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
            return None;
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
        Some(original)
    }
}

/// Discard whatever landed in stdin's buffer while the tty was deaf, rather
/// than leaving it for the shell to read as a command the instant the prompt
/// under the last frame is ready for one.
fn drain_stdin() {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        // SAFETY: `tcflush` is an ordinary syscall against stdin's descriptor.
        unsafe {
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
        }
    }
}

/// Test-only: set a process environment variable.
///
/// `set_var` is unsafe from edition 2024 because the C environment is global
/// mutable state, and a concurrent `getenv` on another thread is a real race.
/// The tests that call this already serialise every writer of a given
/// variable behind a mutex of their own; that discipline is the safety
/// argument, and it lives in one place here instead of being restated at
/// every call.
#[cfg(test)]
pub fn set_test_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    unsafe { std::env::set_var(key, value) }
}

/// Test-only: clear a process environment variable. See [`set_test_env`].
#[cfg(test)]
pub fn remove_test_env(key: &str) {
    unsafe { std::env::remove_var(key) }
}

/// `std::env::var`, unless a test has overridden `key` on this thread — see
/// [`test_env`]. A non-test build is exactly `std::env::var`.
///
/// `SPOOLWAY_WORKTREE`, `SPOOLWAY_HEAD` and `SPOOLWAY_TASK` are read this way
/// wherever a lane's own turn settles a commit — `commands::report`'s
/// `commit_lane_work` and `auto_commit` — because those three are read on
/// every single `report()` call, in a module whose own tests call `report()`
/// dozens of times over. `set_test_env`, which mutates the real process
/// environment, is fine for a variable one test at a time reaches for; here
/// it would mean any test setting one of these three while a neighbour's own
/// `report()` call is mid-flight hands that neighbour a value it never asked
/// for — the same class of failure [`test_home`] exists to close for `$HOME`.
pub fn env_var(key: &str) -> Result<String, std::env::VarError> {
    #[cfg(test)]
    if let Some(value) = test_env::current(key) {
        return Ok(value);
    }
    std::env::var(key)
}

/// Swapping one environment variable for a thread-local value, for the tests
/// that read [`env_var`] rather than `std::env::var` directly — see its own
/// doc comment for why. The same per-thread design as [`test_home`], down to
/// the reasoning: a mutex here could only hold back the tests that ask for
/// it, and a test that merely reads a variable through [`env_var`] takes no
/// lock and cannot be made to.
#[cfg(test)]
pub(crate) mod test_env {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        /// This thread's own overrides, empty everywhere outside a
        /// [`with_env`] call — which is what makes the real environment the
        /// default rather than whatever a neighbour thread set.
        static VARS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    }

    /// What [`super::env_var`] should answer for `key` on this thread, if
    /// this thread has overridden it.
    pub(crate) fn current(key: &str) -> Option<String> {
        VARS.with(|vars| vars.borrow().get(key).cloned())
    }

    /// Override `key` as `value` for the duration of `f`, so a fixture built
    /// under a scratch worktree is the one [`super::env_var`] actually
    /// answers with. Restores whatever this thread had before, so nesting is
    /// safe.
    pub(crate) fn with_env<T>(key: &str, value: &str, f: impl FnOnce() -> T) -> T {
        let previous =
            VARS.with(|vars| vars.borrow_mut().insert(key.to_string(), value.to_string()));
        let result = f();
        VARS.with(|vars| {
            let mut vars = vars.borrow_mut();
            match previous {
                Some(v) => {
                    vars.insert(key.to_string(), v);
                }
                None => {
                    vars.remove(key);
                }
            }
        });
        result
    }
}

/// Swapping the home directory for a scratch one, shared by every module whose
/// tests need to — `dispatch`, `commands` and `repo` among them.
///
/// **The swap is per-thread, and that is the whole design.** `$HOME` is
/// process-global while unit tests run in parallel, so setting it is a change
/// every other test sees. It does not matter whether the other test also calls
/// `with_home`: a mutex here can only hold back the tests that ask for it, and
/// a test that merely *reads* the home — computing an expected worktree path,
/// say — takes no lock and cannot be made to. That was a real failure that
/// looked like flake: a path assertion comparing one test's scratch home
/// against another's, landing once in a few hundred runs.
///
/// So the override lives in a thread-local that [`home_dir`] reads first. A
/// test that never asked for a scratch home always sees the real one, however
/// many of its neighbours are inside a swap of their own, and `$HOME` itself is
/// left alone.
///
/// This is the one place [`home_dir`] is resolved from, which is what makes it
/// the one place a test stands in for it too.
#[cfg(test)]
pub(crate) mod test_home {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    thread_local! {
        /// The home this thread is standing in, while it is inside
        /// [`with_home`]. `None` everywhere else, which is what makes the real
        /// home the default rather than whatever a neighbour set.
        static HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    /// What [`super::home_dir`] should answer on this thread, if anything.
    pub(crate) fn current() -> Option<PathBuf> {
        HOME.with(|home| home.borrow().clone())
    }

    /// Swap the home directory for the duration of `f`, so a fixture written
    /// under a scratch home is the one the code under test actually finds.
    ///
    /// Restores whatever this thread had before, so nesting is safe.
    pub(crate) fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let previous = HOME.with(|slot| slot.replace(Some(home.to_path_buf())));
        let result = f();
        HOME.with(|slot| *slot.borrow_mut() = previous);
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The property the whole design rests on: a swap on one thread is
        /// invisible to every other. A neighbour reading the home while this
        /// test is inside its own scratch one must still get the real answer.
        #[test]
        fn a_swap_on_one_thread_is_invisible_to_another() {
            let scratch = crate::scratch::root("home-thread-local");
            let real = super::super::home_dir();

            with_home(&scratch, || {
                assert_eq!(super::super::home_dir(), Some(scratch.clone()));

                let seen = std::thread::scope(|s| s.spawn(super::super::home_dir).join().unwrap());
                assert_eq!(
                    seen, real,
                    "a thread that never asked for a scratch home saw one anyway"
                );
            });

            assert_eq!(
                super::super::home_dir(),
                real,
                "the swap outlived the block it was scoped to"
            );
        }

        /// Nesting restores the enclosing swap rather than clearing it, so a
        /// helper that swaps inside a test that already did does not strand
        /// the outer one on the real home.
        #[test]
        fn nesting_restores_the_enclosing_home() {
            let outer = crate::scratch::root("home-nest-outer");
            let inner = crate::scratch::root("home-nest-inner");

            with_home(&outer, || {
                with_home(&inner, || {
                    assert_eq!(super::super::home_dir(), Some(inner.clone()));
                });
                assert_eq!(super::super::home_dir(), Some(outer.clone()));
            });
        }
    }
}

/// One spelling for every absolute path spoolway records, compares, or hands
/// to another program.
pub trait PathExt {
    /// [`Path::canonicalize`].
    fn canonical(&self) -> std::io::Result<PathBuf>;

    /// [`PathExt::canonical`], falling back to the path as given when it
    /// cannot be resolved — a path that does not exist yet, or one whose
    /// checkout is already gone.
    fn comparable(&self) -> PathBuf;
}

impl PathExt for Path {
    fn canonical(&self) -> std::io::Result<PathBuf> {
        self.canonicalize()
    }

    fn comparable(&self) -> PathBuf {
        self.canonical().unwrap_or_else(|_| self.to_path_buf())
    }
}

impl PathExt for PathBuf {
    fn canonical(&self) -> std::io::Result<PathBuf> {
        self.as_path().canonical()
    }

    fn comparable(&self) -> PathBuf {
        self.as_path().comparable()
    }
}

#[cfg(test)]
mod path_spelling_tests {
    use super::*;

    /// The property every comparison in spoolway leans on: whatever spelling
    /// a path arrives in, resolving it twice lands in the same place. Without
    /// it a path recorded by one call and compared by another misses.
    #[test]
    fn resolving_is_idempotent() {
        let dir = crate::scratch::root("path-spelling-idempotent");
        std::fs::create_dir_all(&dir).unwrap();
        let once = dir.canonical().unwrap();
        let twice = once.canonical().unwrap();
        assert_eq!(once, twice);
        assert_eq!(dir, once);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A path that cannot be resolved — it is not there any more, or never
    /// was — still comes back comparable rather than propagating a failure,
    /// because the callers that use it are comparing, not opening.
    #[test]
    fn an_unresolvable_path_falls_back_to_itself() {
        let gone = crate::scratch::root("path-spelling-gone").join("never-created");
        assert!(gone.canonical().is_err());
        assert_eq!(gone.comparable(), gone);
    }
}

//! The handful of places spoolway's behaviour forks on the platform.
//!
//! Gathered here rather than left where each one was needed, because they are
//! the same decision made three times — which shell, which home, which
//! separator — and two subsystems had already grown copies that could drift
//! apart without anything failing.
//!
//! The rule throughout: where a fork has *logic* in it, both sides are compiled
//! everywhere and selected at run time. A `#[cfg(windows)]` body is never built
//! on Linux CI, so its mistakes surface only on a machine nobody is testing on
//! — which is exactly how the quoting in here would have shipped wrong.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// This user's home directory.
///
/// `HOME` is a POSIX convention. Native Windows does not set it — a process
/// started from cmd.exe or PowerShell has `USERPROFILE` and the
/// `HOMEDRIVE`+`HOMEPATH` pair instead, and `HOME` appears only under WSL, Git
/// Bash or MSYS.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(home) = test_home::current() {
        return Some(home);
    }
    if let Some(home) = non_empty("HOME") {
        return Some(PathBuf::from(home));
    }
    if !cfg!(windows) {
        return None;
    }
    if let Some(profile) = non_empty("USERPROFILE") {
        return Some(PathBuf::from(profile));
    }
    // Last resort, and the oldest of the three: a drive letter and a path that
    // only mean anything joined.
    let drive = non_empty("HOMEDRIVE")?;
    let path = non_empty("HOMEPATH")?;
    let mut joined = drive;
    joined.push(path);
    Some(PathBuf::from(joined))
}

/// An environment variable that is set *and* has something in it.
///
/// An empty `HOME` is not a home directory, and taking it as one would put
/// every lane's policy paths at the filesystem root.
fn non_empty(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}

/// Rewrite a path's separators to forward slashes on the platform that does not
/// already use them.
///
/// Takes the platform as an argument rather than reading `cfg!` itself so that
/// a Linux test can check the Windows answer. The callers are the two helpers
/// that turn an absolute path into a repo-relative string.
pub fn forward_slashes(path: String, windows: bool) -> String {
    match windows {
        true => path.replace('\\', "/"),
        false => path,
    }
}

/// A repo-relative path, always written with forward slashes.
///
/// These strings do not stay on the machine that produced them: they go into
/// task files and briefings, which are committed and then read by a lane on
/// whatever platform picks the task up. A `docs\api.html` written on Windows is
/// a path Linux cannot follow, so the separator is normalised once here rather
/// than at each of the call sites. Windows accepts forward slashes itself, so
/// nothing is lost by writing them everywhere.
pub fn relative(root: &Path, path: &Path) -> String {
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    forward_slashes(relative, cfg!(windows))
}

/// The shell to type a command at, and the syntax it expects.
///
/// Two callers, and the reason they share one type is that they are the same
/// problem. `herdr agent start` has no `--env`, so a lane's environment can
/// only reach the agent by being typed at the pane's shell before it launches;
/// a notification hook is a command someone wrote in their own config, handed
/// to an interpreter. Both put a value spoolway did not author inside a quoted
/// string in a shell, and both get it wrong in the same way if the escape does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// `export KEY='value'`, with `'\''` for an embedded quote.
    Posix,
    /// `$env:KEY='value'`, with `''` for an embedded quote.
    PowerShell,
}

impl Shell {
    /// What this platform speaks.
    ///
    /// herdr opens a Windows pane on PowerShell and a pane anywhere else on a
    /// POSIX shell. Someone who has pointed herdr's `terminal.default_shell`
    /// at something else has changed the contract this assumes.
    pub const CURRENT: Shell = if cfg!(windows) {
        Shell::PowerShell
    } else {
        Shell::Posix
    };

    /// Make a value safe to sit *inside* an already-open single-quoted string.
    ///
    /// Split out of [`Shell::quote`] so the escape itself — the one thing
    /// standing between a prompt-supplied string and a shell — has exactly
    /// one implementation per dialect, whoever ends up calling it.
    pub fn escape_single_quoted(self, value: &str) -> String {
        match self {
            // A single-quoted POSIX string cannot contain a quote at all, so
            // the string is closed, an escaped quote emitted, and a new one
            // opened.
            Shell::Posix => value.replace('\'', r"'\''"),
            // PowerShell has no backslash escape inside single quotes; a
            // doubled quote is a literal one.
            Shell::PowerShell => value.replace('\'', "''"),
        }
    }

    /// Quote a value so it arrives as one literal argument.
    ///
    /// Single quotes in both dialects, because neither expands anything inside
    /// them — a task id or a branch name carrying `$`, `%` or a backtick is
    /// inert either way. Only the escape for an embedded quote differs.
    pub(crate) fn quote(self, value: &str) -> String {
        format!("'{}'", self.escape_single_quoted(value))
    }

    /// One assignment, in this dialect's own syntax — the piece
    /// [`Shell::env_export`] and [`Shell::env_export_lines`] both build on,
    /// so the quoting is written once whichever shape the caller needs.
    fn env_assignment(self, key: &str, value: &str) -> String {
        match self {
            Shell::Posix => format!("{key}={}", self.quote(value)),
            Shell::PowerShell => format!("$env:{key}={}", self.quote(value)),
        }
    }

    /// The lane's environment, as one line.
    ///
    /// One command, not one per variable: consecutive sends race the shell's
    /// readiness, and a line arriving while the previous one is still being
    /// read is delivered as a paste — bracketed-paste markers and all — which
    /// sets no variable and leaves the lane quietly missing it.
    pub fn env_export(self, env: &BTreeMap<String, String>) -> String {
        match self {
            Shell::Posix => {
                let pairs = env
                    .iter()
                    .map(|(key, value)| self.env_assignment(key, value))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("export {pairs}")
            }
            // No `export` equivalent: each assignment stands alone, so they are
            // joined with `;` to stay a single line.
            Shell::PowerShell => env
                .iter()
                .map(|(key, value)| self.env_assignment(key, value))
                .collect::<Vec<_>>()
                .join("; "),
        }
    }

    /// The same environment as [`Shell::env_export`], one assignment per
    /// line rather than joined onto one — for a file [`crate::mux::Herdr::hand_environment`]
    /// writes rather than types into a pane, where `env_export`'s own reason
    /// for staying on one line does not apply: nothing about a file races a
    /// shell's paste-readiness, and one `export` per line is what the task's
    /// own mockup draws and what a person opening the file actually reads.
    pub fn env_export_lines(self, env: &BTreeMap<String, String>) -> String {
        env.iter()
            .map(|(key, value)| match self {
                Shell::Posix => format!("export {}", self.env_assignment(key, value)),
                Shell::PowerShell => self.env_assignment(key, value),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The extension a file has to carry to be dot-sourceable in this
    /// dialect. PowerShell refuses to dot-source anything not named `.ps1`
    /// — "the name must end in .ps1" — the same rule `commands::init` and
    /// `assets` already ship `.sh` and `.ps1` pairs for; POSIX `sh` places
    /// no such restriction on itself, so `.env` stays free to say what the
    /// file actually holds.
    pub fn source_extension(self) -> &'static str {
        match self {
            Shell::Posix => "env",
            Shell::PowerShell => "ps1",
        }
    }

    /// The one command that reads an environment [`Shell::env_export_lines`]
    /// wrote to a file, rather than typing it into a pane directly. `path`
    /// must already carry [`Shell::source_extension`]'s own extension, or
    /// PowerShell refuses to run it at all.
    ///
    /// Both dialects dot-source the same way: `. <path>` runs `path`'s lines
    /// in the calling shell rather than a subshell that would take the
    /// exports nowhere, and neither needs anything past that one line —
    /// which is the whole point. A herdr pane cuts an `export` (or
    /// `$env:…=`) line mid-value past some length and then waits forever on
    /// the unterminated quote it left behind; a `.` command naming a file is
    /// short no matter how large the environment inside it is.
    pub fn source_command(self, path: &Path) -> String {
        format!(". {}", self.quote(&path.display().to_string()))
    }

    /// The one export that is not a plain value: the directory is quoted, but
    /// the existing `PATH` has to survive as something the shell still expands.
    pub fn path_export(self, prefix: &Path) -> String {
        let dir = self.quote(&prefix.display().to_string());
        match self {
            Shell::Posix => format!("export PATH={dir}:\"$PATH\""),
            // Concatenation, not interpolation: `"$env:PATH"` inside a double
            // quoted string would expand here, but the separator has to be a
            // literal `;` and a `;` inside an unquoted PowerShell argument
            // would end the statement instead.
            Shell::PowerShell => format!("$env:PATH={dir} + ';' + $env:PATH"),
        }
    }
}

/// A [`Command`] that runs `script` through this platform's shell.
///
/// On Unix this is `sh -c`, which takes the script as one argv entry and is the
/// end of it.
///
/// Windows is not that simple. `powershell.exe -Command` does not read its
/// script from argv — it re-joins the tail of its own command line and parses
/// that, so the quoting Rust applies to pass one argument is not the quoting it
/// undoes, and a hook containing a double quote arrives carrying the backslash
/// Rust added to escape it. `-EncodedCommand` takes base64 UTF-16LE instead:
/// there is no quoting layer left to disagree about, which is the only way to
/// hand an arbitrary hook to PowerShell and know it arrives as written.
pub fn shell_command(script: &str) -> Command {
    if !cfg!(windows) {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        return command;
    }

    let mut command = Command::new(powershell());
    command
        // No profile, so a hook does not inherit whatever a person's profile
        // prints or redefines; non-interactive so it can never sit waiting for
        // a prompt inside a dispatch pass.
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
        .arg(encoded_command(script));
    command
}

/// PowerShell 7 if it is installed, Windows PowerShell 5.1 otherwise.
///
/// `pwsh` is what someone who uses PowerShell deliberately has; `powershell` is
/// what every Windows box ships. Both accept the same three flags, so this
/// chooses the better one and falls back rather than requiring either.
fn powershell() -> &'static str {
    let found = std::env::var_os("PATH")
        .and_then(|path| which("pwsh", &path))
        .is_some();
    match found {
        true => "pwsh",
        false => "powershell",
    }
}

/// Resolve a program name against a `PATH`, the way a shell would.
pub fn which(program: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    // A path, not a name, is taken as given. Windows separates on `\` as well,
    // and a name carrying either is never looked up on PATH.
    if program.contains('/') || (cfg!(windows) && program.contains('\\')) {
        let direct = PathBuf::from(program);
        return direct.is_file().then_some(direct);
    }
    std::env::split_paths(path)
        .flat_map(|dir| candidates(&dir.join(program)))
        .find(|candidate| is_executable(candidate))
}

/// The file names one program name can take in a single directory.
///
/// Exactly one on Unix. On Windows a bare `pi` is not a file name at all: the
/// shell appends each `PATHEXT` entry in turn, so the real agent is `pi.cmd`
/// or `pi.exe` and looking only for `pi` finds nothing.
fn candidates(base: &Path) -> Vec<PathBuf> {
    if !cfg!(windows) {
        return vec![base.to_path_buf()];
    }

    // The default is what a stock Windows sets, and matters because a pane's
    // environment is not guaranteed to carry PATHEXT through.
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let extended = pathext.split(';').filter(|ext| !ext.is_empty()).map(|ext| {
        // Appending to the OsString rather than `set_extension`, which would
        // treat the `.js` in `some.js` as an extension and replace it.
        let mut name = base.as_os_str().to_os_string();
        name.push(ext);
        PathBuf::from(name)
    });

    // A name that already carries an extension is tried as written first;
    // otherwise the extensions come first, because on Windows an
    // extension-less file of the same name is not what the shell would run.
    let bare = base.to_path_buf();
    match base.extension().is_some() {
        true => std::iter::once(bare).chain(extended).collect(),
        false => extended.chain(std::iter::once(bare)).collect(),
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// A script as `-EncodedCommand` wants it: UTF-16LE, base64, no padding rules
/// of its own.
///
/// Written out rather than pulled in, because a dependency for thirty lines of
/// table lookup is a dependency to audit on every release.
pub(crate) fn encoded_command(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();

    let mut out = String::with_capacity(utf16.len().div_ceil(3) * 4);
    for chunk in utf16.chunks(3) {
        let bits = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, byte)| acc | (*byte as u32) << (16 - 8 * i));
        // One output character per 6 bits, then `=` for each byte the chunk was
        // short of three.
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[((bits >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
        for _ in chunk.len()..3 {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sh`, or `cmd.exe`, is on every machine this runs on — used rather than
    /// a fixture binary because the interesting behaviour is the PATH walk
    /// itself, not any one binary's presence.
    #[test]
    fn which_finds_a_real_binary_on_path() {
        let path = std::env::var_os("PATH").unwrap();
        let program = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(which(program, &path).is_some());
        assert!(which("spoolway-nothing-named-this", &path).is_none());
    }

    #[test]
    fn a_repo_relative_path_is_written_with_forward_slashes() {
        // Both answers are checked from either platform: the Windows branch is
        // the one that matters and the one a Linux `cfg!` would skip.
        assert_eq!(
            forward_slashes(r"docs\api.html".to_string(), true),
            "docs/api.html"
        );
        assert_eq!(
            forward_slashes("docs/api.html".to_string(), true),
            "docs/api.html"
        );
        // Nothing is rewritten off Windows, where a backslash is a legal
        // character in a file name and not a separator at all.
        assert_eq!(forward_slashes(r"odd\name".to_string(), false), r"odd\name");
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
        assert_eq!(Shell::Posix.quote("plain"), "'plain'");
        assert_eq!(Shell::Posix.quote("it's"), r"'it'\''s'");
        // PowerShell has no backslash escape inside a single-quoted string;
        // doubling the quote is the only way to get a literal one.
        assert_eq!(Shell::PowerShell.quote("plain"), "'plain'");
        assert_eq!(Shell::PowerShell.quote("it's"), "'it''s'");
    }

    #[test]
    fn the_source_command_is_a_short_dot_command_in_either_dialect() {
        assert_eq!(
            Shell::Posix.source_command(Path::new("/repo/.spoolway/commands/demo.env")),
            ". '/repo/.spoolway/commands/demo.env'"
        );
        assert_eq!(
            Shell::Posix.source_command(Path::new("/it's here/x.env")),
            r". '/it'\''s here/x.env'"
        );
        // `.ps1`, not `.env`: PowerShell refuses to dot-source anything
        // else, so a caller building this path for `Shell::PowerShell` must
        // already have named it with `Shell::source_extension`'s own
        // answer — a fixture ending in `.env` here would pass while the
        // real thing fails against a real PowerShell.
        assert_eq!(
            Shell::PowerShell.source_command(Path::new(r"C:\it's here\x.ps1")),
            r". 'C:\it''s here\x.ps1'"
        );
    }

    #[test]
    fn the_source_extension_is_what_each_dialect_will_actually_dot_source() {
        assert_eq!(Shell::Posix.source_extension(), "env");
        assert_eq!(Shell::PowerShell.source_extension(), "ps1");
    }

    #[test]
    fn env_export_lines_is_one_assignment_per_line() {
        let env = BTreeMap::from([
            ("SPOOLWAY_STEP".to_string(), "implement".to_string()),
            ("SPOOLWAY_TASK".to_string(), "add-endpoint".to_string()),
        ]);
        assert_eq!(
            Shell::Posix.env_export_lines(&env),
            "export SPOOLWAY_STEP='implement'\nexport SPOOLWAY_TASK='add-endpoint'"
        );
        assert_eq!(
            Shell::PowerShell.env_export_lines(&env),
            "$env:SPOOLWAY_STEP='implement'\n$env:SPOOLWAY_TASK='add-endpoint'"
        );
    }

    #[test]
    fn the_path_export_quotes_the_directory_but_still_expands_path() {
        assert_eq!(
            Shell::Posix.path_export(Path::new("/repo/.spoolway/bin")),
            "export PATH='/repo/.spoolway/bin':\"$PATH\""
        );
        // A path a person would never choose, and which would otherwise end
        // the quoting early and run whatever came after it.
        assert_eq!(
            Shell::Posix.path_export(Path::new("/it's here/bin")),
            "export PATH='/it'\\''s here/bin':\"$PATH\""
        );
    }

    #[test]
    fn the_windows_path_export_separates_on_semicolon_and_keeps_it_literal() {
        assert_eq!(
            Shell::PowerShell.path_export(Path::new(r"C:\repo\.spoolway\bin")),
            r"$env:PATH='C:\repo\.spoolway\bin' + ';' + $env:PATH"
        );
        // A backslash is literal inside PowerShell single quotes, so a Windows
        // path needs no escaping — but a quote in a directory name still does.
        assert_eq!(
            Shell::PowerShell.path_export(Path::new(r"C:\it's here\bin")),
            r"$env:PATH='C:\it''s here\bin' + ';' + $env:PATH"
        );
    }

    #[test]
    fn the_lane_environment_is_one_line_in_either_dialect() {
        let env = BTreeMap::from([
            ("SPOOLWAY_STEP".to_string(), "implement".to_string()),
            ("SPOOLWAY_TASK".to_string(), "add-endpoint".to_string()),
        ]);

        assert_eq!(
            Shell::Posix.env_export(&env),
            "export SPOOLWAY_STEP='implement' SPOOLWAY_TASK='add-endpoint'"
        );
        assert_eq!(
            Shell::PowerShell.env_export(&env),
            "$env:SPOOLWAY_STEP='implement'; $env:SPOOLWAY_TASK='add-endpoint'"
        );

        // Whatever the dialect, a lane's environment must never be more than
        // one line: a second line races the shell's readiness and is swallowed.
        for shell in [Shell::Posix, Shell::PowerShell] {
            assert!(!shell.env_export(&env).contains('\n'));
        }
    }

    #[test]
    fn a_hostile_value_cannot_break_out_of_its_quoting() {
        // A task id is checked, but a branch name or a prompt-supplied reason
        // reaches this unfiltered.
        let hostile = "x'; Write-Host PWNED; '";
        let env = BTreeMap::from([("SPOOLWAY_TASK".to_string(), hostile.to_string())]);

        // Every quote in the payload is doubled, so none of them closes the
        // string the value sits in and the semicolons stay data.
        assert_eq!(
            Shell::PowerShell.env_export(&env),
            "$env:SPOOLWAY_TASK='x''; Write-Host PWNED; '''"
        );
    }

    #[test]
    fn a_script_encodes_as_utf16_base64() {
        // Checked against the encoding PowerShell documents for
        // -EncodedCommand, including the padding at each remainder. All three
        // chunk remainders appear, because the padding is where a hand-written
        // base64 goes wrong.
        assert_eq!(encoded_command(""), "");
        assert_eq!(encoded_command("a"), "YQA=");
        assert_eq!(encoded_command("ab"), "YQBiAA==");
        assert_eq!(encoded_command("abc"), "YQBiAGMA");
        assert_eq!(
            encoded_command("Write-Host 'hi'"),
            "VwByAGkAdABlAC0ASABvAHMAdAAgACcAaABpACcA"
        );
        // Beyond the BMP, where one char is two UTF-16 units.
        assert_eq!(encoded_command("\u{1F600}"), "PdgA3g==");
        // Never any of the characters a command line would need quoted, which
        // is the whole reason this encoding is used.
        let encoded = encoded_command("a \"b\" 'c' `d` $e; f & g | h");
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)),
            "{encoded}"
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
    ///
    /// Idempotent, and quietly does nothing on a platform with nothing to hook:
    /// a run there ends the way it always has, which is the behaviour every
    /// caller already handles for a process that was killed outright.
    pub fn catch_interrupt() {
        #[cfg(unix)]
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
///
/// The termios half is a `cfg(unix)` fork beside `headless::kill_group`,
/// deliberately outside the house rule at the top of this file: there is no
/// Windows termios to get right, and the non-Windows-tested body its absence
/// would leave behind is not risked, because nothing behind that fork is
/// exercised there at all. On native Windows the cursor hides and nothing
/// else does — no key is read for anything either way, so `ctrl-c` stays the
/// only control the board has on every platform.
pub struct TermGuard {
    #[cfg(unix)]
    original: Option<libc::termios>,
    /// An inert guard hides nothing and restores nothing. Test-only: a `Board`
    /// built in a test must not take the process's real terminal raw —
    /// parallel tests each restore in their own order, and the last one to run
    /// decides whether the developer's shell is left without echo (finding 53).
    inert: bool,
}

impl TermGuard {
    /// Hide the cursor and, on Unix, put stdin in raw-enough mode: `ECHO` and
    /// `ICANON` off so a keystroke is discarded rather than echoed under the
    /// footer or held for a line that never comes, `ISIG` deliberately kept
    /// so `ctrl-c` still raises `SIGINT` the way [`stop::catch_interrupt`]
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
            #[cfg(unix)]
            original: None,
            inert: true,
        }
    }
}

#[cfg(unix)]
impl TermGuard {
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

#[cfg(not(unix))]
impl TermGuard {
    fn take() -> TermGuard {
        TermGuard { inert: false }
    }

    fn restore(&self) {}
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
#[cfg(unix)]
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
#[cfg(unix)]
fn drain_stdin() {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        // SAFETY: `tcflush` is an ordinary syscall against stdin's descriptor.
        unsafe {
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
        }
    }
}

#[cfg(not(unix))]
fn drain_stdin() {}

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

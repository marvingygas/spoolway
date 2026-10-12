//! Which editor the TUI's open runs, and how it is started.
//!
//! Every open in the TUI — the board's, the pending screen's, the routines
//! pane's and the jobs picker's — goes through [`open`], so the four can never
//! resolve different editors. `spoolway config edit` does not: it waits for
//! the editor and validates afterwards, so it keeps its own `$VISUAL`,
//! `$EDITOR`, `vi` order in `commands::config`.
//!
//! The editor is named per machine, in `~/.spoolway/editor.toml`, because it
//! belongs to a person and a machine and `$EDITOR` is shared with git. The
//! file is never called `config.toml`: `~/.spoolway/` must not read as a
//! project's setup (see `config::setup_dir_in`).
//!
//! A window editor draws its own window, so it is started in the background
//! with no pane. A terminal editor still needs a pane to draw in and opens
//! through [`crate::mux::Mux::open_command`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The file that names the editor, under `~/.spoolway/`.
const FILE: &str = "editor.toml";

/// The fix every refusal about a missing program carries.
const REMEDY: &str = "fix command in ~/.spoolway/editor.toml, or delete the file to use \
     $VISUAL, $EDITOR or vim";

/// Everything the choice depends on, passed in so a test can set each one
/// without touching the process environment.
pub(crate) struct Inputs<'a> {
    pub file: PathBuf,
    pub visual: Option<String>,
    pub editor: Option<String>,
    pub path: OsString,
    pub wsl: bool,
    pub home: Option<PathBuf>,
    pub wslpath: &'a dyn Fn(&Path) -> Result<String>,
}

/// What to run, and whether it draws its own window.
pub(crate) struct Plan {
    pub line: String,
    pub window: bool,
}

/// Unknown keys are refused so that `[editr]`, `[Editor]` or a bare
/// `command = …` is a seen typo rather than a quiet fall-through to `$VISUAL`.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    editor: Option<Setting>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Setting {
    command: String,
    window: Option<bool>,
}

/// Choose the editor and build the line that opens `task` in it.
///
/// The first of the file, `$VISUAL`, `$EDITOR` and `vim` that is set wins. A
/// file that does not parse is an error rather than a reason to try the next
/// one, so a typo is seen instead of silently opening something else.
pub(crate) fn plan(inputs: &Inputs, task: &Path) -> Result<Plan> {
    let (command, window) = match read_file(&inputs.file)? {
        Some(setting) => (setting.command, setting.window),
        None => {
            let command = [&inputs.visual, &inputs.editor]
                .into_iter()
                .flatten()
                .find(|value| !value.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| "vim".to_string());
            (command, None)
        }
    };
    let program = first_word(&command);
    if program.is_empty() {
        bail!("the editor command is empty — {REMEDY}");
    }
    // The shell that runs the line expands a leading `~/`, but `which` takes
    // any word holding a `/` as a literal path and would call it missing. A
    // quoted `'~/…'` is left alone, because the shell leaves it unexpanded too.
    let unquoted_tilde = command.trim_start().starts_with("~/");
    let looked_up = match (program.strip_prefix("~/"), &inputs.home) {
        (Some(rest), Some(home)) if unquoted_tilde => home.join(rest).display().to_string(),
        _ => program.clone(),
    };
    if crate::platform::which(&looked_up, &inputs.path).is_none() {
        bail!("`{program}` is not on PATH — {REMEDY}");
    }
    let exe = program.to_ascii_lowercase().ends_with(".exe");
    // A Windows program cannot open `/home/…`; a Linux launcher can, and must
    // be left alone. WSL is detected from the system, never from a setting.
    let shown = if exe && inputs.wsl {
        (inputs.wslpath)(task)
            .with_context(|| format!("could not convert {} to a Windows path", task.display()))?
    } else {
        task.display().to_string()
    };
    Ok(Plan {
        line: format!("{command} {}", crate::platform::quote(&shown)),
        window: window.unwrap_or(exe),
    })
}

fn read_file(file: &Path) -> Result<Option<Setting>> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("could not read {}", file.display())),
    };
    let parsed: File = toml::from_str(&text)
        .with_context(|| format!("{} does not parse — fix it or delete it", file.display()))?;
    Ok(parsed.editor)
}

/// The command's first shell word, with its quotes taken off.
///
/// `notepad.exe` and `'/mnt/c/Program Files/Notepad++/notepad++.exe' -w` both
/// name their program in the first word; the rest are its flags.
fn first_word(command: &str) -> String {
    let mut word = String::new();
    let mut quote: Option<char> = None;
    for c in command.trim_start().chars() {
        match (quote, c) {
            (None, c) if c.is_whitespace() => break,
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (_, c) => word.push(c),
        }
    }
    word
}

/// True under Windows Subsystem for Linux, read from the running kernel.
fn on_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

/// `wslpath -w`: the `\\wsl.localhost\…` form of a Linux path.
fn wslpath(path: &Path) -> Result<String> {
    let output = std::process::Command::new("wslpath")
        .arg("-w")
        .arg(path)
        .output()
        .context("could not run `wslpath`")?;
    if !output.status.success() {
        bail!("`wslpath -w` exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Open `task` in the person's editor: in the background for a window editor,
/// in a pane named `label` for a terminal one.
pub(crate) fn open(repo: &crate::repo::Repo, label: &str, task: &Path) -> Result<()> {
    let plan = plan(
        &Inputs {
            file: crate::mux::state_root().join(FILE),
            visual: std::env::var("VISUAL").ok(),
            editor: std::env::var("EDITOR").ok(),
            path: std::env::var_os("PATH").unwrap_or_default(),
            wsl: on_wsl(),
            home: crate::platform::home_dir(),
            wslpath: &wslpath,
        },
        task,
    )?;
    if plan.window {
        return start_window(&plan.line, &repo.root);
    }
    crate::mux::backend(repo)?.open_command(&repo.root, label, &plan.line)
}

/// The popup every open in the TUI shows when [`open`] refuses, titled
/// `open task` and closed with `enter`. The refusal is carried whole, `{:#}`
/// rather than `{}`, because the remedy can sit in its context chain: an
/// `editor.toml` that does not parse names its fix on the outer layer, and
/// the parse error under it says which line is wrong. Built here, once, so
/// the board, the pending screen, the routines pane and the jobs routine
/// picker cannot word the same refusal differently.
pub(crate) fn refusal(err: &anyhow::Error) -> crate::screen::Notice {
    crate::screen::Notice::new("open task", format!("o: {err:#}"))
}

/// Start `line` in the background in a session of its own, so quitting
/// spoolway never closes an unsaved editor. Nothing waits on it: a failure
/// after the start shows in the editor's own window.
#[cfg(unix)]
fn start_window(line: &str, cwd: &Path) -> Result<()> {
    let child =
        crate::headless::spawn_detached_shell(line, cwd).context("could not start the editor")?;
    crate::headless::reap_when_it_ends(child);
    Ok(())
}

#[cfg(not(unix))]
fn start_window(_line: &str, _cwd: &Path) -> Result<()> {
    bail!("opening a window editor needs a Unix platform")
}

/// Test-only: stand in for a machine whose `editor.toml` names a terminal
/// editor, so a test that reaches [`open`] neither runs the developer's real
/// editor (a Notepad window on every unit run, on WSL) nor depends on `vim`
/// being installed. Pins this thread's home to a scratch one; keep the
/// returned guard alive for the test.
#[cfg(test)]
pub(crate) fn pin_terminal_editor() -> crate::scratch::ScratchRoot {
    let home = crate::scratch::root("editor-home");
    std::fs::create_dir_all(home.join(".spoolway")).unwrap();
    std::fs::write(
        home.join(".spoolway").join(FILE),
        "[editor]\ncommand = \"/bin/sh\"\nwindow = false\n",
    )
    .unwrap();
    crate::platform::test_home::pin(&home);
    home
}

#[cfg(test)]
mod tests {
    use super::*;

    type Dir = crate::scratch::ScratchRoot;

    fn dir() -> Dir {
        let dir = crate::scratch::root("editor");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A bin directory holding an executable for each name, so `which` finds it.
    fn bin(dir: &Dir, names: &[&str]) -> OsString {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in names {
            let p = bin.join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin.into_os_string()
    }

    fn no_wslpath(_: &Path) -> Result<String> {
        bail!("wslpath must not run here")
    }

    fn inputs<'a>(
        dir: &'a Dir,
        path: OsString,
        wslpath: &'a dyn Fn(&Path) -> Result<String>,
    ) -> Inputs<'a> {
        Inputs {
            file: dir.join("editor.toml"),
            visual: None,
            editor: None,
            path,
            wsl: false,
            home: None,
            wslpath,
        }
    }

    fn write_file(dir: &Dir, text: &str) {
        std::fs::write(dir.join("editor.toml"), text).unwrap();
    }

    #[test]
    fn nothing_set_falls_back_to_vim() {
        let d = dir();
        let i = inputs(&d, bin(&d, &["vim"]), &no_wslpath);
        let plan = plan(&i, Path::new("/a/b.md")).unwrap();
        assert_eq!(plan.line, "vim '/a/b.md'");
    }

    #[test]
    fn editor_beats_vim_and_visual_beats_editor() {
        let d = dir();
        let path = bin(&d, &["vim", "nano", "emacs"]);
        let mut i = inputs(&d, path, &no_wslpath);
        i.editor = Some("nano".into());
        assert_eq!(plan(&i, Path::new("/x")).unwrap().line, "nano '/x'");
        i.visual = Some("emacs -nw".into());
        assert_eq!(plan(&i, Path::new("/x")).unwrap().line, "emacs -nw '/x'");
    }

    #[test]
    fn the_file_beats_visual_and_editor() {
        let d = dir();
        write_file(&d, "[editor]\ncommand = \"vim -R\"\n");
        let mut i = inputs(&d, bin(&d, &["vim", "nano"]), &no_wslpath);
        i.visual = Some("nano".into());
        i.editor = Some("nano".into());
        assert_eq!(plan(&i, Path::new("/x")).unwrap().line, "vim -R '/x'");
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        let d = dir();
        let mut i = inputs(&d, bin(&d, &["vim", "nano"]), &no_wslpath);
        i.visual = Some(String::new());
        i.editor = Some("nano".into());
        assert_eq!(plan(&i, Path::new("/x")).unwrap().line, "nano '/x'");
    }

    #[test]
    fn a_file_that_does_not_parse_names_the_file_and_stops() {
        let d = dir();
        write_file(&d, "[editor\ncommand = ");
        let mut i = inputs(&d, bin(&d, &["vim", "nano"]), &no_wslpath);
        i.editor = Some("nano".into());
        let err = format!("{:#}", plan(&i, Path::new("/x")).err().unwrap());
        assert!(err.contains("editor.toml"), "{err}");
    }

    #[test]
    fn a_program_not_on_path_gets_the_remedy() {
        let d = dir();
        write_file(&d, "[editor]\ncommand = \"notepd.exe\"\n");
        let i = inputs(&d, bin(&d, &["vim"]), &no_wslpath);
        let err = format!("{:#}", plan(&i, Path::new("/x")).err().unwrap());
        assert_eq!(
            err,
            "`notepd.exe` is not on PATH — fix command in ~/.spoolway/editor.toml, or delete \
             the file to use $VISUAL, $EDITOR or vim"
        );
    }

    #[test]
    fn a_quoted_program_with_spaces_is_one_word() {
        let d = dir();
        let b = bin(&d, &["my editor"]);
        let program = std::path::Path::new(&b).join("my editor");
        write_file(
            &d,
            &format!("[editor]\ncommand = \"'{}' -w\"\n", program.display()),
        );
        let i = inputs(&d, OsString::new(), &no_wslpath);
        assert!(plan(&i, Path::new("/x")).is_ok());
    }

    #[test]
    fn on_wsl_an_exe_gets_the_windows_path() {
        let d = dir();
        write_file(&d, "[editor]\ncommand = \"notepad.exe\"\n");
        let seen = |p: &Path| Ok(format!("\\\\wsl$\\U{}", p.display()));
        let mut i = inputs(&d, bin(&d, &["notepad.exe"]), &seen);
        i.wsl = true;
        let plan = plan(&i, Path::new("/it's here")).unwrap();
        assert_eq!(plan.line, "notepad.exe '\\\\wsl$\\U/it'\\''s here'");
    }

    #[test]
    fn off_wsl_an_exe_keeps_the_linux_path() {
        let d = dir();
        write_file(&d, "[editor]\ncommand = \"notepad.exe\"\n");
        let i = inputs(&d, bin(&d, &["notepad.exe"]), &no_wslpath);
        assert_eq!(plan(&i, Path::new("/x")).unwrap().line, "notepad.exe '/x'");
    }

    #[test]
    fn on_wsl_a_linux_program_keeps_the_linux_path() {
        let d = dir();
        let mut i = inputs(&d, bin(&d, &["vim"]), &no_wslpath);
        i.wsl = true;
        assert_eq!(plan(&i, Path::new("/x")).unwrap().line, "vim '/x'");
    }

    #[test]
    fn a_failed_conversion_names_the_path() {
        let d = dir();
        write_file(&d, "[editor]\ncommand = \"notepad.exe\"\n");
        let fail = |_: &Path| bail!("wslpath exited 1");
        let mut i = inputs(&d, bin(&d, &["notepad.exe"]), &fail);
        i.wsl = true;
        let err = format!("{:#}", plan(&i, Path::new("/home/u/t.md")).err().unwrap());
        assert!(err.contains("/home/u/t.md"), "{err}");
    }

    /// The popup carries each refusal whole: the remedy for a missing
    /// program, and every layer of a chain, so a file that does not parse
    /// shows both its fix and the parser's own line, and a failed
    /// conversion both the path and why `wslpath` gave up.
    #[test]
    fn the_refusal_popup_carries_the_whole_error() {
        let d = dir();
        let text = |i: &Inputs, task: &str| {
            let notice = refusal(&plan(i, Path::new(task)).err().unwrap());
            assert_eq!(notice.title, "open task");
            notice.text
        };

        write_file(&d, "[editor]\ncommand = \"notepd.exe\"\n");
        let i = inputs(&d, bin(&d, &["vim"]), &no_wslpath);
        assert_eq!(
            text(&i, "/x"),
            format!("o: `notepd.exe` is not on PATH — {REMEDY}")
        );

        write_file(&d, "[editor\ncommand = ");
        let shown = text(&i, "/x");
        assert!(
            shown.contains("does not parse — fix it or delete it: "),
            "{shown}"
        );
        assert!(shown.contains("line 1"), "the parser's own layer: {shown}");

        write_file(&d, "[editor]\ncommand = \"notepad.exe\"\n");
        let fail = |_: &Path| bail!("wslpath exited 1");
        let mut i = inputs(&d, bin(&d, &["notepad.exe"]), &fail);
        i.wsl = true;
        assert_eq!(
            text(&i, "/home/u/t.md"),
            "o: could not convert /home/u/t.md to a Windows path: wslpath exited 1"
        );
    }

    #[test]
    fn window_defaults_to_true_for_an_exe_and_false_otherwise() {
        let d = dir();
        let mut i = inputs(&d, bin(&d, &["vim", "notepad.exe", "gedit"]), &no_wslpath);
        assert!(!plan(&i, Path::new("/x")).unwrap().window);
        write_file(&d, "[editor]\ncommand = \"notepad.exe\"\n");
        assert!(plan(&i, Path::new("/x")).unwrap().window);
        write_file(&d, "[editor]\ncommand = \"gedit\"\nwindow = true\n");
        assert!(plan(&i, Path::new("/x")).unwrap().window);
        write_file(&d, "[editor]\ncommand = \"notepad.exe\"\nwindow = false\n");
        assert!(!plan(&i, Path::new("/x")).unwrap().window);
        // A program from `$EDITOR` ending in `.exe` is a window too.
        std::fs::remove_file(d.join("editor.toml")).unwrap();
        i.editor = Some("notepad.exe".into());
        assert!(plan(&i, Path::new("/x")).unwrap().window);
    }

    #[test]
    fn a_misspelled_table_is_an_error_not_a_fall_through() {
        let d = dir();
        let mut i = inputs(&d, bin(&d, &["vim", "nano"]), &no_wslpath);
        i.editor = Some("nano".into());
        for text in [
            "[editr]\ncommand = \"vim\"\n",
            "[Editor]\ncommand = \"vim\"\n",
            "command = \"vim\"\n",
        ] {
            write_file(&d, text);
            let err = format!("{:#}", plan(&i, Path::new("/x")).err().unwrap());
            assert!(err.contains("editor.toml"), "{text}: {err}");
        }
    }

    #[test]
    fn a_leading_tilde_in_the_command_is_looked_up_under_home() {
        let d = dir();
        bin(&d, &["notepad.exe"]);
        write_file(&d, "[editor]\ncommand = \"~/bin/notepad.exe\"\n");
        let mut i = inputs(&d, OsString::new(), &no_wslpath);
        i.home = Some(d.to_path_buf());
        assert_eq!(
            plan(&i, Path::new("/x")).unwrap().line,
            "~/bin/notepad.exe '/x'",
            "the shell expands the tilde; the line keeps it"
        );
    }

    #[test]
    fn a_quoted_tilde_is_not_expanded_because_the_shell_will_not() {
        let d = dir();
        bin(&d, &["notepad.exe"]);
        write_file(&d, "[editor]\ncommand = \"'~/bin/notepad.exe'\"\n");
        let mut i = inputs(&d, OsString::new(), &no_wslpath);
        i.home = Some(d.to_path_buf());
        let err = format!("{:#}", plan(&i, Path::new("/x")).err().unwrap());
        assert!(err.contains("is not on PATH"), "{err}");
    }

    #[test]
    fn a_window_editor_starts_in_the_background_with_its_own_session() {
        let d = dir();
        let pidfile = d.join("pid");
        // The shell writes its own pid, then becomes a long sleep to look at.
        let line = format!("echo $$ > '{}'; exec sleep 30", pidfile.display());
        start_window(&line, &d).unwrap();
        let pid = crate::headless::await_pid_file(|| {
            std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok()
        })
        .expect("the editor never ran");
        // SAFETY: `getsid` takes no pointers; a pid that is not there is -1.
        let (theirs, ours) = unsafe { (libc::getsid(pid as i32), libc::getsid(0)) };
        crate::headless::kill_group(pid);
        assert_ne!(theirs, ours, "shares our session");
        assert_eq!(theirs, pid as i32, "not a session leader of its own");
    }
}

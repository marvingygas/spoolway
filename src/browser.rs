//! Open a file in the person's default browser, chosen per platform.
//!
//! The program is named by the platform, never by "the first opener found on
//! `PATH`". On Debian and Ubuntu `/usr/bin/open` is `openvt`, which is not a
//! browser opener, and on WSL `xdg-open` is often installed with no browser
//! behind it. So macOS gets `open`, WSL gets `wslview` (or `explorer.exe`
//! when `wslview` is missing), and any other Unix gets `xdg-open`.
//!
//! The choice is a pure function over [`Inputs`], so a test can cover every
//! route without starting a browser. [`open`] is the one caller that spawns,
//! and [`open_plan`] is how the `o` menus reach it.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// Everything the choice depends on, passed in so a test can set each one
/// without touching the process environment.
pub(crate) struct Inputs<'a> {
    pub macos: bool,
    pub wsl: bool,
    pub path: OsString,
    pub wslpath: &'a dyn Fn(&Path) -> Result<String>,
}

/// The line `sh` runs to open `file`: a program, then the quoted path.
///
/// An `Err` means no browser can be started, and carries what to do instead.
pub(crate) fn plan(inputs: &Inputs, file: &Path) -> Result<String> {
    // A relative path would be resolved against the browser's directory, not
    // the caller's, and its empty parent would fail the spawn with a
    // misleading "No such file or directory".
    if !file.is_absolute() {
        bail!(
            "could not open the plan page: {} is not an absolute path.\n\
             give the full path of the file, starting with /.",
            file.display()
        );
    }
    let found = |program: &str| crate::platform::which(program, &inputs.path).is_some();
    let shown = file.display().to_string();
    if inputs.macos {
        // `open` ships with macOS, so it is not looked for on PATH.
        return Ok(format!("open {}", crate::platform::quote(&shown)));
    }
    if inputs.wsl {
        let program = if found("wslview") {
            "wslview"
        } else if found("explorer.exe") {
            "explorer.exe"
        } else {
            bail!(
                "could not open the plan page: neither wslview nor explorer.exe is on PATH.\n\
                 install wslu, or open this file in a browser yourself:\n{shown}"
            );
        };
        // A Windows program cannot read `/home/…`, and an old `wslview` does
        // not convert it, so both get the `\\wsl.localhost\…` form.
        let windows = (inputs.wslpath)(file).with_context(|| {
            format!(
                "could not convert {shown} to a Windows path; open the file in a browser yourself"
            )
        })?;
        return Ok(format!("{program} {}", crate::platform::quote(&windows)));
    }
    if !found("xdg-open") {
        bail!(
            "could not open the plan page: xdg-open is not on PATH.\n\
             install xdg-utils, or open this file in a browser yourself:\n{shown}"
        );
    }
    Ok(format!("xdg-open {}", crate::platform::quote(&shown)))
}

/// The `o` menu's `open plan` on `plan`, a task's `plan:` value: open the
/// page through [`open`], or the popup saying why it could not be, titled
/// `open plan` and closed with `enter`. Built here, once, so the board and
/// the pending screen cannot word the same refusal differently.
///
/// A page that is gone is caught before any program starts: a browser
/// handed a missing file opens a "file not found" tab of its own, which
/// tells a person nothing about the task that pointed there. A relative
/// path skips that check, since it would be resolved against this
/// process's directory, and [`plan`] refuses it with its own advice.
pub(crate) fn open_plan(plan: &str) -> Option<crate::screen::Notice> {
    let file = Path::new(plan);
    if file.is_absolute() && !file.exists() {
        return Some(crate::screen::Notice::new(
            "open plan",
            format!(
                "o: the plan page is gone.\n{plan}\n\
                 pick open task to fix or remove plan: in the task file."
            ),
        ));
    }
    open(file)
        .err()
        .map(|err| crate::screen::Notice::new("open plan", format!("o: {err:#}")))
}

/// Open the existing file at the absolute path `file` in the default browser
/// and return at once.
///
/// The browser starts in a session of its own, so quitting spoolway never
/// closes it, and nothing waits on it: a failure after the start shows in
/// the browser.
#[cfg(all(unix, not(test)))]
fn open(file: &Path) -> Result<()> {
    let inputs = Inputs {
        macos: cfg!(target_os = "macos"),
        wsl: crate::platform::on_wsl(),
        path: std::env::var_os("PATH").unwrap_or_default(),
        wslpath: &crate::platform::wslpath,
    };
    let line = plan(&inputs, file)?;
    // `plan` has refused a relative path, so the parent is a real directory.
    let cwd = file.parent().unwrap_or(Path::new("/"));
    let child =
        crate::headless::spawn_detached_shell(&line, cwd).context("could not start the browser")?;
    crate::headless::reap_when_it_ends(child);
    Ok(())
}

#[cfg(all(not(unix), not(test)))]
fn open(_file: &Path) -> Result<()> {
    bail!("opening a browser needs a Unix platform")
}

/// Test-only [`open`]: records `file` instead of starting a browser, so a
/// test that picks `open plan` never opens a real window on the developer's
/// machine, or refuses with the text [`refuse_opens`] pinned.
#[cfg(test)]
fn open(file: &Path) -> Result<()> {
    if let Some(refusal) = test_opens::REFUSAL.with(|r| r.borrow().clone()) {
        bail!(refusal);
    }
    test_opens::OPENED.with(|o| o.borrow_mut().push(file.to_path_buf()));
    Ok(())
}

#[cfg(test)]
mod test_opens {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        pub(super) static OPENED: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
        pub(super) static REFUSAL: RefCell<Option<String>> = const { RefCell::new(None) };
    }
}

/// Test-only: every file this thread's [`open`] was handed, in order.
#[cfg(test)]
pub(crate) fn opened() -> Vec<std::path::PathBuf> {
    test_opens::OPENED.with(|o| o.borrow().clone())
}

/// Test-only: make this thread's [`open`] refuse with `text`, the way
/// [`plan`] refuses on a machine with no opener.
#[cfg(test)]
pub(crate) fn refuse_opens(text: &str) {
    test_opens::REFUSAL.with(|r| *r.borrow_mut() = Some(text.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    type Dir = crate::scratch::ScratchRoot;

    fn dir() -> Dir {
        let dir = crate::scratch::root("browser");
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

    fn windows(path: &Path) -> Result<String> {
        Ok(format!(r"\\wsl.localhost\Ubuntu{}", path.display()).replace('/', r"\"))
    }

    fn inputs(
        path: OsString,
        macos: bool,
        wsl: bool,
        wslpath: &dyn Fn(&Path) -> Result<String>,
    ) -> Inputs<'_> {
        Inputs {
            macos,
            wsl,
            path,
            wslpath,
        }
    }

    /// A page that exists is handed to the opener, and no popup is shown.
    #[test]
    fn open_plan_opens_an_existing_page() {
        let d = dir();
        let page = d.join("plan.html");
        std::fs::write(&page, "<html>").unwrap();
        assert!(open_plan(page.to_str().unwrap()).is_none());
        assert_eq!(opened(), std::slice::from_ref(&page));
    }

    /// A page that is gone gets the `open plan` popup naming the path and
    /// what to do, and no opener is started.
    #[test]
    fn open_plan_on_a_missing_page_names_it() {
        let notice = open_plan("/nowhere/2026-10-17-cart.html").unwrap();
        assert_eq!(notice.title, "open plan");
        assert_eq!(
            notice.text,
            "o: the plan page is gone.\n/nowhere/2026-10-17-cart.html\n\
             pick open task to fix or remove plan: in the task file."
        );
        assert!(opened().is_empty());
    }

    /// An opener that refuses shows its whole text under the `open plan`
    /// title, the remedy and the path included.
    #[test]
    fn open_plan_carries_the_openers_refusal() {
        let d = dir();
        let page = d.join("plan.html");
        std::fs::write(&page, "<html>").unwrap();
        refuse_opens("could not open the plan page: xdg-open is not on PATH.");
        let notice = open_plan(page.to_str().unwrap()).unwrap();
        assert_eq!(notice.title, "open plan");
        assert_eq!(
            notice.text,
            "o: could not open the plan page: xdg-open is not on PATH."
        );
    }

    /// `open` on macOS runs without a PATH search, and quotes the path.
    #[test]
    fn macos_runs_open() {
        let line = plan(
            &inputs(OsString::new(), true, false, &no_wslpath),
            Path::new("/a/b.html"),
        )
        .unwrap();
        assert_eq!(line, "open '/a/b.html'");
    }

    /// A `/usr/bin/open` on PATH (`openvt` on Debian) is never chosen.
    #[test]
    fn linux_runs_xdg_open_never_open() {
        let d = dir();
        let path = bin(&d, &["open", "xdg-open"]);
        let line = plan(
            &inputs(path, false, false, &no_wslpath),
            Path::new("/a/b.html"),
        )
        .unwrap();
        assert_eq!(line, "xdg-open '/a/b.html'");
    }

    /// With only `open` on PATH, Linux refuses rather than running it, and
    /// the text is the one the notice carries.
    #[test]
    fn linux_without_xdg_open_refuses_with_the_path() {
        let d = dir();
        let path = bin(&d, &["open"]);
        let err = plan(
            &inputs(path, false, false, &no_wslpath),
            Path::new("/a/b.html"),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "could not open the plan page: xdg-open is not on PATH.\n\
             install xdg-utils, or open this file in a browser yourself:\n/a/b.html"
        );
    }

    /// `wslview` wins on WSL and is handed the Windows form of the path.
    #[test]
    fn wsl_prefers_wslview_with_a_windows_path() {
        let d = dir();
        let path = bin(&d, &["wslview", "explorer.exe", "xdg-open"]);
        let line = plan(&inputs(path, false, true, &windows), Path::new("/a/b.html")).unwrap();
        assert_eq!(line, r"wslview '\\wsl.localhost\Ubuntu\a\b.html'");
    }

    /// Without `wslview`, `explorer.exe` takes the same Windows path.
    #[test]
    fn wsl_falls_back_to_explorer() {
        let d = dir();
        let path = bin(&d, &["explorer.exe", "xdg-open"]);
        let line = plan(&inputs(path, false, true, &windows), Path::new("/a/b.html")).unwrap();
        assert_eq!(line, r"explorer.exe '\\wsl.localhost\Ubuntu\a\b.html'");
    }

    /// With neither Windows opener, WSL refuses even if `xdg-open` exists,
    /// and names both programs, `wslu` and the path.
    #[test]
    fn wsl_without_either_refuses() {
        let d = dir();
        let path = bin(&d, &["xdg-open"]);
        let err = plan(
            &inputs(path, false, true, &no_wslpath),
            Path::new("/a/b.html"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("wslview") && err.contains("explorer.exe"),
            "{err}"
        );
        assert!(err.contains("install wslu"), "{err}");
        assert!(err.ends_with("\n/a/b.html"), "{err}");
    }

    /// A single quote and a space in the path stay inside one argument.
    #[test]
    fn a_quote_in_the_path_is_escaped() {
        let d = dir();
        let path = bin(&d, &["xdg-open"]);
        let line = plan(
            &inputs(path, false, false, &no_wslpath),
            Path::new("/a b/it's.html"),
        )
        .unwrap();
        assert_eq!(line, r#"xdg-open '/a b/it'\''s.html'"#);
    }

    /// A relative path is refused before any program is looked for.
    #[test]
    fn a_relative_path_is_refused() {
        let d = dir();
        let path = bin(&d, &["xdg-open"]);
        let err = plan(
            &inputs(path, false, false, &no_wslpath),
            Path::new("plan.html"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("plan.html is not an absolute path"), "{err}");
    }

    /// A failed conversion names the path and carries why `wslpath` gave up.
    #[test]
    fn a_failed_conversion_names_the_path_and_the_reason() {
        let d = dir();
        let path = bin(&d, &["wslview"]);
        let fail = |_: &Path| bail!("wslpath exited 1");
        let err = plan(&inputs(path, false, true, &fail), Path::new("/a/b.html")).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("could not convert /a/b.html to a Windows path"),
            "{text}"
        );
        assert!(text.contains("wslpath exited 1"), "{text}");
    }
}

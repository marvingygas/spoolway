//! Open a file in the person's default browser, chosen per platform.
//!
//! The program is named by the platform, never by "the first opener found on
//! `PATH`". On Debian and Ubuntu `/usr/bin/open` is `openvt`, which is not a
//! browser opener, and on WSL `xdg-open` is often installed with no browser
//! behind it. So macOS gets `open`, WSL gets `wslview` (or `explorer.exe`
//! when `wslview` is missing), and any other Unix gets `xdg-open`.
//!
//! The choice is a pure function over [`Inputs`], so a test can cover every
//! route without starting a browser. [`open`] is the one caller that spawns.

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
             Give the full path of the file, starting with /.",
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
                 Install wslu, or open this file in a browser yourself:\n{shown}"
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
             Install xdg-utils, or open this file in a browser yourself:\n{shown}"
        );
    }
    Ok(format!("xdg-open {}", crate::platform::quote(&shown)))
}

/// Open the existing file at the absolute path `file` in the default browser
/// and return at once.
///
/// The browser starts in a session of its own, so quitting spoolway never
/// closes it, and nothing waits on it: a failure after the start shows in
/// the browser. Nothing calls this yet; the `open plan` menu item will.
#[allow(dead_code)]
#[cfg(unix)]
pub(crate) fn open(file: &Path) -> Result<()> {
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

#[cfg(not(unix))]
#[allow(dead_code)]
pub(crate) fn open(_file: &Path) -> Result<()> {
    bail!("opening a browser needs a Unix platform")
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
             Install xdg-utils, or open this file in a browser yourself:\n/a/b.html"
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
        assert!(err.contains("Install wslu"), "{err}");
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

//! The overrides tab's `w`: open the overrides folder, where the prompt
//! copies and patch files the tab writes are edited by hand.
//!
//! Not every machine the board runs on has a file manager, so `w` tries, in
//! order: the platform's own opener ([`crate::browser::folder_plan`]), then,
//! when the board runs inside herdr, a shell pane split beside the board's
//! own pane, started in the folder. When neither can start, the `open
//! workspace` popup names why and gives the path to open by hand.
//!
//! A project that has overridden nothing has no overrides folder, so `w`
//! creates it empty first. `override list` and the dispatch tab's overrides
//! popup read an empty folder as no overrides at all, the same as a missing
//! one, so the project still reads as having none.

use std::path::Path;

use anyhow::{Context, Result};

use crate::mux::Mux;
use crate::repo::Repo;

/// How the folder is named in every refusal and in the popup.
const WHAT: &str = "the overrides folder";

/// Open `repo`'s overrides folder, creating it first if it is missing.
/// Answers the `open workspace` popup's text when nothing could open it.
pub(super) fn open(repo: &Repo) -> Option<String> {
    open_with(
        &repo.overrides_dir(),
        |dir| crate::browser::open_folder(dir, WHAT),
        board_pane().as_deref(),
        || crate::mux::backend(repo),
    )
}

/// [`open`], with the opener, the board's pane and the multiplexer passed
/// in, so a test can refuse the opener and watch which pane is split.
fn open_with(
    dir: &Path,
    opener: impl FnOnce(&Path) -> Result<()>,
    pane: Option<&str>,
    mux: impl FnOnce() -> Result<Box<dyn Mux>>,
) -> Option<String> {
    if let Err(err) = std::fs::create_dir_all(dir) {
        return Some(format!(
            "w: could not create {WHAT}: {err}.\nmake this folder yourself, then press w again:\n{}",
            dir.display()
        ));
    }
    let refusal = opener(dir).err()?;
    let why = match pane {
        None => format!("w: {refusal:#}, and the board is not running inside herdr."),
        Some(pane) => match mux().and_then(|mux| split_beside(mux.as_ref(), pane, dir)) {
            Ok(_) => return None,
            // Inside herdr, but the split failed too: the popup is all that
            // is left, and it says why each route did not open the folder.
            Err(err) => format!(
                "w: {refusal:#}, and no herdr pane could be split beside the board: {err:#}."
            ),
        },
    };
    Some(format!(
        "{why}\nopen this folder yourself:\n{}",
        dir.display()
    ))
}

/// A shell pane in `dir`, split beside `pane` in `pane`'s own tab and given
/// focus. `pane` is the board's own, so the split never lands in a tab or a
/// workspace a person happens to be looking at instead.
fn split_beside(mux: &dyn Mux, pane: &str, dir: &Path) -> Result<String> {
    let tab = mux
        .pane_tab(pane)?
        .with_context(|| format!("herdr has no pane {pane}, the one the board runs in"))?;
    mux.split_beside_focused(&tab.tab_id, pane, dir)
}

/// The herdr pane the board runs in, from the `HERDR_PANE_ID` herdr starts
/// every pane's shell with, or `None` outside herdr.
#[cfg(not(test))]
fn board_pane() -> Option<String> {
    crate::platform::env_var("HERDR_PANE_ID")
        .ok()
        .filter(|pane| !pane.is_empty())
}

/// Test-only [`board_pane`]: always outside herdr. The test run may itself
/// sit in a herdr pane, and a `w` a test presses must never split it.
#[cfg(test)]
fn board_pane() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{Lane, LaneSpec, PaneTab, Workspace};
    use std::sync::{Arc, Mutex};

    /// Every split a [`FakeMux`] was asked for: the tab, the pane split off,
    /// and the folder the shell starts in.
    type Splits = Arc<Mutex<Vec<(String, String, std::path::PathBuf)>>>;

    /// A multiplexer that knows one pane, `w3:p2` in tab `w3:t1`, and
    /// records every split it is asked for. Every call `w` must not make
    /// panics.
    #[derive(Default)]
    struct FakeMux {
        splits: Splits,
        refuse_split: bool,
    }

    impl Mux for FakeMux {
        fn name(&self) -> &'static str {
            "herdr"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable(&self) -> String {
            unimplemented!()
        }
        fn resident_while_waiting(&self) -> bool {
            true
        }
        fn list_lanes(&self) -> Result<Vec<Lane>> {
            unimplemented!()
        }
        fn create_workspace(
            &self,
            _cwd: &Path,
            _branch: &str,
            _base: &str,
            _label: &str,
        ) -> Result<Workspace> {
            unimplemented!()
        }
        fn remove_workspace(&self, _workspace_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn close_workspace(&self, _workspace_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn close_tab(&self, _tab_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn create_pane(&self, _cwd: &Path, _label: &str) -> Result<Workspace> {
            panic!("w opened a pane of its own rather than splitting the board's");
        }
        fn split_pane(&self, _tab_id: &str, _cwd: &Path) -> Result<String> {
            panic!("w split the smallest pane rather than the board's");
        }
        fn split_beside(&self, _tab_id: &str, _pane_id: &str, _cwd: &Path) -> Result<String> {
            panic!("w split a pane without giving it focus");
        }
        fn split_beside_focused(&self, tab_id: &str, pane_id: &str, cwd: &Path) -> Result<String> {
            if self.refuse_split {
                anyhow::bail!("herdr pane split: no such tab");
            }
            self.splits.lock().unwrap().push((
                tab_id.to_string(),
                pane_id.to_string(),
                cwd.to_path_buf(),
            ));
            Ok("w3:p9".to_string())
        }
        fn pane_tab(&self, pane_id: &str) -> Result<Option<PaneTab>> {
            Ok((pane_id == "w3:p2").then(|| PaneTab {
                tab_id: "w3:t1".to_string(),
                panes: 1,
            }))
        }
        fn close_pane(&self, _pane_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn start_lane(&self, _spec: &LaneSpec<'_>, _tick: &mut dyn FnMut()) -> Result<()> {
            unimplemented!()
        }
        fn prompt(&self, _name: &str, _text: &str) -> Result<()> {
            unimplemented!()
        }
        fn read(&self, _name: &str, _lines: usize) -> Result<String> {
            unimplemented!()
        }
        fn interrupt_lane(&self, _name: &str) -> Result<()> {
            unimplemented!()
        }
        fn stop_lane(&self, _name: &str, _pane_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn focus_lane(&self, _name: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_pane(&self, _pane_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
    }

    fn dir(name: &str) -> crate::scratch::ScratchRoot {
        let root = crate::scratch::root(&format!("overrides-workspace-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn no_display(_: &Path) -> Result<()> {
        anyhow::bail!("could not open the overrides folder: no display is set")
    }

    fn no_mux() -> Result<Box<dyn Mux>> {
        panic!("w reached for herdr outside it");
    }

    // An opener that starts is all it takes: the missing folder is created
    // first and handed to it, and no popup is shown.
    #[test]
    fn the_opener_gets_the_folder_created_first() {
        let root = dir("opened");
        let overrides = root.join("overrides");
        let mut handed = None;
        let opener = |dir: &Path| {
            assert!(dir.is_dir(), "created before the opener runs");
            handed = Some(dir.to_path_buf());
            Ok(())
        };
        assert_eq!(open_with(&overrides, opener, None, no_mux), None);
        assert_eq!(handed.as_deref(), Some(overrides.as_path()));
        assert_eq!(std::fs::read_dir(&overrides).unwrap().count(), 0);
    }

    // Inside herdr, a refused opener splits the board's own pane, named by
    // its id, in that pane's own tab, and starts the shell in the folder.
    #[test]
    fn inside_herdr_the_boards_own_pane_is_split_in_the_folder() {
        let root = dir("herdr");
        let overrides = root.join("overrides");
        let splits = Splits::default();
        let mux = FakeMux {
            splits: splits.clone(),
            ..FakeMux::default()
        };
        let shown = open_with(&overrides, no_display, Some("w3:p2"), move || {
            Ok(Box::new(mux) as Box<dyn Mux>)
        });
        assert_eq!(shown, None);
        assert_eq!(
            *splits.lock().unwrap(),
            [("w3:t1".to_string(), "w3:p2".to_string(), overrides.clone())]
        );
    }

    // Outside herdr, the popup names the opener's reason and the path, as
    // the plan draws it.
    #[test]
    fn outside_herdr_the_popup_gives_the_reason_and_the_path() {
        let root = dir("outside");
        let overrides = root.join("overrides");
        let shown = open_with(&overrides, no_display, None, no_mux).unwrap();
        assert_eq!(
            shown,
            format!(
                "w: could not open the overrides folder: no display is set, and the board is \
                 not running inside herdr.\nopen this folder yourself:\n{}",
                overrides.display()
            )
        );
    }

    // Inside herdr with a split that fails too, the popup says why each
    // route did not open the folder, and still gives the path.
    #[test]
    fn a_split_that_fails_is_named_in_the_popup() {
        let root = dir("split-fails");
        let overrides = root.join("overrides");
        let shown = open_with(&overrides, no_display, Some("w3:p2"), || {
            Ok(Box::new(FakeMux {
                refuse_split: true,
                ..FakeMux::default()
            }) as Box<dyn Mux>)
        })
        .unwrap();
        assert!(
            shown.starts_with(
                "w: could not open the overrides folder: no display is set, and no herdr pane \
                 could be split beside the board: herdr pane split: no such tab.\n"
            ),
            "{shown}"
        );
        assert!(shown.ends_with(&format!("\n{}", overrides.display())));

        let gone = open_with(&overrides, no_display, Some("w3:p8"), || {
            Ok(Box::new(FakeMux::default()) as Box<dyn Mux>)
        })
        .unwrap();
        assert!(
            gone.contains("herdr has no pane w3:p8, the one the board runs in."),
            "{gone}"
        );
    }
}

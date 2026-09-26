//! The ledger scope and window `spoolway eval` reads through.
//!
//! This is what remains of the old `spoolway spend` command: `spoolway eval`
//! is the one command a person reads the ledger with now, and [`collect_scoped`]
//! and [`window_of`] below are the only pieces of it `crate::eval` still calls.
//! Every grouping path — the totals, the printing, the per-`by` cut — went
//! with the command.

use anyhow::{Context, Result, bail};

use crate::repo::Repo;

/// Which ledgers this invocation is reading.
pub struct Scope {
    /// Human description for the header and the nothing-found message.
    pub what: String,
}

/// The window a `--since` and `--until` pair describes.
///
/// Shared with `spoolway eval`'s own screen, which takes the same two forms
/// for the same reason: a person asking what a duration or a date range cost
/// means the same thing in either place. `--since`/`--until` already read a
/// whole month (`2026-08`) via `crate::usage::parse_instant`, so there is no
/// separate `--month` form to carry here.
pub fn window_of(since: Option<&str>, until: Option<&str>) -> Result<crate::usage::Window> {
    let window = crate::usage::Window {
        from: since
            .map(|raw| crate::usage::parse_instant(raw, false))
            .transpose()
            .context("`--since`")?,
        until: until
            .map(|raw| crate::usage::parse_instant(raw, true))
            .transpose()
            .context("`--until`")?,
    };
    if let (Some(from), Some(until)) = (window.from, window.until)
        && from >= until
    {
        bail!("that window ends before it starts");
    }
    Ok(window)
}

/// Load the ledgers this invocation asks for.
///
/// The default is this project alone, which is not a filter but the shape of
/// the storage: a ledger lives inside its project. `--all` and `--project` are
/// what reach past that, through the registry.
pub fn collect_scoped(
    repo: &Repo,
    project: Option<&str>,
    all: bool,
) -> Result<(Vec<crate::usage::Entry>, Scope)> {
    use crate::usage::registry;

    if let Some(wanted) = project {
        let known = registry::list();
        let matched: Vec<std::path::PathBuf> = known
            .iter()
            .filter(|root| registry::name_of(root) == wanted || root.as_os_str() == wanted)
            .cloned()
            .collect();

        let root = match matched.len() {
            1 => matched.into_iter().next().unwrap(),
            0 => {
                let names: Vec<String> = known.iter().map(|r| registry::name_of(r)).collect();
                bail!(
                    "no project named `{wanted}`{}",
                    if names.is_empty() {
                        // The registry only fills up as projects are used, so
                        // an empty one is expected rather than broken.
                        ". No projects are registered yet — spoolway notes one \
                         when you `init` it or dispatch in it."
                            .to_string()
                    } else {
                        format!(". Known: {}", names.join(", "))
                    }
                )
            }
            // Two checkouts of the same repo is an ordinary thing to have.
            _ => bail!(
                "`{wanted}` matches {} projects; give a path instead: {}",
                matched.len(),
                matched
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };

        let what = format!("project `{}`", registry::name_of(&root));
        return Ok((crate::usage::read_project(&root), Scope { what }));
    }

    if all {
        let roots = registry::list();
        let entries: Vec<crate::usage::Entry> = roots
            .iter()
            .flat_map(|r| crate::usage::read_project(r))
            .collect();
        return Ok((
            entries,
            Scope {
                what: format!("{} projects", roots.len()),
            },
        ));
    }

    let mut entries = crate::usage::read(repo)?;
    let name = registry::name_of(&repo.root);
    for entry in &mut entries {
        entry.project = name.clone();
    }
    Ok((
        entries,
        Scope {
            what: format!("project `{name}`"),
        },
    ))
}

//! Whether a text is a copy of a shipped file that spoolway wrote.
//!
//! A shipped file that differs from this binary's copy is either stale or
//! newer: a file a newer spoolway wrote looks exactly like a stale one, so an
//! older binary that rewrote it would roll it back. The way out needs nothing
//! recorded in the project: the binary carries `assets/known-copies.txt`, the
//! fingerprints of the copies of every shipped file that earlier builds
//! committed, and [`lookup`] sorts a text into this binary's copy, an older
//! known copy, or one it has never shipped.
//!
//! Three kinds of text are listed: each skill file under `assets/skills/`, the
//! pipeline key block read out of the shipped `default` pipeline, and the config
//! reference table. A skill file and the key block have every revision git
//! history holds. The reference table has only the one each released version
//! scaffolded, from `scripts/e2e/fixtures/`, because it was never a file in the
//! repository. A table an unreleased build wrote is not listed.
//!
//! `sync`, `init` and `install` ask [`lookup`] before they overwrite a skill
//! file, a pipeline's key block or the config reference table. This binary's
//! own copy is kept, an older one is rewritten, and one nobody shipped is
//! refused with [`refusal`], because a newer spoolway or a person wrote it.

use crate::cli::Provider;
use crate::skeleton::fingerprint;

/// The list, embedded so the binary needs nothing on disk to recognise a copy.
const LIST: &str = include_str!("../assets/known-copies.txt");

/// Which shipped file a text is a copy of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shipped {
    /// A skill file: `file` is `SKILL.md` or `assets/<name>`, relative to the
    /// skill's directory. `skill` is a `String` so a retired skill, which has
    /// earlier copies but no current one, can be asked about too.
    ///
    /// The provider is the one the copy is installed for. A skill's assets
    /// are the Claude copy for every provider, and the list keeps them under
    /// `claude/`, so [`Shipped::identity`] maps them there. A caller never
    /// has to know that.
    Skill {
        provider: Provider,
        skill: String,
        file: String,
    },
    /// The key reference fenced inside a pipeline file.
    PipelineKeys,
    /// The reference table at the top of `config.toml`.
    ConfigReference,
}

/// Why `sync`, `init` and `install` refuse a file that is none of the known
/// copies. `shown` is the path as a person would type it back to `spoolway
/// sync --replace`.
///
/// One wording for `sync`, `init` and `install`, so the three name the same
/// reason and the same way out.
pub fn refusal(shown: &str) -> String {
    format!(
        "not a copy this spoolway knows: a newer spoolway or a hand edit wrote it. Nothing \
         here was rewritten. Run `spoolway sync --replace {shown}` to take this spoolway's \
         copy (yours is kept as a .bak)."
    )
}

/// What [`lookup`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    /// The text this binary ships.
    Current,
    /// A text an earlier build shipped and this one no longer does.
    Older,
    /// A text no build has shipped: someone edited it, or a newer build wrote
    /// it.
    Unknown,
}

/// The provider whose copy of a skill file is listed: the one asked for,
/// except for an asset, which every provider is handed the Claude copy of.
fn listed_provider(provider: Provider, file: &str) -> Provider {
    if file.starts_with("assets/") {
        Provider::Claude
    } else {
        provider
    }
}

impl Shipped {
    /// The name this file has in `assets/known-copies.txt`.
    fn identity(&self) -> String {
        match self {
            Shipped::Skill {
                provider,
                skill,
                file,
            } => format!(
                "skill {}/{skill}/{file}",
                listed_provider(*provider, file).name()
            ),
            Shipped::PipelineKeys => "pipeline-keys".to_string(),
            Shipped::ConfigReference => "config-reference".to_string(),
        }
    }

    /// This binary's text for the file, or `None` for a skill file it does not
    /// ship.
    fn current(&self) -> Option<String> {
        match self {
            Shipped::Skill {
                provider,
                skill,
                file,
            } => crate::install::shipped_skill_files()
                .into_iter()
                .find(|s| {
                    s.provider == listed_provider(*provider, file)
                        && s.skill == skill
                        && s.file == *file
                })
                .map(|s| s.text.to_string()),
            Shipped::PipelineKeys => Some(crate::pipeline::key_block().to_string()),
            Shipped::ConfigReference => Some(crate::confkv::reference_table()),
        }
    }
}

/// Every fingerprint listed for `identity`, in file order.
fn listed(identity: &str) -> impl Iterator<Item = &'static str> + '_ {
    LIST.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.rsplit_once(' '))
        .filter(move |(id, _)| *id == identity)
        .map(|(_, fp)| fp)
}

/// Whether `text` is this binary's copy of `file`, an older copy a build
/// shipped, or neither.
///
/// A file this binary does not ship at all (a retired skill) has no current
/// copy, so its listed fingerprints are all [`Known::Older`].
pub fn lookup(file: &Shipped, text: &str) -> Known {
    let fp = fingerprint(text);
    if file.current().is_some_and(|c| fingerprint(&c) == fp) {
        Known::Current
    } else if listed(&file.identity()).any(|known| known == fp) {
        Known::Older
    } else {
        Known::Unknown
    }
}

/// Real earlier copies of shipped files, for tests of the commands that
/// rewrite them. A fingerprint cannot be forged, so a test that needs "an
/// older copy" needs an actual one; each is a revision git history holds, kept
/// under `tests/fixtures/known-copies/`. [`tests`] checks every one still
/// looks up as [`Known::Older`].
#[cfg(test)]
pub(crate) mod older {
    use super::*;

    /// An earlier `SKILL.md` of `skill` for `provider`. Only the pairs the
    /// tests use have a fixture.
    pub fn skill(provider: Provider, skill: &str) -> &'static str {
        match (provider, skill) {
            (Provider::Claude, "spoolway-plan") => {
                include_str!("../tests/fixtures/known-copies/older-claude-spoolway-plan-SKILL.md")
            }
            (Provider::Codex, "spoolway-plan") => {
                include_str!("../tests/fixtures/known-copies/older-codex-spoolway-plan-SKILL.md")
            }
            (Provider::Pi, "spoolway-plan") => {
                include_str!("../tests/fixtures/known-copies/older-pi-spoolway-plan-SKILL.md")
            }
            (Provider::Claude, "spoolway-config") => {
                include_str!("../tests/fixtures/known-copies/older-claude-spoolway-config-SKILL.md")
            }
            _ => panic!("no older {skill} copy for {provider:?} is kept as a fixture"),
        }
    }

    /// An earlier pipeline file, fenced key block included.
    pub fn pipeline_file() -> &'static str {
        include_str!("../tests/fixtures/known-copies/older-pipeline.yml")
    }

    /// The key block of [`pipeline_file`].
    pub fn key_block() -> &'static str {
        crate::pipeline::KEY_BLOCK
            .read(pipeline_file())
            .expect("the fixture pipeline carries the key block")
    }

    /// An earlier reference table, as it stood above a `config.toml`.
    pub fn config_reference() -> &'static str {
        include_str!("../tests/fixtures/known-copies/older-config-reference.txt")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything this binary ships that the list has to cover.
    fn shipped() -> Vec<(Shipped, String)> {
        let mut all: Vec<(Shipped, String)> = crate::install::shipped_skill_files()
            .into_iter()
            .map(|s| {
                let file = Shipped::Skill {
                    provider: s.provider,
                    skill: s.skill.to_string(),
                    file: s.file,
                };
                (file, s.text.to_string())
            })
            .collect();
        all.push((
            Shipped::PipelineKeys,
            crate::pipeline::key_block().to_string(),
        ));
        all.push((Shipped::ConfigReference, crate::confkv::reference_table()));
        all
    }

    /// Every shipped file's current text has a line in the list, so a release
    /// never treats its own copy as edited.
    #[test]
    fn every_current_copy_is_listed() {
        let missing: Vec<String> = shipped()
            .iter()
            .filter(|(file, text)| !listed(&file.identity()).any(|f| f == fingerprint(text)))
            .map(|(file, text)| format!("{} {}", file.identity(), fingerprint(text)))
            .collect();
        assert!(
            missing.is_empty(),
            "assets/known-copies.txt lacks the current copy of a shipped file. \
             Append each of these lines to it, after the last line with the \
             same identity, so projects holding this copy are still recognised \
             as unedited:\n{}",
            missing.join("\n")
        );
    }

    /// The shipped text must classify as current, not merely as listed.
    #[test]
    fn current_copies_look_up_as_current() {
        for (file, text) in shipped() {
            assert_eq!(lookup(&file, &text), Known::Current, "{file:?}");
        }
    }

    /// Editing whitespace is not editing the file, matching `fingerprint`.
    #[test]
    fn whitespace_changes_do_not_change_the_answer() {
        let text = format!("{}\r\n\r\n", crate::pipeline::key_block());
        assert_eq!(lookup(&Shipped::PipelineKeys, &text), Known::Current);
    }

    /// Text no build shipped is neither current nor older.
    #[test]
    fn a_stranger_is_unknown() {
        assert_eq!(
            lookup(&Shipped::ConfigReference, "not a table anyone shipped"),
            Known::Unknown
        );
        let other = Shipped::Skill {
            provider: Provider::Claude,
            skill: "no-such-skill".to_string(),
            file: "SKILL.md".to_string(),
        };
        assert_eq!(lookup(&other, "anything"), Known::Unknown);
    }

    /// A released fixture's table is an older copy, which is the very case the
    /// list exists for.
    #[test]
    fn a_released_fixture_table_is_an_older_copy() {
        let toml = include_str!("../scripts/e2e/fixtures/0.6.0/.spoolway/config.toml");
        let table: Vec<&str> = toml.lines().take_while(|l| l.starts_with('#')).collect();
        assert_eq!(
            lookup(&Shipped::ConfigReference, &table.join("\n")),
            Known::Older
        );
    }

    /// A retired skill has no current copy, but its earlier ones are still
    /// known.
    #[test]
    fn a_retired_skill_has_only_older_copies() {
        let doctor = Shipped::Skill {
            provider: Provider::Claude,
            skill: "spoolway-doctor".to_string(),
            file: "SKILL.md".to_string(),
        };
        assert!(doctor.current().is_none());
        assert!(listed(&doctor.identity()).next().is_some());
    }

    /// A provider's copy of a skill asset is the Claude one, and is looked up
    /// as such: asking for it by the provider being installed for must not
    /// come back unknown for the text that provider is handed.
    #[test]
    fn an_asset_is_found_under_any_provider() {
        let text = include_str!("../assets/skills/claude/spoolway-plan/assets/template.html");
        for provider in [Provider::Claude, Provider::Codex, Provider::Pi] {
            let asset = Shipped::Skill {
                provider,
                skill: "spoolway-plan".to_string(),
                file: "assets/template.html".to_string(),
            };
            assert_eq!(lookup(&asset, text), Known::Current, "{provider:?}");
        }
    }

    /// The older copies the other modules' tests lean on are older copies, not
    /// the current text and not strangers. Without this a fixture that drifted
    /// out of the list would turn those tests into ones about refusal.
    #[test]
    fn the_older_fixtures_look_up_as_older() {
        for (provider, skill) in [
            (Provider::Claude, "spoolway-plan"),
            (Provider::Codex, "spoolway-plan"),
            (Provider::Pi, "spoolway-plan"),
            (Provider::Claude, "spoolway-config"),
        ] {
            let file = Shipped::Skill {
                provider,
                skill: skill.to_string(),
                file: "SKILL.md".to_string(),
            };
            assert_eq!(lookup(&file, older::skill(provider, skill)), Known::Older);
        }
        assert_eq!(
            lookup(&Shipped::PipelineKeys, older::key_block()),
            Known::Older
        );
        assert_eq!(
            lookup(&Shipped::ConfigReference, older::config_reference()),
            Known::Older
        );
    }

    /// The list's shape: a hand-appended line with a mistyped identity or a
    /// fingerprint cut short would never match, and every project holding that
    /// copy would later be treated as edited without any error saying so.
    #[test]
    fn the_list_is_well_formed() {
        let mut identities: Vec<String> = shipped().iter().map(|(f, _)| f.identity()).collect();
        for (name, _) in crate::install::RETIRED_SKILLS {
            for provider in [Provider::Claude, Provider::Codex, Provider::Pi] {
                identities.push(format!("skill {}/{name}/SKILL.md", provider.name()));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for line in LIST
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let (identity, fp) = line
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("{line:?} is not `<identity> <fingerprint>`"));
            assert!(
                fp.len() == 8 && fp.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
                "{line:?}: the fingerprint is not eight lowercase hex digits"
            );
            assert!(
                identities.iter().any(|i| i == identity),
                "{line:?}: no shipped or retired file is named {identity:?}"
            );
            assert!(seen.insert(line), "{line:?} appears twice");
        }
    }
}

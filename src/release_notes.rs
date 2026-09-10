//! The release record compiled into spoolway, and the two ways it is read.
//!
//! [`CHANGELOG`] is deliberately an `include_str!`: `whats-new` must work in a
//! directory with no checkout and an update must not add a GitHub request to
//! the command path. The small parser enforces the public contract written at
//! the top of `CHANGELOG.md`; keeping that contract structural rather than
//! accepting arbitrary Markdown lets a release fail its tests before a binary
//! with incomplete highlights or migration advice is published.

use anyhow::{Context, Result, bail};

/// The release history shipped in this exact binary.
pub const CHANGELOG: &str = include_str!("../CHANGELOG.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    pub fn parse(raw: &str) -> Result<Self> {
        let parts: Vec<&str> = raw.split('.').collect();
        if parts.len() != 3
            || parts.iter().any(|part| {
                part.is_empty()
                    || !part.bytes().all(|byte| byte.is_ascii_digit())
                    || (part.len() > 1 && part.starts_with('0'))
            })
        {
            bail!(
                "`{raw}` is not a version; use three canonical numeric components such as `0.2.0`."
            );
        }
        let numbers = parts
            .iter()
            .map(|part| part.parse::<u64>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| {
                format!(
                    "`{raw}` is outside the supported version range; use smaller numeric X.Y.Z components."
                )
            })?;
        Ok(Self(numbers[0], numbers[1], numbers[2]))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Section {
    heading: String,
    bullets: Vec<String>,
}

/// One validated release section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub theme: String,
    overview: String,
    highlights: Vec<String>,
    migrations: Vec<String>,
    details: Vec<Section>,
    url: String,
}

/// Parse and validate the embedded record. Results are always oldest first,
/// even though maintainers may keep the newest Markdown section at the top.
pub fn parse(text: &str) -> Result<Vec<Release>> {
    let mut starts = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if let Some(heading) = line.strip_prefix("## ") {
            starts.push((index, heading));
        }
    }
    if starts.is_empty() {
        bail!("The changelog contains no release sections; add one headed `## X.Y.Z — Theme`.");
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut releases = Vec::new();
    for (position, (start, heading)) in starts.iter().enumerate() {
        let end = starts
            .get(position + 1)
            .map(|(next, _)| *next)
            .unwrap_or(lines.len());
        releases.push(parse_release(heading, &lines[start + 1..end])?);
    }
    releases.sort_by_key(|release| release.version);
    for pair in releases.windows(2) {
        if pair[0].version == pair[1].version {
            bail!(
                "The changelog contains release {} more than once; keep exactly one section for each version.",
                pair[0].version
            );
        }
    }
    Ok(releases)
}

fn parse_release(heading: &str, lines: &[&str]) -> Result<Release> {
    let (raw_version, theme) = heading.split_once(" — ").with_context(|| {
        format!("Release heading `{heading}` is invalid; write it as `## X.Y.Z — Theme`.")
    })?;
    let version = Version::parse(raw_version)?;
    if theme.trim().is_empty() {
        bail!("Release {version} has no theme; add one after ` — ` in its heading.");
    }

    let mut cursor = 0;
    while lines.get(cursor).is_some_and(|line| line.trim().is_empty()) {
        cursor += 1;
    }
    let mut overview_lines = Vec::new();
    while let Some(line) = lines.get(cursor) {
        if line.trim().is_empty() || line.starts_with("### ") || line.starts_with("Release: ") {
            break;
        }
        overview_lines.push(line.trim());
        cursor += 1;
    }
    if overview_lines.is_empty() {
        bail!("Release {version} has no overview; add a non-empty paragraph below its heading.");
    }

    let mut sections = Vec::new();
    let mut url = None;
    while cursor < lines.len() {
        if lines[cursor].trim().is_empty() {
            cursor += 1;
            continue;
        }
        if let Some(value) = lines[cursor].strip_prefix("Release: ") {
            if url.replace(value.trim().to_string()).is_some() {
                bail!(
                    "Release {version} has more than one release URL; keep one final `Release: https://…` line."
                );
            }
            cursor += 1;
            if lines[cursor..].iter().any(|line| !line.trim().is_empty()) {
                bail!(
                    "Release {version} has content after its release URL; move the `Release: https://…` line to the end."
                );
            }
            continue;
        }
        let Some(name) = lines[cursor].strip_prefix("### ") else {
            bail!(
                "Release {version} has content outside a named section: `{}`; put it under a `### Heading` as `- ` bullets.",
                lines[cursor]
            );
        };
        if name.trim().is_empty() {
            bail!(
                "Release {version} contains an unnamed section; add text after its `###` marker."
            );
        }
        cursor += 1;
        let mut bullets = Vec::new();
        while cursor < lines.len()
            && !lines[cursor].starts_with("### ")
            && !lines[cursor].starts_with("Release: ")
        {
            if !lines[cursor].trim().is_empty() {
                let bullet = lines[cursor].strip_prefix("- ").with_context(|| {
                    format!(
                        "Release {version} section `{name}` has invalid content; write every item as a `- ` bullet."
                    )
                })?;
                if bullet.trim().is_empty() {
                    bail!(
                        "Release {version} section `{name}` contains an empty bullet; add its user-visible text or remove it."
                    );
                }
                bullets.push(bullet.trim().to_string());
            }
            cursor += 1;
        }
        if bullets.is_empty() {
            bail!(
                "Release {version} section `{name}` is empty; add a `- ` bullet or remove the section."
            );
        }
        sections.push(Section {
            heading: name.trim().to_string(),
            bullets,
        });
    }

    let highlight_index = sections
        .iter()
        .position(|section| section.heading == "Highlights")
        .with_context(|| {
            format!(
                "Release {version} has no Highlights section; add `### Highlights` with three to five bullets."
            )
        })?;
    if highlight_index != 0 {
        bail!(
            "Release {version} puts another section before Highlights; move `### Highlights` immediately after the overview."
        );
    }
    let highlights = sections.remove(highlight_index).bullets;
    if !(3..=5).contains(&highlights.len()) {
        bail!(
            "Release {version} has {} highlights; keep three to five `- ` bullets.",
            highlights.len()
        );
    }
    let migrations = match sections
        .iter()
        .position(|section| section.heading == "Breaking changes and migration")
    {
        Some(0) => sections.remove(0).bullets,
        Some(_) => bail!(
            "Release {version} puts another section before its migrations; move `### Breaking changes and migration` immediately after Highlights."
        ),
        None => Vec::new(),
    };
    if sections.iter().any(|section| {
        section.heading == "Highlights" || section.heading == "Breaking changes and migration"
    }) {
        bail!(
            "Release {version} repeats Highlights or Breaking changes and migration; merge each reserved heading into one section."
        );
    }
    let url = url.with_context(|| {
        format!(
            "Release {version} has no release URL; end it with `Release: https://github.com/marvingygas/spoolway/releases/tag/v{version}`."
        )
    })?;
    let expected = format!("https://github.com/marvingygas/spoolway/releases/tag/v{version}");
    if url != expected {
        bail!(
            "Release {version} has the wrong release URL; replace it with `Release: {expected}`."
        );
    }

    Ok(Release {
        version,
        theme: theme.trim().to_string(),
        overview: overview_lines.join(" "),
        highlights,
        migrations,
        details: sections,
        url,
    })
}

/// Select every release after `since`, in ascending order.
pub fn since(releases: &[Release], since: Version) -> Vec<&Release> {
    releases
        .iter()
        .filter(|release| release.version > since)
        .collect()
}

/// Render the installed release, or all releases after an explicit version.
pub fn whats_new(raw_since: Option<&str>) -> Result<String> {
    let releases = parse(CHANGELOG)?;
    let selected: Vec<&Release> = match raw_since {
        Some(raw) => since(&releases, Version::parse(raw)?),
        None => {
            let installed = Version::parse(crate::release::current())?;
            releases
                .iter()
                .filter(|release| release.version == installed)
                .collect()
        }
    };
    if selected.is_empty() {
        return Ok(match raw_since {
            Some(raw) => format!("No releases follow {raw}.\n"),
            None => format!(
                "No release notes are embedded for {}.\n",
                crate::release::current()
            ),
        });
    }
    Ok(render_full(&selected))
}

fn render_full(releases: &[&Release]) -> String {
    let mut out = String::new();
    for (index, release) in releases.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{} — {}\n\n", release.version, release.theme));
        out.push_str("Overview\n");
        out.push_str(&format!("  {}\n\n", release.overview));
        render_bullets(&mut out, "Highlights", &release.highlights);
        if !release.migrations.is_empty() {
            render_bullets(
                &mut out,
                "Breaking changes and migration",
                &release.migrations,
            );
        }
        for section in &release.details {
            render_bullets(&mut out, &section.heading, &section.bullets);
        }
        out.push_str(&format!("{}\n", release.url));
    }
    out
}

fn render_bullets(out: &mut String, heading: &str, bullets: &[String]) {
    out.push_str(heading);
    out.push('\n');
    for bullet in bullets {
        out.push_str(&format!("  • {bullet}\n"));
    }
    out.push('\n');
}

/// The terminal-only digest appended by the new process after an npm handover.
pub fn update_digest(previous: &str, terminal: bool) -> Result<Option<String>> {
    if !terminal {
        return Ok(None);
    }
    let previous = handover_version(previous)?;
    let installed = Version::parse(crate::release::current())?;
    let releases = parse(CHANGELOG)?;
    Ok(digest_for(&releases, previous, installed))
}

/// Read the lower end carried by the old binary. Version 0.1.0 shipped before
/// the environment value carried a version and used `1` only as a loop guard;
/// treating that one published sentinel as 0.1.0 lets its first upgrade show
/// notes without making any other malformed handover value ambiguous.
fn handover_version(raw: &str) -> Result<Version> {
    match raw {
        "1" => Version::parse("0.1.0"),
        _ => Version::parse(raw),
    }
}

fn digest_for(releases: &[Release], previous: Version, installed: Version) -> Option<String> {
    let crossed = since(releases, previous)
        .into_iter()
        .filter(|release| release.version <= installed)
        .collect::<Vec<_>>();
    if crossed.is_empty() {
        return None;
    }

    let mut out = format!("Updated spoolway {previous} → {installed}\n\n");
    if crossed.len() == 1 {
        let release = crossed[0];
        out.push_str(&format!("What's new — {}\n", release.theme));
        for highlight in &release.highlights {
            out.push_str(&format!("  • {highlight}\n"));
        }
        if !release.migrations.is_empty() {
            out.push_str("\nBreaking changes and migration\n");
            for migration in &release.migrations {
                out.push_str(&format!("  • {migration}\n"));
            }
        }
        out.push_str(&format!("\nFull notes: {}\n", release.url));
    } else {
        out.push_str("What's new\n");
        for release in &crossed {
            out.push_str(&format!("  {} — {}\n", release.version, release.theme));
        }
        let migrations: Vec<_> = crossed
            .iter()
            .filter(|release| !release.migrations.is_empty())
            .collect();
        if !migrations.is_empty() {
            out.push_str("\nBreaking changes and migration\n");
            for release in migrations {
                for migration in &release.migrations {
                    out.push_str(&format!("  • {}: {migration}\n", release.version));
                }
            }
        }
        out.push_str(&format!(
            "\nFull history: spoolway whats-new --since {previous}\n"
        ));
        for release in crossed {
            out.push_str(&format!("{}\n", release.url));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: &str = "# Changelog\n\n## 0.2.0 — Second\n\nOverview two.\n\n### Highlights\n- One\n- Two\n- Three\n\n### Breaking changes and migration\n- Rename old to new.\n\nRelease: https://github.com/marvingygas/spoolway/releases/tag/v0.2.0\n\n## 0.1.0 — First\n\nOverview one.\n\n### Highlights\n- A\n- B\n- C\n\nRelease: https://github.com/marvingygas/spoolway/releases/tag/v0.1.0\n";

    #[test]
    fn embedded_history_has_the_installed_release() {
        let releases = parse(CHANGELOG).unwrap();
        let installed = Version::parse(crate::release::current()).unwrap();
        assert!(releases.iter().any(|release| release.version == installed));
    }

    #[test]
    fn records_are_validated_and_sorted_oldest_first() {
        let releases = parse(TWO).unwrap();
        assert_eq!(
            releases
                .iter()
                .map(|r| r.version.to_string())
                .collect::<Vec<_>>(),
            ["0.1.0", "0.2.0"]
        );
        assert_eq!(releases[1].migrations, ["Rename old to new."]);
    }

    #[test]
    fn malformed_versions_and_records_are_refused() {
        for version in ["", "1", "1.2", "v1.2.3", "1.02.3", "1.2.3-beta", "1.+2.3"] {
            assert!(Version::parse(version).is_err(), "{version}");
        }
        assert!(parse(&TWO.replace("### Highlights", "### Changes")).is_err());
        assert!(parse(&TWO.replace("\n- Three", "")).is_err());
        assert!(parse(&TWO.replace("tag/v0.2.0", "tag/v9.9.9")).is_err());
        assert!(parse(&TWO.replace("### Highlights", "### \n\n### Highlights")).is_err());
        assert!(
            parse(&TWO.replace(
                "### Breaking changes and migration",
                "### Detail\n- More\n\n### Breaking changes and migration"
            ))
            .is_err()
        );
        assert!(parse(&TWO.replace("Overview two.\n", "Overview two.\n\nLoose text.\n")).is_err());
    }

    #[test]
    fn ranges_are_oldest_first_and_an_empty_range_says_so() {
        let releases = parse(TWO).unwrap();
        let selected = since(&releases, Version::parse("0.1.0").unwrap());
        assert_eq!(
            selected
                .iter()
                .map(|r| r.version.to_string())
                .collect::<Vec<_>>(),
            ["0.2.0"]
        );
        assert!(since(&releases, Version::parse("9.0.0").unwrap()).is_empty());
    }

    #[test]
    fn digest_is_terminal_gated_and_keeps_migrations() {
        assert_eq!(update_digest("0.0.0", false).unwrap(), None);
        let digest = update_digest("0.0.0", true).unwrap().unwrap();
        assert!(digest.contains("Updated spoolway 0.0.0 → 0.1.0"));
        assert!(digest.contains("What's new — Deterministic agent pipelines arrive"));
        assert!(digest.contains("Full notes: https://github.com/"));

        let releases = parse(TWO).unwrap();
        let one = digest_for(
            &releases,
            Version::parse("0.1.0").unwrap(),
            Version::parse("0.2.0").unwrap(),
        )
        .unwrap();
        assert!(one.contains("What's new — Second"));
        assert!(one.contains("  • One\n  • Two\n  • Three\n"));
        assert!(one.contains("Breaking changes and migration\n  • Rename old to new."));
    }

    #[test]
    fn the_legacy_v0_1_handover_sentinel_selects_that_release_as_previous() {
        let previous = handover_version("1").unwrap();
        assert_eq!(previous, Version::parse("0.1.0").unwrap());
        let releases = parse(TWO).unwrap();
        let digest = digest_for(&releases, previous, Version::parse("0.2.0").unwrap()).unwrap();
        assert!(digest.contains("Updated spoolway 0.1.0 → 0.2.0"));
        assert!(digest.contains("What's new — Second"));
        assert!(
            handover_version("2").is_err(),
            "only the published sentinel is compatible"
        );
    }

    #[test]
    fn a_multi_release_digest_keeps_themes_migrations_commands_and_links() {
        let releases = parse(TWO).unwrap();
        let digest = digest_for(
            &releases,
            Version::parse("0.0.0").unwrap(),
            Version::parse("0.2.0").unwrap(),
        )
        .unwrap();
        assert!(digest.contains("0.1.0 — First"));
        assert!(digest.contains("0.2.0 — Second"));
        assert!(digest.contains("0.2.0: Rename old to new."));
        assert!(digest.contains("spoolway whats-new --since 0.0.0"));
        assert_eq!(digest.matches("https://github.com/").count(), 2);
        assert!(
            !digest.contains("Overview one"),
            "multi-release output stays compact"
        );
    }
}

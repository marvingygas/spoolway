//! Path-glob comparison that belongs to no one domain.
//!
//! [`overlaps`] used to live in `src/docs.rs`, where it decided whether a
//! domain document's `covers` reached a changed path. `src/docs.rs` is gone —
//! spoolway keeps no notion of documentation any more, see
//! `assets/prompts/archivist/PROMPT.md` — but the check itself was never
//! about documents: it is a pure glob-overlap test, and `queue conflicts`
//! still needs it to tell a planner where two tasks' `touches` collide.

/// Whether two path globs can refer to overlapping files.
///
/// Compares the literal prefix each glob starts with — everything before its
/// first wildcard — and asks whether one is a path prefix of the other.
/// Deliberately approximate in the direction of saying yes: a false positive
/// puts one extra pair in front of `queue conflicts`, whereas full glob
/// intersection is a surprisingly deep problem for no real gain here.
pub fn overlaps(a: &str, b: &str) -> bool {
    let a = literal_prefix(a);
    let b = literal_prefix(b);
    // Segment by segment, so `src/cmd` is never a prefix of `src/cmdline` and
    // `src/cmd/hex.rs` is never a prefix of `src/cmd/hex.rs.bak`.
    let mut a = a.split('/').filter(|s| !s.is_empty());
    let mut b = b.split('/').filter(|s| !s.is_empty());
    loop {
        match (a.next(), b.next()) {
            (Some(x), Some(y)) if x != y => return false,
            (Some(_), Some(_)) => continue,
            // One ran out: everything it named, the other named too, so the
            // shorter one is a directory the longer one lives under.
            _ => return true,
        }
    }
}

fn literal_prefix(glob: &str) -> &str {
    let Some(end) = glob.find(['*', '?', '[']) else {
        // No wildcard at all: the glob is one literal path, and the whole of it
        // is its prefix. Backing off to the directory here would make any two
        // files in one directory look like they could be the same file.
        return glob;
    };
    // The segment holding the wildcard is only partly known, so drop it: `src/ap*`
    // must not appear to cover `src/api/` purely by sharing a partial segment.
    match glob[..end].rfind('/') {
        Some(slash) => &glob[..=slash],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_is_decided_on_whole_path_segments() {
        assert!(overlaps("src/api/**", "src/api/routes.rs"));
        assert!(overlaps("src/**", "src/api/routes.rs"));
        assert!(overlaps("src/api/**", "src/api/**"));

        assert!(!overlaps("src/api/**", "src/billing/**"));
        assert!(!overlaps("src/api/**", "docs/**"));

        // Two literal paths name one file each. Sharing a directory is not
        // sharing a file, and this is what decides whether two tasks writing
        // neighbouring files need a `depends_on` between them.
        assert!(overlaps("src/cmd/hex.rs", "src/cmd/hex.rs"));
        assert!(!overlaps("src/cmd/hex.rs", "src/cmd/slug.rs"));
        assert!(!overlaps("src/cmd/hex.rs", "src/cmd/hex.rs.bak"));
        assert!(!overlaps("Cargo.toml", "Cargo.lock"));
        // A literal directory still covers what lives under it.
        assert!(overlaps("src/cmd", "src/cmd/hex.rs"));
        assert!(!overlaps("src/cmd", "src/cmdline.rs"));
        // A bare wildcard reaches everything.
        assert!(overlaps("**", "src/cmd/hex.rs"));
    }
}

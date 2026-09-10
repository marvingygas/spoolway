# Release-note editor

## What you are looking at

Write the release record for the exact candidate proven by preflight. You produce one section, and
that single section becomes three things: the entry committed to `CHANGELOG.md`, the notes compiled
into every binary built from the tag, and the body of the GitHub release. There is no second, looser
copy anywhere, so this is the public explanation of why someone should upgrade — not a commit dump
and not internal project minutes. A person reviews it before the irreversible tag, and what they
approve is what ships byte-for-byte.

Read the release routine named in the task's References and every preflight handoff. Read the
contract at the top of `CHANGELOG.md` in full before drafting; the binary parses that structure and a
section that breaks it fails the repository's own tests. Verify important claims against the diff
from the previous tag to the recorded main commit.

## How to do it here

1. Decide the next semver version from the evidence. Challenge the preflight recommendation when a
   changed default, renamed key, removed flag, compatibility break, or new behaviour points to a
   different bump under the repository's major-zero policy.
2. Write `release-notes.md` in the scratch directory named under WHAT YOU HAVE. It holds exactly one
   changelog section and nothing else — no preamble, no trailing commentary — because the publisher
   inserts the file's bytes into `CHANGELOG.md` unchanged. Follow the contract at the top of that
   file exactly:
   - `## X.Y.Z — Theme` as the first line, with a real em dash and a theme that says what the release
     is about in a few words;
   - one overview paragraph on the line or lines immediately below, two to three sentences, focused
     on user outcomes rather than internals, and unbroken by a blank line;
   - `### Highlights` next, with three to five `- ` bullets ordered by impact, each explaining the
     benefit before the implementation and carrying its pull-request number;
   - `### Breaking changes and migration` immediately after Highlights when migration work exists,
     with exact before/after names, commands, defaults, or configuration and a copyable migration
     path in every bullet; omit the whole section only after proving in your handoff that nothing
     in the diff breaks an existing user;
   - further `### ` sections for features, fixes, reliability and performance, platform and packaging
     work, contributor credit, and upgrading, in that order, including only sections that carry
     meaningful material;
   - an upgrade section whose bullets carry copyable npm installation or update commands and name the
     supported artifacts;
   - a contributor section crediting the people preflight named, with the relevant pull requests, so
     the record stays traceable without becoming a raw list; and
   - `Release: https://github.com/marvingygas/spoolway/releases/tag/vX.Y.Z` as the final line.
3. Every line inside a `### ` section is a `- ` bullet. The parser accepts nothing else there — no
   sub-headings, no fenced code blocks, no loose paragraphs, no blank bullets — so put commands in
   backticks inside a bullet and split a long thought into two bullets rather than a paragraph. Read
   your draft against the contract line by line before handing off; a structural mistake here is
   found by the repository's tests only after the publisher has already committed it.
4. Match the house style of the previous section preflight reported. This file is read as a history,
   so a reader moving from one version to the next should not feel the voice change.
5. Make every claim falsifiable. Preserve exact CLI, config, pipeline, model, file-format, and platform
   names from the released tree. Quantify improvements only when the repository contains evidence.
6. Write for three readers at once: a new user scanning the overview, an existing user checking for
   migration work, and a maintainer looking for traceability. Put the critical upgrade risk before
   the long detail.
7. Re-read the notes against the complete diff. Remove hype, repeated points, implementation trivia,
   empty sections, and claims that cannot be linked to code, docs, an issue, or a pull request.
8. Hand off the proposed version, candidate commit, scratch-file path, a one-line release theme, and
   every migration item the approving person must inspect. State plainly whether a
   `### Breaking changes and migration` section is present, and when it is absent name the evidence
   that proves no user-facing break exists in this candidate. The person at the gate is approving
   this exact text as the committed changelog entry and the published release body, so say so and
   quote the section in full for them.

## Never

- Never modify the repository, bump the manifest, push, tag, publish, or edit an existing release.
- Never claim a workflow, package, archive, checksum, or install succeeded before publishing verifies it.
- Never bury a breaking change among highlights or label a compatibility change as an internal detail.
- Never use generic phrases such as “various improvements,” copy commit subjects verbatim as prose,
  or invent benchmarks, users, adoption, security impact, or contributor intent.
- Never treat missing archived task files as a gap in the changes; the diff and the merged pull
  requests are the coverage proof, and a task title is never publishable prose on its own.
- Never write anything into `release-notes.md` outside the single section, and never carry a section
  approved for an earlier candidate forward when preflight has run again on a moved main.
- Never pass without a complete, non-empty `release-notes.md` grounded in the approved candidate.

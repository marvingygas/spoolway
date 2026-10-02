//! Files spoolway writes into a project. Embedded in the binary so that the CLI
//! is also the installer — `spoolway init` needs nothing on disk to work from.

/// One shipped prompt: a whole file, and not one byte spoolway parses.
///
/// A prompt is prose about a role — what it reads, what it judges, what this
/// project's bar is. The mechanics of a lane are not in here and never reach a
/// file a project also writes in: the dispatcher composes them around this prose
/// at launch — its framing before, this pass's policy after — into a system
/// prompt that is spoolway's outright and changes when spoolway changes. Which
/// is also what stops a project losing them by rewriting its prompts.
///
/// The consequence is deliberate: `spoolway sync` never touches a prompt.
/// These are the defaults a project starts from, and after `init` they are the
/// project's — to sharpen, to rewrite, or to leave. Anyone wanting today's
/// defaults runs `init` in a scratch directory and copies what they like.
///
/// Each lives in a directory of its own, holding a `PROMPT.md` and — where the
/// role has belongings — an `assets/`, the way a skill is laid out. The
/// directory is what makes [`Prompt::assets`] possible at all: a role with a
/// skeleton to fill needs somewhere to keep it that is unmistakably *its*.
pub struct Prompt {
    pub name: &'static str,
    pub body: &'static str,
    /// Files written to the prompt's own `assets/`, by filename.
    ///
    /// These inherit the prompt's rules rather than a skeleton's: `init`
    /// writes them once, `sync` never touches them, and after that they are
    /// the project's. A project restyles its pages by editing these in place,
    /// which is why there is no setting naming them — the prompt that fills
    /// them is the only thing that reads them, and it knows where its own
    /// assets are.
    pub assets: &'static [(&'static str, &'static str)],
}

/// The file a prompt's prose lives in, inside its directory.
pub const PROMPT_FILE: &str = "PROMPT.md";

/// The directory a prompt keeps its belongings in, inside its directory.
pub const PROMPT_ASSETS: &str = "assets";

/// Prompts, by directory name. A pipeline step's `prompt:` selects one.
pub const PROMPTS: &[Prompt] = &[
    Prompt {
        name: "implementer",
        body: include_str!("../assets/prompts/implementer/PROMPT.md"),
        assets: &[],
    },
    Prompt {
        name: "reviewer",
        body: include_str!("../assets/prompts/reviewer/PROMPT.md"),
        assets: &[],
    },
    // An `e2e` prompt shipped here once, named by no step since the default
    // pipeline dropped its e2e step. A role nothing runs is a role nothing
    // keeps honest: it was written against every project at once — hunt for
    // Playwright, or Cypress, or Detox — which is advice for a project spoolway
    // has never seen rather than a role. A project that adds the step back
    // writes the prompt beside it, against the harness it actually has.
    //
    // The bugfix pipeline's only new role. Runs twice — capture the bug as a
    // failing repro, then run that same repro after the fix.
    Prompt {
        name: "reproducer",
        body: include_str!("../assets/prompts/reproducer/PROMPT.md"),
        assets: &[],
    },
    // It writes pages, and a page needs a shape. One skeleton per kind of page,
    // because "one per domain" and "one per project" are different documents
    // with different rules, and a single skeleton serving both would state
    // neither.
    Prompt {
        name: "archivist",
        body: include_str!("../assets/prompts/archivist/PROMPT.md"),
        assets: &[
            (
                "document.md",
                include_str!("../assets/prompts/archivist/assets/document.md"),
            ),
            (
                "landing-page.md",
                include_str!("../assets/prompts/archivist/assets/landing-page.md"),
            ),
        ],
    },
    // A sample, not a mechanism: the binary knows the step id `blocked`, not
    // this prompt's name — nothing in `src/` outside this file spells
    // `unblocker`. Staffed only in an unattended run, on a pipeline that
    // declares `blocked` as a step; the shipped `default` and `bugfix`
    // pipelines both do, with `session: true`, so the same conversation that
    // hit a blocker is the one asked to clear it.
    Prompt {
        name: "unblocker",
        body: include_str!("../assets/prompts/unblocker/PROMPT.md"),
        assets: &[],
    },
];

pub fn prompt(name: &str) -> Option<&'static Prompt> {
    PROMPTS.iter().find(|prompt| prompt.name == name)
}

/// The skeletons a task file is written from, by pipeline name.
///
/// Only the markdown half is here; the block documenting what spoolway will do
/// to a task later is rendered from [`crate::task_template`] when the file is
/// written, so there is one copy of that text in the tree and no asset that can
/// fall behind the code generating it.
///
/// `default` answers for any pipeline with no file of its own, which is what
/// makes adding a pipeline cost no configuration: write `<pipeline>.md` beside
/// these to give it a shape, or write nothing and take this one.
pub const TASK_TEMPLATES: &[(&str, &str)] = &[
    ("default", include_str!("../assets/tasks/default.md")),
    // A bug is reported, not designed: what goes wrong and how to see it, so
    // the reproduce step has something to start from.
    ("bugfix", include_str!("../assets/tasks/bugfix.md")),
];

pub fn task_template(name: &str) -> Option<&'static str> {
    TASK_TEMPLATES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, text)| *text)
}

/// The two ticket-body templates a project's `.spoolway/templates/tracking/`
/// starts from, by name — seeded by `spoolway init` the way a task
/// skeleton's own shipped default is seeded into `.spoolway/templates/tasks/`.
/// `spoolway sync` never touches either, once `init` has written them: the
/// same rule a task skeleton or a prompt already follows.
///
/// [`crate::task_template::resolve_tracking`] never falls back to these at
/// render time — a project with neither file written gets a single line
/// naming the task instead, never this prose silently standing in for its
/// own. Neither carries the word "spoolway": the rendered body is the
/// project's own ticket, not a spoolway one — see this module's own test.
pub const TRACKING_TEMPLATES: &[(&str, &str)] = &[
    ("epic", include_str!("../assets/tracking/epic.md")),
    ("ticket", include_str!("../assets/tracking/ticket.md")),
];

/// Read one of [`TRACKING_TEMPLATES`] by name — what `crate::update`'s own
/// `shipped_for` hands `--replace` for a tracking template, the same way it
/// already does for a task skeleton and a prompt; `init` itself still places
/// the pair by iterating the slice above directly, and `resolve_tracking`
/// never falls back to either by name.
pub fn tracking_template(name: &str) -> Option<&'static str> {
    TRACKING_TEMPLATES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, text)| *text)
}

/// The two hook scripts `spoolway init` writes into `.spoolway/hooks/`, one
/// per tracker, each calling the tracker's own command-line tool (`gh` or
/// `acli`) rather than any tracker's HTTP API directly. Both are written
/// only when a tracker is chosen, and with `none` there is no `hooks/`
/// folder at all. Both rather than only the chosen one's, so switching
/// between trackers later is a `spoolway config set issue_tracking.hook`
/// away, not a second `init`.
///
/// `spoolway sync` never touches a hook it has already written, the same
/// rule a prompt or a task skeleton already follows once a project has made
/// a file its own.
///
/// `github.sh` treats GitHub as a mirror: `open` opens the group's epic (if
/// any) and the task's own ticket, `started` labels the ticket in progress
/// once the task actually leaves `queued`,
/// `blocked`/`paused` comment a snapshot of the task file's own status log
/// and handoff, and `done` — reached when `spoolway stack` has already
/// opened this task's own pull request, not when it merges — leaves a
/// `<!-- spoolway-issue: URL -->` marker comment on that pull request and
/// relabels the ticket for review. Closing the ticket itself is left to
/// the project. Spoolway's own repository does it with
/// `.github/workflows/spoolway-issues.yml`, which `init` no longer ships:
/// it trusts only a marker left by an owner, member or collaborator,
/// reads the pull request's `merged` event, and closes the ticket — and,
/// once every child of a group has closed, the group's own epic — from
/// there. A project without it closes the ticket by hand after the merge.
/// This whole design, `open` through `done`, was run against a real
/// repository: the issues it creates, the parent link, the labels, the
/// marker comment and the workflow's own close all landed, four times over,
/// closing a real group and the tasks inside it.
///
/// `jira.sh` opens one Story per group, a group of one included, and one
/// Sub-task per task under it — `Blocks` links a Sub-task to the task its
/// own `depends_on` names, `Relates` links the Story to a `…/browse/<key>`
/// source. `open`, `started` and `done` were all run live against a real
/// Jira site: creating the Story and its Sub-tasks, the `Blocks` and
/// `Relates` links, the label union on the Story, and the Sub-task's and
/// Story's own moves to Review on `done` all landed there. `started`'s move
/// to `status_progress` did not — that site's own workflow wires no
/// transition into its "In Progress" status from Draft at all, on any issue
/// type, so every `started` attempt there was a proven no-op rather than a
/// landed move; a site whose workflow does wire that transition in is what
/// would actually show it moving. That run is also what moved the version
/// floor to `acli 1.3.39` —
/// `workitem view` took its key as `--key` at 1.3.30 and takes it
/// positionally now, a breaking change between the two — and what found
/// that `.self` on a viewed issue names the API's own backend host, not the
/// host a browser can reach, so `url=` and every browse link this hook
/// writes read the site back off `acli jira auth status` instead. Spoolway's
/// own part stops at Review: `jira.sh` never sets Resolved, and closing a
/// ticket is left to whatever the project wires up on merge, the same way
/// `github.sh` leaves it to the project. `jq` is a hard
/// dependency of the Jira pair alongside `acli` itself — the only way
/// `workitem create --json` hands a ticket's key back — and a create call
/// that comes back with no key exits loudly rather than writing an empty
/// `epic=`/`ticket=` line.
///
/// The Jira pair does not send the task file. Jira's REST API would take it as
/// a real attachment, but only against a site, an account email and an API
/// token, and spoolway holds no credentials of its own — `gh` and `acli` each
/// carry their own. `acli` cannot upload one either: its `workitem attachment`
/// group only lists and deletes. So the Jira comment names the task and leaves
/// the file in the queue, while the GitHub pair, which needs nothing beyond
/// the `gh` login a project already has, still folds it into the comment.
///
/// `github.sh` carries one line `.spoolway/hooks/github.sh` (the file this
/// repository actually runs, and what `github.sh` here is a byte-for-byte
/// port of) does not: `# spoolway-requires: gh >= 2.97.0`, restored because
/// the marker-and-label design otherwise ships with no machine-readable
/// version floor at all — `spoolway doctor` and the submit-time gate
/// (`scripts/e2e/suites/issue-tracking.sh`'s "gh below the floor" block) both
/// read it, and read nothing back once it is gone, which stops warning silently
/// rather than failing loudly. `.spoolway/hooks/github.sh` itself still wants
/// this line added to stay in step.
pub const HOOK_SCRIPTS: &[(&str, &str)] = &[
    ("github.sh", include_str!("../assets/hooks/github.sh")),
    ("jira.sh", include_str!("../assets/hooks/jira.sh")),
];

/// The markers around spoolway's rules in the project's `.gitignore`.
///
/// A project already has a `.gitignore`, or wants one at its root — so the rules
/// go there rather than into a second file two directories down that nobody
/// remembers is there. The markers are what make that possible without merging:
/// [`crate::gitignore`] rewrites what is between them and never reads a line the
/// project wrote around them.
pub const IGNORE_BEGIN: &str = "# >>> spoolway >>>";
pub const IGNORE_END: &str = "# <<< spoolway <<<";

/// The markers around the key reference in a pipeline file.
///
/// The same pair, deliberately: both fence a block spoolway rewrites inside a
/// file the project owns the rest of, and both are `#` comments in a format
/// that has no element to close. A second spelling would be a second thing to
/// recognise for one idea — see [`crate::skeleton::Region::Comment`].
pub const PIPELINE_KEYS_BEGIN: &str = IGNORE_BEGIN;
pub const PIPELINE_KEYS_END: &str = IGNORE_END;

#[cfg(test)]
mod tests {
    use super::*;

    /// One of [`HOOK_SCRIPTS`] by name, panicking on a typo rather than
    /// silently comparing against nothing.
    fn hook_script(name: &str) -> &'static str {
        HOOK_SCRIPTS
            .iter()
            .find(|(known, _)| *known == name)
            .unwrap_or_else(|| panic!("no shipped hook script named {name}"))
            .1
    }

    /// The GitHub hook hands a ticket to its own pull request at `done`
    /// rather than closing anything — see the `done` branch of the script.
    /// A `gh issue close` anywhere in it would silently undo that, so it is
    /// refused outright rather than left to a live-repo run nobody in CI
    /// can make.
    #[test]
    fn shipped_github_hook_never_closes_an_issue() {
        let script = hook_script("github.sh");
        assert!(
            !script.contains("issue close"),
            "github.sh still closes a GitHub issue"
        );
    }

    /// `spoolway doctor` and the submit-time version gate both read this
    /// declaration back through `crate::tracking::required_tools` — see
    /// [`HOOK_SCRIPTS`]'s own doc for why it is restored here even though
    /// `.spoolway/hooks/github.sh` does not carry it (yet).
    #[test]
    fn shipped_github_hook_declares_its_gh_version_floor() {
        let script = hook_script("github.sh");
        assert!(
            script.contains("# spoolway-requires: gh >= 2.97.0"),
            "github.sh no longer declares the gh version its own commands were checked against"
        );
    }

    /// `hand_off_for_review` — the `done` branch — leaves the marker comment
    /// a close-on-merge workflow trusts on the pull request, relabels the
    /// ticket for review, and tells it where to find the pull request,
    /// checking every `gh` call for failure rather than treating a failed
    /// lookup, comment or edit as nothing to react to. Static substring
    /// checks can only prove these markers are present, not that the
    /// script's logic is correct on its own — see `tracking::tests` for
    /// execution-level proof of `github.sh`'s `done` behavior.
    #[test]
    fn github_sh_hands_off_to_a_pull_request_for_review() {
        let sh = hook_script("github.sh");
        for marker in [
            "spoolway-issue:",
            "gh pr view",
            "gh pr comment",
            "gh issue edit",
            "--remove-label spoolway:in-progress",
            "--add-label spoolway:review",
            "gh issue comment",
            "ready for review",
            "no pull request found",
        ] {
            assert!(sh.contains(marker), "hook script drops `{marker}`");
        }
        // The branch lookup, the marker comment, the label swap and the
        // closing comment are each bare now: `set -eE` and the ERR trap at
        // the top of the file stop the handoff on any one of them failing,
        // tracing the command that did — `|| exit $?` would hide that trace,
        // since bash never fires an ERR trap for a command inside an `||`
        // list, so none may appear in this function's own body.
        let done_branch = sh
            .split("hand_off_for_review() {")
            .nth(1)
            .expect("github.sh still defines hand_off_for_review");
        let done_branch = &done_branch[..done_branch.find("\n}\n").unwrap_or(done_branch.len())];
        assert!(
            !done_branch.contains("|| exit $?"),
            "github.sh's done branch must rely on the ERR trap, not `|| exit $?`, to stop on \
             a failed call"
        );
        assert!(
            sh.contains("set -eE") && sh.contains("trap 'hook_trace"),
            "github.sh no longer runs under set -eE with its own ERR trap"
        );
        // The branch lookup can also succeed with nothing to report — a
        // `done` this hook fires for always has a pull request behind it by
        // then, so that has to fail too rather than read as "nothing to do".
        assert!(
            sh.contains("[ -n \"$pr\" ]"),
            "github.sh no longer treats a missing pull request as a failure"
        );
        // `done` here is the hand-off, not a merge — GitHub itself is what
        // closes the ticket, once the pull request the marker names
        // actually merges.
        assert!(
            sh.contains("GitHub will close this issue after the pull request merges"),
            "github.sh's own comment no longer explains that GitHub closes the ticket, not \
             this hook"
        );
    }

    /// Spoolway's part ends at Review — `jira.sh` never sets Resolved, and
    /// the epic no longer moves to Done on a group's last `done` the way it
    /// once did; it moves to `status_review` instead, the same status the
    /// Sub-task itself lands on.
    #[test]
    fn shipped_jira_hook_never_sets_resolved_or_done() {
        let script = hook_script("jira.sh");
        assert!(
            !script.contains("--status Done") && !script.contains("--status Resolved"),
            "jira.sh still sets a closing status itself — that is the user's own merge \
             automation to wire"
        );
        assert!(
            script.contains("$SPOOLWAY_GROUP_LAST") && script.contains("status_review"),
            "jira.sh no longer moves the Story to review on a group's last `done`"
        );
    }

    /// `jira.sh`'s own Markdown-to-ADF converter, pulled out of its shipped
    /// source rather than copied into this test a second time — a change to
    /// the heredoc below is exercised here automatically, with nothing to
    /// keep in sync by hand. The program lives between the single-quoted
    /// heredoc marker and the line that closes it.
    fn adf_filter() -> &'static str {
        let script = hook_script("jira.sh");
        let open_marker = script
            .find("<<'JQ'")
            .expect("jira.sh no longer opens its ADF_FILTER heredoc");
        let start = script[open_marker..]
            .find('\n')
            .map(|offset| open_marker + offset + 1)
            .expect("jira.sh's ADF_FILTER heredoc opener has no end of line");
        let rest = &script[start..];
        let end = rest
            .find("\nJQ\n")
            .expect("jira.sh no longer closes its ADF_FILTER heredoc");
        &rest[..end]
    }

    /// Runs `$md` through [`adf_filter`] with a real `jq`, the same way
    /// `to_adf_file` in `jira.sh` does, and returns the parsed ADF document.
    /// `jq` is one of the hook's own declared hard dependencies (see the
    /// script's header), so a test environment missing it fails loudly
    /// rather than skipping a check it never ran.
    fn run_adf_filter(md: &str) -> serde_json::Value {
        use std::process::{Command, Stdio};
        let output = Command::new("jq")
            .args(["-n", "--arg", "md"])
            .arg(md)
            .arg(adf_filter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("jq must be on PATH to run this test — it is jira.sh's own hard dependency");
        assert!(
            output.status.success(),
            "jq rejected this Markdown: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("jq's own output is not valid JSON: {e}"))
    }

    /// Every `text` leaf anywhere in an ADF document, concatenated with a
    /// space between — what a reader would actually see written out.
    fn adf_text(value: &serde_json::Value) -> String {
        let mut out = Vec::new();
        fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("type").and_then(|t| t.as_str()) == Some("text")
                        && let Some(text) = map.get("text").and_then(|t| t.as_str())
                    {
                        out.push(text.to_string());
                    }
                    for v in map.values() {
                        walk(v, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for v in items {
                        walk(v, out);
                    }
                }
                _ => {}
            }
        }
        walk(value, &mut out);
        out.join(" ")
    }

    /// Every `link` mark's `href` anywhere in an ADF document, in the order
    /// they appear — `adf_text` only ever sees a link's visible text, never
    /// the address it points at, so the loss check below needs this to
    /// prove a link's `[text](url)` survives as a whole, not just its text.
    fn adf_hrefs(value: &serde_json::Value) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("type").and_then(|t| t.as_str()) == Some("link")
                        && let Some(href) = map
                            .get("attrs")
                            .and_then(|a| a.get("href"))
                            .and_then(|h| h.as_str())
                    {
                        out.push(href.to_string());
                    }
                    for v in map.values() {
                        walk(v, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for v in items {
                        walk(v, out);
                    }
                }
                _ => {}
            }
        }
        walk(value, &mut out);
        out
    }

    /// The visible prose of a Markdown section this converter is meant to
    /// handle, with every syntax marker it consumes removed and every
    /// `[text](url)` link's address pulled out on its own — so what is left
    /// is exactly the characters a reader would see on the page, in order,
    /// with its link addresses alongside, never mixed into the text.
    ///
    /// Whitespace is dropped last, rather than used to split words: a
    /// bulletList's own `listItem`s each render as a *separate* ADF node
    /// [`adf_text`] joins back together with an inserted space, which can
    /// land right next to punctuation the source never put a space before
    /// (a code span followed by a comma, say). Comparing word by word would
    /// read that reformatting as a dropped or split word; comparing with no
    /// whitespace at all compares content only, in the order it appears,
    /// immune to exactly how much space sits between two nodes.
    ///
    /// A fenced code block's own lines pass through untouched — no heading,
    /// bullet or mark stripping inside one, since a shell comment's leading
    /// `#` or a snippet's own backtick is the block's real content, not
    /// Markdown syntax this converter interprets — and the fence markers
    /// themselves (`` ``` ``, with or without a language) are dropped, the
    /// same way the rendered `codeBlock` carries only what was inside them.
    ///
    /// Link, bold and inline-code markers are resolved on each finished
    /// block's *whole* accumulated text, not line by line: the archive
    /// wraps a long bullet or numbered item across source lines, so a
    /// `` `backtick span` `` or a link can open on one line and close on the
    /// next. `jira.sh`'s own converter sees the same lines and only runs
    /// its own inline scan once a block's lines are joined with `" "`
    /// (`md_to_adf`'s `buf | join(" ") | inline_nodes`) — so matching that
    /// order here is what makes a span crossing a source line resolve on
    /// both sides instead of only on the real one.
    ///
    /// Which block a line belongs to, and whether its own leading `##`,
    /// `-` or `1.` is a real marker to strip or part of the prose, is
    /// decided by the exact same, indentation-sensitive rules `md_to_adf`
    /// itself uses — an indented line such as `    ## not a heading`,
    /// four spaces deep in an *indented* code block this converter does
    /// not implement at all, starts with none of those at column zero, so
    /// neither side treats it as one; a line-by-line approximation that
    /// trims leading space before checking got this wrong (review finding
    /// in this task's own history). Reusing the real decision, rather than
    /// a second guess at it, is the only way this check can trust a
    /// mismatch to mean an actual loss.
    // The final `flush_list!()` call at end of input sets `list_active =
    // false` with nothing left to read it afterward — correct, since every
    // other call site needs that reset before the next line is classified.
    #[allow(unused_assignments)]
    fn visible_text_and_links(markdown: &str) -> (String, Vec<String>) {
        let top_bullet = regex::Regex::new(r"^(?:-|[0-9]+\.) ").unwrap();
        let nested_bullet = regex::Regex::new(r"^  +(?:-|[0-9]+\.) ").unwrap();

        // One left-to-right tokenizer, not three separate find-and-replace
        // passes — a pass-per-mark order can't mirror `scan`'s own
        // priority, where a `` `code span` `` already claims everything up
        // to its own closing backtick, `**` included, before the bold
        // alternative is ever tried at that position. Three sequential
        // passes strip the backticks around `` `assets/**` `` first (as
        // `code`), which then exposes its bare `**` to the *next* pass as
        // if it were real bold syntax — the same two markers
        // [`visible_text_and_links`]'s own doc already names, wrongly
        // merged a second time by passes instead of a scan. Named capture
        // groups say which alternative actually matched the leftmost spot;
        // the gap between one match and the next is literal text, kept
        // exactly as it is, covering a stray `*`, `` ` `` or `[` the same
        // way the real converter's own catch-all alternatives do.
        let token = regex::Regex::new(concat!(
            r"(?P<code>`[^`]*`)",
            r"|(?P<bold>\*\*(?:[^*\s][^*]*[^*\s]|[^*\s])\*\*)",
            r"|\[(?P<ltext>[^\]]*)\]\((?P<lhref>[^)]*)\)",
        ))
        .unwrap();

        let mut blocks: Vec<String> = Vec::new();
        let mut hrefs: Vec<String> = Vec::new();
        let mut buf = String::new();
        let mut list_items: Vec<String> = Vec::new();
        let mut list_active = false;
        let mut in_code = false;
        let mut code_lines: Vec<&str> = Vec::new();

        let inline = |raw: &str, hrefs: &mut Vec<String>| -> String {
            let mut out = String::new();
            let mut last = 0;
            for caps in token.captures_iter(raw) {
                let whole = caps.get(0).unwrap();
                out.push_str(&raw[last..whole.start()]);
                if let Some(c) = caps.name("code") {
                    let s = c.as_str();
                    out.push_str(&s[1..s.len() - 1]);
                } else if let Some(b) = caps.name("bold") {
                    let s = b.as_str();
                    out.push_str(&s[2..s.len() - 2]);
                } else if let Some(t) = caps.name("ltext") {
                    out.push_str(t.as_str());
                    hrefs.push(caps.name("lhref").map_or("", |h| h.as_str()).to_string());
                }
                last = whole.end();
            }
            out.push_str(&raw[last..]);
            out
        };

        macro_rules! flush_para {
            () => {
                if !buf.trim().is_empty() {
                    blocks.push(inline(&buf, &mut hrefs));
                }
                buf.clear();
            };
        }
        macro_rules! flush_list {
            () => {
                for item in list_items.drain(..) {
                    blocks.push(inline(&item, &mut hrefs));
                }
                list_active = false;
            };
        }

        for line in markdown.lines() {
            if line.starts_with("```") {
                if in_code {
                    blocks.push(code_lines.join("\n"));
                    code_lines.clear();
                    in_code = false;
                } else {
                    flush_para!();
                    flush_list!();
                    in_code = true;
                }
                continue;
            }
            if in_code {
                code_lines.push(line);
                continue;
            }
            if let Some(rest) = line.strip_prefix("## ") {
                flush_para!();
                flush_list!();
                blocks.push(inline(rest, &mut hrefs));
            } else if let Some(m) = top_bullet.find(line) {
                if !list_active {
                    flush_para!();
                }
                list_active = true;
                list_items.push(line[m.end()..].to_string());
            } else if list_active && let Some(m) = nested_bullet.find(line) {
                list_items.push(line[m.end()..].to_string());
            } else if line.trim().is_empty() {
                flush_para!();
                flush_list!();
            } else if list_active {
                let last = list_items.last_mut().expect("list_active implies an item");
                last.push(' ');
                last.push_str(line.trim_start());
            } else {
                buf.push(' ');
                buf.push_str(line);
            }
        }
        flush_para!();
        flush_list!();
        if in_code {
            blocks.push(code_lines.join("\n"));
        }

        (blocks.join(" "), hrefs)
    }

    /// All whitespace removed — see [`visible_text_and_links`]'s own doc for
    /// why that, not a word split, is what the loss check below compares.
    fn squash(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The Context and Acceptance criteria of every task this project has
    /// ever archived, captured as `tests/fixtures/adf_corpus.json` so the
    /// check below runs the same way on every machine rather than reading a
    /// live, local `archive/` directory that is a dispatcher's own working
    /// state and not part of the repository. `jira.sh`'s converter must turn
    /// every one of them into valid ADF that reproduces the same visible
    /// characters, in the same order, with the same link addresses — not
    /// merely as many characters, which a converter that swapped, duplicated
    /// or re-split words could still pass.
    #[test]
    fn jira_sh_adf_filter_loses_no_text_from_any_archived_task() {
        #[derive(serde::Deserialize)]
        struct Section {
            task: String,
            heading: String,
            markdown: String,
        }
        let corpus: Vec<Section> =
            serde_json::from_str(include_str!("../tests/fixtures/adf_corpus.json"))
                .expect("tests/fixtures/adf_corpus.json must be a JSON array of sections");
        assert!(!corpus.is_empty(), "the ADF corpus fixture is empty");

        for section in &corpus {
            let doc = run_adf_filter(&section.markdown);
            assert_eq!(
                doc.get("type").and_then(|t| t.as_str()),
                Some("doc"),
                "{}/{}: not an ADF document: {doc}",
                section.task,
                section.heading
            );
            let (original_text, original_hrefs) = visible_text_and_links(&section.markdown);
            let rendered_hrefs = adf_hrefs(&doc);
            assert_eq!(
                squash(&original_text),
                squash(&adf_text(&doc)),
                "{}/{}: the rendered ADF's visible text does not match the source, in order",
                section.task,
                section.heading
            );
            assert_eq!(
                original_hrefs, rendered_hrefs,
                "{}/{}: the rendered ADF's link addresses do not match the source, in order",
                section.task, section.heading
            );
        }
    }

    /// The one difference `.spoolway/hooks/jira.sh` (this project's own
    /// control plane, not the binary it builds) is allowed to carry against
    /// the shipped `jira.sh` above — a guard this project added because it
    /// moved to Jira after some of its own tickets were already GitHub
    /// issue URLs, which `acli` can never resolve. Kept here as its own
    /// constant, rather than inlined into the test below, so the one
    /// sanctioned drift is named in exactly one place.
    const JIRA_KAN_GUARD: &str = r#"
# A task queued before this project moved to Jira still carries a GitHub
# issue URL as its ticket — skip it, since acli can never resolve that key.
case "$SPOOLWAY_TICKET" in
  "$project"-[0-9]*) ;;
  *) exit 0 ;;
esac
"#;

    /// `.spoolway/hooks/github.sh` and `.spoolway/hooks/jira.sh` are this
    /// project's own copies of the hooks it ships to every other project —
    /// proof that the shipped scripts actually run, not a second
    /// implementation of them. A copy that drifts hides a fix from this
    /// project the moment it diverges, which is what sent both scripts
    /// out of sync before this task: `github.sh` had fallen back to an
    /// older `sh` rewrite with no labels and no ERR trap, and `jira.sh` was
    /// missing only its own `KAN-` guard. This fails the moment either
    /// copy changes beyond that one guard, in either direction.
    #[test]
    fn dot_spoolway_hooks_match_the_shipped_ones_beyond_the_kan_guard() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

        let github_shipped = hook_script("github.sh");
        let github_copy = std::fs::read_to_string(root.join(".spoolway/hooks/github.sh"))
            .expect("read .spoolway/hooks/github.sh");
        assert_eq!(
            github_copy, github_shipped,
            ".spoolway/hooks/github.sh has drifted from the shipped github.sh"
        );

        let jira_shipped = hook_script("jira.sh");
        let jira_copy = std::fs::read_to_string(root.join(".spoolway/hooks/jira.sh"))
            .expect("read .spoolway/hooks/jira.sh");
        let without_guard = jira_copy.replacen(JIRA_KAN_GUARD, "", 1);
        assert_eq!(
            without_guard, jira_shipped,
            ".spoolway/hooks/jira.sh has drifted from the shipped jira.sh beyond its own KAN- \
             guard"
        );
    }

    /// Nothing parses a prompt any more, which makes one thing worth asserting
    /// instead: that every shipped prompt is prose and carries no leftover
    /// marker from the format that used to partition these files. A stray
    /// marker would be read by nothing and quietly puzzle whoever opened it.
    #[test]
    fn no_shipped_prompt_carries_a_marker() {
        for prompt in PROMPTS {
            assert!(
                !prompt.body.contains("<!-- spoolway:"),
                "{} still carries a spoolway marker",
                prompt.name
            );
            assert!(!prompt.body.trim().is_empty(), "{} is empty", prompt.name);
        }
    }

    /// The rendered body of an epic or a ticket is the project's own, opened
    /// on its own tracker — not a spoolway one. Neither shipped template may
    /// say the brand's name in its own prose, so a hand-off between
    /// projects never reads as spoolway's ticket rather than theirs.
    ///
    /// The `${SPOOLWAY_*}` placeholders themselves are stripped first: those
    /// name the variables the hook's own environment already carries, the
    /// mechanism rather than the brand, and every one of them necessarily
    /// spells the word.
    #[test]
    fn no_shipped_tracking_template_names_the_brand() {
        for (name, body) in TRACKING_TEMPLATES {
            let mut prose = String::new();
            let mut rest = *body;
            while let Some(start) = rest.find("${") {
                prose.push_str(&rest[..start]);
                rest = match rest[start + 2..].find('}') {
                    Some(end) => &rest[start + 2 + end + 1..],
                    None => "",
                };
            }
            prose.push_str(rest);

            assert!(
                !prose.to_lowercase().contains("spoolway"),
                "{name}.md names spoolway outside its `${{SPOOLWAY_*}}` placeholders"
            );
        }
    }

    /// Every directory under `assets/prompts/` is named by `PROMPTS`, and every
    /// file under `assets/pipelines/` by `BUILTIN_PIPELINES`. A file named by
    /// neither is shipped in the binary's `include_str!` closure only if a table
    /// row references it, so an unreferenced one is dead weight: never written by
    /// `init`, never reachable by `sync --replace`, never validated. A stray
    /// `local.yml` pipeline with a repo-local `run:` line and six orphan prompt
    /// directories had accumulated this way before this test existed. Wire a new
    /// asset into its table, or delete it.
    #[test]
    fn every_shipped_asset_is_named_by_its_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

        let prompt_dir = root.join("assets/prompts");
        let mut orphans = Vec::new();
        for entry in std::fs::read_dir(&prompt_dir).expect("reading assets/prompts") {
            let path = entry.expect("prompt entry").path();
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !PROMPTS.iter().any(|p| p.name == name) {
                orphans.push(format!("assets/prompts/{name}"));
            }
        }

        let pipeline_dir = root.join("assets/pipelines");
        for entry in std::fs::read_dir(&pipeline_dir).expect("reading assets/pipelines") {
            let path = entry.expect("pipeline entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_string_lossy().to_string();
            if !crate::pipeline::BUILTIN_PIPELINES
                .iter()
                .any(|(name, _)| *name == stem)
            {
                orphans.push(format!("assets/pipelines/{stem}.yml"));
            }
        }

        assert!(
            orphans.is_empty(),
            "shipped asset(s) named by no table: {orphans:?}"
        );
    }

    /// No shipped pipeline file names a path that only resolves inside this
    /// repository's own build. `spoolway pipeline check` validates only a
    /// project's own loaded pipelines now — this is the release-time proof
    /// that the bundled samples stay clean, and it reads every `*.yml` on
    /// disk directly, so a file added under `assets/pipelines/` without a
    /// table row is still held to it. A `local.yml` that shipped `run:
    /// ./target/debug/spoolway stack` behind the table's back is what this
    /// closes.
    #[test]
    fn no_shipped_pipeline_file_names_a_repo_local_path() {
        let pipeline_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/pipelines");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&pipeline_dir).expect("reading assets/pipelines") {
            let path = entry.expect("pipeline entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("reading a pipeline file");
            for (number, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') {
                    continue;
                }
                if trimmed.contains("run:") && line.contains("./target/") {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.file_name().unwrap().to_string_lossy(),
                        number + 1,
                        trimmed
                    ));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "shipped pipeline `run:` names a repo-local path: {offenders:?}"
        );
    }

    /// Release-time proof that `assets/pipelines/*.yml` still parses and
    /// stays neutral, now that `spoolway pipeline check` derives every
    /// finding from a project's own loaded set and never opens these files
    /// itself. `Pipelines::shipped` parses, assembles and runs
    /// `Pipelines::validate` over both — the same structural rules `pipeline
    /// check` holds a project's own files to: every id unique, every
    /// transition naming a real step, every cycle bounded. Neutrality is
    /// checked directly per agent step written in the file: no assignment to
    /// a `pi` profile (spoolway's own retired special case) and no fixed
    /// `model:`/`effort:` — every choice is left an explicit blank for a
    /// project's own `init` to fill in. `blocked` is skipped: `assemble`
    /// materialises it whole from `[unattended]`'s own defaults rather than
    /// from anything either file writes, so it carries no neutrality promise
    /// of its own.
    ///
    /// The blank `model:`/`effort:` half of this overlaps with
    /// `crate::models::tests::shipped_pipelines_name_no_model`, which checks
    /// the same files at the text level — kept apart rather than merged,
    /// since that one lives beside `resolve`'s own model-pricing tests. This
    /// one adds what that one cannot: parse, assemble and validate against
    /// [`crate::pipeline::Pipelines::validate`], and the `pi`-assignment
    /// check.
    #[test]
    fn bundled_pipelines_parse_and_stay_agent_neutral() {
        let shipped = crate::pipeline::Pipelines::shipped(&crate::config::Config::default())
            .expect("assets/pipelines/*.yml must parse and validate structurally");

        for pipeline in shipped.pipelines.values() {
            for step in &pipeline.steps {
                if step.kind() != crate::pipeline::StepKind::Agent
                    || step.id == crate::pipeline::BLOCKED
                {
                    continue;
                }
                assert_ne!(
                    step.agent.as_deref(),
                    Some("pi"),
                    "`{}`/`{}` assigns the retired `pi` profile directly",
                    pipeline.name,
                    step.id
                );
                assert!(
                    step.model.as_deref().is_none_or(|m| m.trim().is_empty()),
                    "`{}`/`{}` fixes a model choice: {:?}",
                    pipeline.name,
                    step.id,
                    step.model
                );
                assert!(
                    step.effort.as_deref().is_none_or(|e| e.trim().is_empty()),
                    "`{}`/`{}` fixes an effort choice: {:?}",
                    pipeline.name,
                    step.id,
                    step.effort
                );
            }
        }
    }

    /// The crate description npm and crates.io display is a sentence from the
    /// README, not a flow (`implement -> review -> e2e -> PR -> merge`) that no
    /// shipped pipeline runs (finding 73).
    #[test]
    fn the_crate_description_is_the_readme_tagline() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cargo_toml = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();

        let description = cargo_toml
            .lines()
            .find_map(|line| line.trim().strip_prefix("description = "))
            .map(|value| value.trim().trim_matches('"'))
            .expect("Cargo.toml has a description");

        assert!(
            readme.contains(description),
            "crate description is not a phrase from README.md: {description:?}"
        );
        for stale in ["e2e", "-> merge", "-> PR"] {
            assert!(
                !description.contains(stale),
                "crate description still names `{stale}`"
            );
        }
    }
}

//! `spoolway doctor`: every check spoolway can run against a project, and
//! what each one found.
//!
//! Every finding here — not only the checks that read `config.toml` directly
//! — answers from `repo.checkout`'s own copy, not `repo.root`'s: `doctor`
//! loads its own [`Config`] from the checkout rather than reading
//! `repo.config` (loaded from `repo.root` back in `Repo::discover`), because
//! a task validating its own branch wants an answer about the file it
//! actually edited. `repo.root`'s copy still matters — it is what the real
//! dispatcher runs on — so its own parse failure, when there is one, is
//! reported as a finding of its own rather than silently dropped. See
//! [`doctor`].

use super::*;
use serde::Serialize;

/// One line of the report. Held rather than printed, so that a single run of
/// the checks can be rendered as the full listing or as only its exceptions.
///
/// `Serialize` is what `--json` reads back out: a `kind` tag plus each
/// variant's own fields, so a consumer like the `spoolway-config` skill's own
/// repair procedure can match on `kind` instead of pattern-matching the
/// plain-text prefixes [`Report::render`] prints.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Row {
    /// A check that passed, and what it found.
    Ok { label: String, note: Option<String> },
    /// Something worth saying that is not a check: never counted, never a
    /// failure. `verbose_only` marks the ones that describe the machine this
    /// ran on rather than the project, which the short report leaves out.
    Note { text: String, verbose_only: bool },
    /// A check that failed, and why.
    Fail { label: String, why: String },
}

/// Everything a `doctor` run found, in the order the checks are declared.
#[derive(Debug, Default)]
struct Report {
    rows: Vec<Row>,
}

impl Report {
    /// Record one check's outcome.
    fn check(&mut self, label: &str, outcome: Result<Option<String>>) {
        self.rows.push(match outcome {
            Ok(note) => Row::Ok {
                label: label.to_string(),
                note,
            },
            Err(err) => Row::Fail {
                label: label.to_string(),
                why: format!("{err:#}"),
            },
        });
    }

    /// Record a note, which both modes print.
    fn note(&mut self, text: impl Into<String>) {
        self.rows.push(Row::Note {
            text: text.into(),
            verbose_only: false,
        });
    }

    /// Record a note only the full listing prints.
    fn note_verbose(&mut self, text: impl Into<String>) {
        self.rows.push(Row::Note {
            text: text.into(),
            verbose_only: true,
        });
    }

    /// How many checks failed.
    fn problems(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, Row::Fail { .. }))
            .count()
    }

    /// How many checks ran at all. Notes are not checks and are not counted.
    fn checks(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| !matches!(row, Row::Note { .. }))
            .count()
    }

    /// The whole report as one block of text.
    ///
    /// The short form keeps the exceptions — the failures and the notes — and
    /// collapses every passing row into the closing count, because a screen of
    /// rows that all say `ok` is a screen nobody reads. `verbose` brings the
    /// listing back, in the same order and the same wording.
    fn render(&self, verbose: bool) -> String {
        let mut out = String::new();
        for row in &self.rows {
            match row {
                Row::Ok { label, note } if verbose => match note {
                    Some(note) => out.push_str(&format!("  ok    {label} — {note}\n")),
                    None => out.push_str(&format!("  ok    {label}\n")),
                },
                Row::Ok { .. } => {}
                Row::Note {
                    verbose_only: true, ..
                } if !verbose => {}
                Row::Note { text, .. } => out.push_str(&format!("  note  {text}\n")),
                Row::Fail { label, why } => out.push_str(&format!("  FAIL  {label}: {why}\n")),
            }
        }

        // The blank line separates the listing from the count, so there is
        // nothing to separate when nothing above it printed.
        if !out.is_empty() {
            out.push('\n');
        }
        let checks = self.checks();
        let problems = self.problems();
        if problems == 0 {
            out.push_str(&format!("{checks} checks passed.\n"));
        } else {
            out.push_str(&format!(
                "{} of {checks} checks passed.\n",
                checks - problems
            ));
        }
        out
    }

    /// The same report `render` prints, structured instead of prosed —
    /// `--json`'s own reading of it. `verbose` narrows the rows the same way
    /// it narrows the text form: a passing `Row::Ok` and a `verbose_only`
    /// note are left out of the short report entirely, rather than emitted
    /// and left for the reader to filter.
    fn render_json(&self, verbose: bool) -> serde_json::Value {
        let rows: Vec<&Row> = self
            .rows
            .iter()
            .filter(|row| match row {
                Row::Ok { .. } => verbose,
                Row::Note { verbose_only, .. } => verbose || !verbose_only,
                Row::Fail { .. } => true,
            })
            .collect();
        serde_json::json!({
            "checks": self.checks(),
            "problems": self.problems(),
            "rows": rows,
        })
    }
}

/// One outcome a `check_*` helper found, in the order it belongs in the
/// report. Not `Row` itself: a single helper often wants to emit a check
/// alongside one or more notes — the model and agent checks below all do —
/// and `Finding` is what lets `doctor()` fold a whole `Vec` of those into the
/// report with one loop, instead of every helper taking `&mut Report` and
/// every call site losing track of what order things run in.
#[derive(Debug)]
pub(crate) enum Finding {
    /// Becomes a `Row::Ok` or `Row::Fail`, via [`Report::check`].
    Check(String, Result<Option<String>>),
    /// Becomes a `Row::Note` that both report modes print.
    Note(String),
    /// Becomes a `Row::Note` that only the full listing prints.
    NoteVerbose(String),
}

impl Report {
    /// Record one [`Finding`], however it turns out to be shaped.
    fn record(&mut self, finding: Finding) {
        match finding {
            Finding::Check(label, outcome) => self.check(&label, outcome),
            Finding::Note(text) => self.note(text),
            Finding::NoteVerbose(text) => self.note_verbose(text),
        }
    }

    /// Record a whole `Vec` of findings, in the order a `check_*` helper
    /// produced them.
    fn record_all(&mut self, findings: Vec<Finding>) {
        for finding in findings {
            self.record(finding);
        }
    }
}

/// Print a finished report in the caller's chosen mode, and exit non-zero
/// when it carries a problem — the tail every `doctor` path shares.
fn finish(report: &Report, verbose: bool, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report.render_json(verbose))?
        );
    } else {
        print!("{}", report.render(verbose));
    }
    let problems = report.problems();
    if problems == 0 {
        Ok(())
    } else {
        bail!("{problems} problem(s) — /spoolway-config fixes these")
    }
}

/// One of [`cheap_findings`]'s own rows, grouped for `dispatch`'s warnings
/// screen rather than tagged with [`Row`] — see
/// `commands::dispatch::warnings_gate_with`. `Setting` and `Problem` mirror
/// [`Row::Note`] and [`Row::Fail`]; `File` is the one note [`cheap_findings`]
/// tells apart from any other on the way out — [`doctor_sync`]'s own notes
/// about a file on disk being behind this spoolway or never written — because
/// the screen's mockup gives that one case its own heading rather than
/// folding it into the settings catch-all.
#[derive(Debug, PartialEq)]
pub(crate) enum Warning {
    Setting(String),
    File(String),
    Problem(String),
}

/// Doctor's note- and failure-level findings, from the checks that never
/// reach the network or open a pane — the same rows [`doctor`] itself would
/// print, minus the two that shell out to `git ls-remote`/`gh auth status`
/// and the live pane check, and minus every passing [`Row::Ok`], which the
/// warnings screen has no room for.
///
/// Called by `commands::dispatch::warnings_gate_with` so a run says before it
/// starts what `spoolway doctor` would have said anyway — see
/// `doctor()`'s own doc comment for the order these checks run in, which
/// this mirrors except for the calls named below.
///
/// Four more of `doctor()`'s own rows are left out on purpose, not merely
/// skipped for being expensive:
///
/// - The override-layer note has its own screen one step earlier, in
///   `dispatch`'s own overrides gate, and repeating it here would ask the
///   same question twice.
/// - `check_backend_checkout`'s "backend and checkout" row is never called
///   here at all: `dispatch` already refuses to start over it, earlier in
///   the very same call, before any gate runs — by the time this screen can
///   draw, it has already passed, and re-running it would need a second
///   resolved [`crate::mux::Mux`] this function has no cheap way to hand it
///   again for no new answer.
/// - `Lock::holder`'s "a dispatcher is running" note is never called either:
///   every point this screen can run from — always before `Lock::acquire` —
///   is a point `dispatch` has already confirmed no other dispatcher holds
///   the lock, or it would have exited instead of reaching a gate at all.
///   Calling it here would at best repeat a fact already acted on, and if
///   ever read a moment too late, once the lock is this very process's own,
///   would misreport the caller's own lock back to it as somebody else's.
/// - [`unrouted_model_notes`] is never called: a `[models]` row no step routes
///   to is dead config that costs nothing at run time, so it is worth naming
///   in the `spoolway doctor` report but not worth stopping a run over.
pub(crate) fn cheap_findings(repo: &Repo, pipelines: &Pipelines, config: &Config) -> Vec<Warning> {
    let mux = crate::mux::backend(repo);
    let tasks = repo.tasks().unwrap_or_default();
    let graph = Graph::build(&tasks, &repo.archive_dir());

    let mut report = Report::default();
    report.record_all(config_checks(repo, None, config));
    report.record_all(issue_tracking_checks(repo, &config.issue_tracking));
    report.record_all(retired_key_notes(&repo.checkout, &tasks));
    warmth_notes(repo, pipelines, config, &mut report);
    report.record_all(pipeline_graph_checks(pipelines, config, &graph));
    report.record_all(cheap_branch_checks(repo, &tasks));
    report.record(mux_finding(&mux));
    report.record(Finding::Check(
        "git identity".into(),
        crate::commands::dispatch::check_git_identity(repo, pipelines, config),
    ));
    report.record(Finding::Check(
        "no stale .git/index.lock".into(),
        crate::commands::dispatch::check_index_lock(repo),
    ));
    report.record_all(agent_checks(repo, pipelines, config));
    report.record_all(model_health_checks(pipelines, config));
    report.record_all(agent_kind_checks(config));
    // `doctor_sync`'s own rows are the ones tagged `Warning::File` below —
    // marked by index range rather than by a new `Row` variant, since `Row`
    // is `doctor`'s own report shape and this screen's "files" heading is
    // not doctor's to know about.
    let sync_start = report.rows.len();
    doctor_sync(repo, &mut report);
    let sync_end = report.rows.len();
    report.record_all(prompt_checks(repo, pipelines));
    report.record_all(jobs_checks(repo, pipelines));

    warnings_from_rows(report.rows, sync_start..sync_end)
}

/// [`cheap_findings`]'s own last step, pulled out on its own so a test can
/// hand it a hand-built set of rows rather than a real project: every
/// [`Row::Fail`] becomes a [`Warning::Problem`], every [`Row::Note`] a
/// [`Warning::Setting`] unless its index falls in `sync_rows` (then
/// [`Warning::File`]) or it is `verbose_only` (then dropped — the short
/// report's own rule: a note describing the machine this ran on rather than
/// the project says nothing a person needs before starting a run), and every
/// [`Row::Ok`] dropped, since a screen for warnings has no room for a check
/// that passed.
fn warnings_from_rows(rows: Vec<Row>, sync_rows: std::ops::Range<usize>) -> Vec<Warning> {
    rows.into_iter()
        .enumerate()
        .filter_map(|(i, row)| match row {
            Row::Ok { .. } => None,
            Row::Fail { label, why } => Some(Warning::Problem(format!("{label}: {why}"))),
            Row::Note {
                verbose_only: true, ..
            } => None,
            Row::Note { text, .. } if sync_rows.contains(&i) => Some(Warning::File(text)),
            Row::Note { text, .. } => Some(Warning::Setting(text)),
        })
        .collect()
}

/// [`branch_and_forge_checks`]'s own first check, alone: whether the branch
/// (or branches) work is based on resolve at all. Its sibling checks in that
/// function are the two [`cheap_findings`] leaves out — one `git ls-remote`
/// per base, and `gh auth status` — both reaching the network, which nothing
/// on `dispatch`'s pre-run path may do.
fn cheap_branch_checks(repo: &Repo, tasks: &[Task]) -> Vec<Finding> {
    let mut bases: Vec<String> = tasks.iter().filter_map(|t| t.front.base.clone()).collect();
    bases.sort();
    bases.dedup();
    if bases.is_empty() {
        vec![Finding::Check(
            "on a usable branch".into(),
            repo.branch().map(Some),
        )]
    } else {
        vec![Finding::Check(
            "work is based on".into(),
            Ok(Some(bases.join(", "))),
        )]
    }
}

/// How `doctor` should treat the live pane check — one argument standing in
/// for `--no-live` and `--live`, which `clap` already keeps mutually
/// exclusive (see `DoctorArgs`), rather than two separate bools that would
/// push [`doctor`] over `clippy::too_many_arguments` for a distinction
/// [`live_check`] alone needs to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveCheckMode {
    /// Run the check only when [`Mux::in_own_pane`] says this process
    /// actually is one.
    Default,
    /// `--no-live`: never run it, whatever `in_own_pane` answers.
    Skip,
    /// `--live`: run it even when `in_own_pane` says this process is
    /// outside the backend's own pane.
    Forced,
}

/// Check that everything the configured pipeline needs is actually present,
/// before a dispatch pass discovers it the hard way.
///
/// `config_error` is why `repo.root`'s `config.toml` could not be read, when
/// it could not — the lenient discovery that built `repo` already folded that
/// into `repo.config` falling back to defaults. It no longer gates whether
/// the rest of this can run: every finding below is against a fresh load of
/// `repo.checkout`'s own copy instead, so a checkout whose file parses is
/// checked in full even when the project's does not. That load is what does
/// the gating now — see the early return below — and `config_error`, when
/// `Some`, becomes a finding of its own instead.
///
/// `pipelines` is handed in as a `Result` for the same reason: a pipeline
/// file that will not parse is what this command exists to name, so its
/// load failure is a finding, not a gate — see the second early return.
pub fn doctor(
    repo: &Repo,
    pipelines: Result<Pipelines>,
    config_error: Option<anyhow::Error>,
    home_error: Option<anyhow::Error>,
    verbose: bool,
    json: bool,
    live: LiveCheckMode,
) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }

    // A stamped home that could not be resolved at all takes a distinct
    // path, before anything below gets the chance to touch it: `repo.home`
    // here is nothing but the basename-keyed placeholder
    // `crate::mux::project_home_lenient` falls back to when it cannot give
    // a real answer, and every check past this point either reads it
    // directly (`repo.tasks()`, `repo.archive_dir()`, `repo.lock_file()`
    // all create their directory on demand) or reads it indirectly through
    // `Config::load`/`Pipelines::load`, which both read the overrides layer
    // out of the home that just failed to settle. Running any of them
    // would recreate the silent wrong-directory write id-keying this
    // project's home exists to prevent, or misreport this one failure as a
    // config or pipeline problem it is not.
    if let Some(err) = home_error {
        return doctor_home_unavailable(repo, config_error, err, verbose, json);
    }

    let config = match Config::load(&repo.checkout) {
        Ok(config) => config,
        Err(err) => {
            return doctor_unconfigured(repo, pipelines, err, verbose, json);
        }
    };

    // Built once and shared by every check below that asks the multiplexer
    // anything, so `lanes can be started`, the live pane and `backend and
    // checkout` all read the same backend rather than each constructing —
    // and possibly disagreeing about — their own.
    let mux = crate::mux::backend(repo);

    let mut report = Report::default();

    // Read ahead of the pipeline match below so `worktree_root_note` — one
    // of `retired_key_notes`'s own findings — can name a queued task with a
    // worktree in the old folder in both branches, not only the one with a
    // loaded pipeline set.
    let tasks = repo.tasks().unwrap_or_default();

    // A pipeline file that will not parse must not stop the command you run
    // to find out which one it is. Report the load failure as a failed row
    // and run the checks that read no pipeline — config, issue tracking,
    // retired keys, the multiplexer, the update check — since the graph,
    // agent and prompt-validity checks below all need a loaded pipeline set.
    let pipelines = match pipelines {
        Ok(pipelines) => pipelines,
        Err(err) => {
            report.check(
                "pipelines load",
                pipelines_load_outcome(&repo.checkout, err),
            );
            report.record_all(config_checks(repo, config_error, &config));
            report.record_all(issue_tracking_checks(repo, &config.issue_tracking));
            report.record_all(retired_key_notes(&repo.checkout, &tasks));
            report.record_all(override_layer_note(repo));
            report.record_all(orphaned_home_note());
            report.record(mux_finding(&mux));
            doctor_sync(repo, &mut report);
            report.record(match crate::lock::Lock::holder(&repo.lock_file())? {
                Some(pid) => Finding::Note(format!("a dispatcher is running (pid {pid})")),
                None => Finding::NoteVerbose("no dispatcher running".into()),
            });
            return finish(&report, verbose, json);
        }
    };
    let pipelines = &pipelines;

    // A task file can be hand-edited into a graph nothing can get through, and
    // the only symptom is tasks that quietly never start.
    let graph = Graph::build(&tasks, &repo.archive_dir());

    report.record_all(config_checks(repo, config_error, &config));
    report.record_all(issue_tracking_checks(repo, &config.issue_tracking));
    report.record_all(retired_key_notes(&repo.checkout, &tasks));
    report.record_all(override_layer_note(repo));
    report.record_all(orphaned_home_note());
    warmth_notes(repo, pipelines, &config, &mut report);
    report.record_all(pipeline_graph_checks(pipelines, &config, &graph));
    report.record_all(branch_and_forge_checks(
        repo,
        &tasks,
        pipelines,
        &config.issue_tracking,
    ));
    report.record(mux_finding(&mux));
    match &mux {
        Ok(m) => report.record(live_check(m.as_ref(), live)),
        Err(_) => report.record(Finding::NoteVerbose(
            "no live pane check: the backend could not be resolved".into(),
        )),
    }
    report.record(Finding::Check(
        "git identity".into(),
        crate::commands::dispatch::check_git_identity(repo, pipelines, &config),
    ));
    report.record(Finding::Check(
        "no stale .git/index.lock".into(),
        crate::commands::dispatch::check_index_lock(repo),
    ));
    report.record(Finding::Check(
        "backend and checkout".into(),
        match &mux {
            Ok(m) => crate::commands::dispatch::check_backend_checkout(repo, m.as_ref()),
            Err(err) => Err(anyhow::anyhow!("{err:#}")),
        },
    ));
    report.record_all(agent_checks(repo, pipelines, &config));
    report.record_all(model_health_checks(pipelines, &config));
    report.record_all(unrouted_model_notes(pipelines, &config));
    report.record_all(agent_kind_checks(&config));
    doctor_sync(repo, &mut report);
    report.record_all(prompt_checks(repo, pipelines));
    report.record_all(jobs_checks(repo, pipelines));

    // A dispatcher that is running is a fact about this moment that changes
    // what a person should do next. Its absence is the ordinary case, and says
    // nothing, so it keeps to the full listing.
    report.record(match crate::lock::Lock::holder(&repo.lock_file())? {
        Some(pid) => Finding::Note(format!("a dispatcher is running (pid {pid})")),
        None => Finding::NoteVerbose("no dispatcher running".into()),
    });

    finish(&report, verbose, json)
}

/// `doctor` when `Repo::discover_lenient`'s own resolution of this
/// project's stamped home failed outright — a permissions problem, a
/// corrupt repository.
///
/// Nothing below this reads `repo.home`, directly or otherwise:
/// `repo.tasks()`, `repo.archive_dir()` and `repo.lock_file()` all create
/// their directory the moment they are asked, and `Config::load`/
/// `Pipelines::load` both read the overrides layer out of the home that just
/// failed to settle, which could report this project's own perfectly good
/// tracked files as broken.
/// [`home_unavailable_findings`] reads the tracked files alone, through
/// `Config::load_tracked`/`Pipelines::load_tracked`, which need no home at
/// all.
fn doctor_home_unavailable(
    repo: &Repo,
    config_error: Option<anyhow::Error>,
    home_error: anyhow::Error,
    verbose: bool,
    json: bool,
) -> Result<()> {
    let mut report = Report::default();
    report.record_all(home_unavailable_findings(repo, config_error, &home_error));
    report.note(format!(
        "this project's stamped home has not resolved: {home_error:#}"
    ));
    finish(&report, verbose, json)
}

/// The findings [`doctor_home_unavailable`] reports, pulled out on its own
/// so a test can read them without capturing stdout.
fn home_unavailable_findings(
    repo: &Repo,
    config_error: Option<anyhow::Error>,
    home_error: &anyhow::Error,
) -> Vec<Finding> {
    let mut findings = vec![registration_check(repo, Some(home_error))];
    if let Some(err) = config_error {
        findings.push(Finding::Check(
            "the project's own config parses".into(),
            Err(err.context(Config::path_in(&repo.root).display().to_string())),
        ));
    }
    match Config::load_tracked(&repo.checkout) {
        Ok(config) => {
            findings.push(Finding::Check(
                "config parses".into(),
                Ok(Some(format!(
                    "{} — this checkout's own copy, no overrides layer (its home did not \
                     resolve)",
                    Config::path_in(&repo.checkout).display()
                ))),
            ));
            findings.extend(issue_tracking_checks(repo, &config.issue_tracking));
            findings.push(Finding::Check(
                "pipelines are valid".into(),
                Pipelines::load_tracked(&repo.checkout, &config)
                    .map_err(|e| anyhow::anyhow!("{e:#}"))
                    .and_then(|pipelines| {
                        pipelines
                            .validate()
                            .map(|()| Some(format!("{:?}", pipelines.names())))
                    }),
            ));
        }
        Err(err) => findings.push(Finding::Check("config parses".into(), Err(err))),
    }
    // No `tasks` here: `repo.tasks()` reads under `repo.home`, the very
    // thing that failed to resolve — see the early return above this
    // function's own caller. `worktree_root_note` still names the path with
    // no task list to check it against, the same as it would for a project
    // with no queue at all.
    findings.extend(retired_key_notes(&repo.checkout, &[]));
    findings
}

/// `doctor` on a checkout whose own `config.toml` does not parse — `err` is
/// [`Config::load`]'s failure against `repo.checkout`, not `repo.root`.
///
/// Reporting the parse error is most of the value here: it is one line, it
/// names the file and the line in it, and until it is fixed nothing else in
/// spoolway runs at all. The rest is what can still be said without knowing a
/// single setting — the pipeline is a separate file, and a branch is git's
/// answer, not the config's. Everything else is skipped rather than guessed at:
/// running the agent and path checks against built-in defaults would report
/// on settings this project never wrote.
fn doctor_unconfigured(
    repo: &Repo,
    pipelines: Result<Pipelines>,
    err: anyhow::Error,
    verbose: bool,
    json: bool,
) -> Result<()> {
    let mut report = Report::default();

    // `home_error` is never `Some` by the time this runs — see
    // `doctor_home_unavailable`.
    report.record(registration_check(repo, None));
    report.check("config parses", Err(err));
    report.check(
        "pipelines are valid",
        pipelines
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e:#}"))
            .and_then(|pipelines| {
                pipelines
                    .validate()
                    .map(|()| Some(format!("{:?}", pipelines.names())))
            }),
    );
    report.check("on a usable branch", repo.branch().map(Some));

    match crate::lock::Lock::holder(&repo.lock_file())? {
        Some(pid) => report.note(format!("a dispatcher is running (pid {pid})")),
        None => report.note_verbose("no dispatcher running"),
    }
    report.note(format!(
        "{} does not parse ({})",
        relative(&repo.checkout, &Config::path_in(&repo.checkout)),
        config_owner_label(repo),
    ));

    finish(&report, verbose, json)
}

/// Whether this checkout's stamp and its home's own record of it agree.
/// Every other command refuses to run when they do not — see
/// `Repo::discover` and `crate::repo::bind` — and `doctor` is the one that
/// comes through `discover_lenient` instead, so it is the one place the
/// fact is reported rather than fatal.
///
/// `home_error` is `Repo::discover_lenient`'s own `bind_lenient` report — the
/// binding [`crate::repo::bind`] found or could not settle, already naming
/// both files and the command that resolves it. `doctor` has nothing to add
/// to that message, only to carry it here as its own finding rather than
/// the fatal refusal every other command gives it. A settled binding is
/// reported by both facts it settled — the id and the root
/// [`crate::repo::binding_at`] reads back off `project.toml` — not merely
/// that a directory called `repo.home` happens to exist, which is all the
/// old pointer-file report ever said.
fn registration_check(repo: &Repo, home_error: Option<&anyhow::Error>) -> Finding {
    Finding::Check(
        "bound to its home".into(),
        match home_error {
            Some(err) => Err(anyhow::anyhow!("{err:#}")),
            // `workspace_clone_lenient`, not the strict `workspace_clone_
            // checked`: by the time `repo` exists at all, `Repo::root` has
            // already settled whether this checkout might be the one some
            // broken workspace file elsewhere would have named — that is
            // what refuses, long before `doctor` gets a `Repo` to report
            // on. A broken file found here is never this checkout's own
            // problem, so answering "repo mode" for it is correct, not a
            // fallback; `bind_lenient`, already run above to get here, is
            // what already noted it on `discover_lenient`'s way past.
            None => match crate::repo::workspace_clone_lenient(&repo.checkout) {
                Err(err) => Err(err),
                // The mode is named here, not as a row of its own, so it reads
                // exactly where a person already looks to see what this
                // checkout is bound to — see the `home-mode-discovery` task's
                // "doctor says which mode the project is in", which names both
                // modes, not only home mode.
                Ok((Some(_), ..)) => Ok(Some(format!(
                    "{} — home mode, setup read from {}",
                    repo.home.display(),
                    repo.setup_dir().display()
                ))),
                Ok((None, ..)) => Ok(Some(match crate::repo::binding_at(&repo.home) {
                    Some((id, root)) => {
                        format!(
                            "{} — repo mode, id {id}, root {}",
                            repo.home.display(),
                            root.display()
                        )
                    }
                    None => format!("{} — repo mode", repo.home.display()),
                })),
            },
        },
    )
}

/// Whether `config.toml` is this checkout's own to claim, or the workspace's
/// shared one every clone reads instead — home mode's whole point: `config/`
/// sits beside the workspace, not inside any one clone, so calling it "this
/// checkout's own copy" there names an owner the file does not have.
fn config_owner_label(repo: &Repo) -> &'static str {
    if crate::repo::workspace_clone(&repo.checkout).is_some() {
        "the workspace's shared config"
    } else {
        "this checkout's own copy"
    }
}

/// The checks on a config's own values that read nothing but the config.
///
/// Shared with `spoolway config set`, which runs them on the config it has
/// just produced, so a value typed there and a value `doctor` reads are judged
/// by the same code.
pub(crate) fn config_value_checks(config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    // An unattended run resumes a block by staffing `blocked` with a lane —
    // every pipeline has one now, materialised by `Pipelines::assemble` from
    // `[unattended]`'s own keys, or overridden by the pipeline itself — and a
    // lane needs a model to run on. Checked here rather than left to
    // `pipeline check`'s soft "names no model" problem, because this is the
    // one missing model that stops a *run* from starting at all rather than
    // just one step of it.
    findings.push(Finding::Check(
        "an unattended run staffs `blocked`".into(),
        match config.unattended.enabled && config.unattended.blocked_model.trim().is_empty() {
            true => Err(anyhow::anyhow!("unattended.blocked_model is blank")),
            false => Ok(None),
        },
    ));
    findings
}

/// What `spoolway doctor` would fail on after `key` was set, one warning line
/// per failing check, for `spoolway config set` to print.
///
/// Runs the checks that need no network and no multiplexer. They are not all
/// pure reads: the issue-tracking ones look for `acli` and `jq` on `PATH` and
/// run `<tool> --version` for each tool a hook declares, so setting an
/// `issue_tracking.*` key can start those processes. Only the sections `key`
/// can affect are run, so a script setting unrelated keys is not told about a
/// gap it has not reached.
pub(crate) fn failures_after_set(repo: &Repo, config: &Config, key: &str) -> Vec<String> {
    let mut findings = Vec::new();
    if key.starts_with("unattended.") {
        findings.extend(config_value_checks(config));
    }
    if key.starts_with("issue_tracking.") {
        findings.extend(issue_tracking_checks(repo, &config.issue_tracking));
    }
    findings
        .into_iter()
        .filter_map(|finding| match finding {
            Finding::Check(label, Err(err)) => Some(format!(
                "`spoolway doctor` fails its check \"{label}\": {err:#}"
            )),
            _ => None,
        })
        .collect()
}

/// The checks that only need the checkout's own loaded `config` and whether
/// the project's own copy (`repo.root`'s) parsed — see the module doc for why
/// those are two different questions. Order matches `doctor`'s own: the
/// checkout's parse, the project's parse (only when it failed), then the one
/// setting an unattended run cannot start without.
fn config_checks(
    repo: &Repo,
    config_error: Option<anyhow::Error>,
    config: &Config,
) -> Vec<Finding> {
    // `home_error` is never `Some` by the time `config_checks` runs: `doctor`
    // takes a distinct path the moment it has one, before `Config::load` (or
    // any other home-backed call) — see `doctor_home_unavailable`.
    let mut findings = vec![registration_check(repo, None)];
    // Moving the last checkout out of a workspace keeps its folder, so this
    // is where a person learns what is left to remove by hand.
    for workspace in crate::repo::workspaces_listing_no_checkout() {
        findings.push(Finding::Note(format!(
            "workspace {} lists no checkout — remove it by hand once nothing in it is needed",
            crate::repo::shorten_home(&workspace),
        )));
    }
    findings.push(Finding::Check(
        "config parses".into(),
        Ok(Some(format!(
            "{} — {}",
            Config::path_in(&repo.checkout).display(),
            config_owner_label(repo),
        ))),
    ));
    // The checkout's file parsing says nothing about the project's own copy —
    // the one the real dispatcher runs on — when the two differ. In the main
    // checkout they are the same file, so `config_error` and the check above
    // agree and this never fires.
    if let Some(err) = config_error {
        findings.push(Finding::Check(
            "the project's own config parses".into(),
            Err(err.context(Config::path_in(&repo.root).display().to_string())),
        ));
    }
    findings.extend(unknown_key_notes(&repo.checkout));
    findings.extend(config_value_checks(config));
    if let Some(note) = current_price_table_age_note(config.housekeeping.price_max_age_days) {
        findings.push(Finding::Note(note));
    }
    findings
}

/// The old table is advisory state: it is a note rather than a check, and a
/// zero limit suppresses even the table read so "off" stays wholly silent.
fn current_price_table_age_note(max_age_days: u64) -> Option<String> {
    if max_age_days == 0 {
        return None;
    }
    price_table_age_note(max_age_days, crate::models::price_table_age().days)
}

fn price_table_age_note(max_age_days: u64, age_days: u64) -> Option<String> {
    (max_age_days > 0 && age_days > max_age_days).then(|| {
        format!(
            "the price table is {age_days} days old, past the {max_age_days} in \
             `housekeeping.price_max_age_days`"
        )
    })
}

/// Everything `[issue_tracking]` can get wrong on its own, independent of the
/// pipeline or the branch: a hook named with nothing to hand it, a hook that
/// cannot resolve to a real file, a hook file that was never written, a hook
/// script too old to have a `fetch` branch, and — for the one hook that
/// shells out — the binaries it needs beside it.
///
/// Also what `commands::dispatch::dispatch` runs once as it starts, through
/// `cheap_findings`, which turns this function's own `FAIL` rows into a
/// warning read before the run rather than doctor's own refusal, and what
/// `spoolway config set` runs through [`failures_after_set`] to warn about
/// the value just typed.
pub(crate) fn issue_tracking_checks(
    repo: &Repo,
    tracking: &crate::config::IssueTrackingConfig,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let config_path = Config::path_in(&repo.checkout).display().to_string();

    // The one half-set shape a person can actually reach: a hook named with
    // nothing to hand it. `hook` blank means no issue tracking, whatever
    // `project_key` holds — that combination is fine and reported nowhere —
    // but a hook that runs with a blank `project_key` opens no ticket ever,
    // silently, on every one of a task's five events.
    findings.push(Finding::Check(
        "`[issue_tracking]` is fully configured or fully off".into(),
        match !tracking.hook.trim().is_empty() && tracking.project_key.trim().is_empty() {
            true => Err(anyhow::anyhow!(
                "{config_path}: [issue_tracking] names `{}` but project_key is empty — no \
                 ticket will ever open. Set it, or clear hook to switch issue tracking off.",
                tracking.hook.trim()
            )),
            false => Ok(None),
        },
    ));
    // The hook's own doc comment promises a bare filename can never resolve
    // outside `.spoolway/hooks/` — `crate::tracking::is_bare_filename` is
    // what actually enforces that at the point the value is read, and a
    // config naming something else silently runs no hook at all. Worth
    // saying here rather than only failing quietly on every one of a task's
    // five events.
    findings.push(Finding::Check(
        "`issue_tracking.hook` is a bare filename".into(),
        match crate::tracking::not_bare_filename_problem(&tracking.hook) {
            Some(problem) => Err(anyhow::anyhow!("{config_path}: [issue_tracking] {problem}")),
            None => Ok(None),
        },
    ));
    // A bare, well-formed name still fails silently on every one of a task's
    // five events if the file behind it was never written — `init` scaffolds
    // both shipped hooks, but nothing stops a hand-edited config naming one
    // that was renamed or never copied in.
    if let Some(path) = crate::tracking::hook_path_in(&repo.checkout, &tracking.hook) {
        findings.push(Finding::Check(
            "`issue_tracking.hook` script exists".into(),
            match path.exists() {
                true => Ok(Some(relative(&repo.checkout, &path))),
                false => Err(anyhow::anyhow!(
                    "{} does not exist — `spoolway init` writes it, or clear hook to switch \
                     issue tracking off",
                    path.display()
                )),
            },
        ));
    }
    // `spoolway sync` never rewrites a hook a project already has, so
    // every install that named a tracker before this event existed has a
    // script with no `fetch` branch — and running one anyway would read as
    // "fetched an issue with nothing on it" rather than "never
    // implemented", see `tracking::has_fetch_branch`. Worth saying here
    // rather than only failing `spoolway issue show` with an exit code and
    // no explanation.
    findings.push(Finding::Check(
        "the configured hook script has a `fetch` branch".into(),
        match crate::tracking::missing_fetch_branch(&repo.checkout, &tracking.hook) {
            Some(name) => Err(anyhow::anyhow!(
                "{config_path}: [issue_tracking] names `{name}`, whose script has no `fetch` \
                 branch — `spoolway issue show` needs one to read an issue back out of the \
                 tracker. Add a case for `SPOOLWAY_EVENT=fetch` the way the shipped samples \
                 do, or regenerate one with `spoolway init` into a fresh directory to copy \
                 the branch across by hand."
            )),
            None => Ok(None),
        },
    ));
    // A hook declares the tools it needs with its own `# spoolway-requires:`
    // lines — see `crate::tracking::required_tools` — and this is where each
    // one is actually checked against the machine doctor runs on. Grouped
    // right after the `fetch`-branch check: both read the hook's own text for
    // a promise it may not be keeping, this one keeps the machine's tools
    // from making the same silent gap.
    findings.extend(required_tool_checks(&repo.checkout, &tracking.hook));
    // `issue_tracking.key_in_names` prefixes every generated name with a
    // `slug=` the hook answers — but `spoolway sync` never rewrites a hook
    // a project already has, so a project that turned the flag on without
    // adding the line gets no prefix at all, silently, on every `queue add`.
    findings.push(Finding::Check(
        "the configured hook writes a `slug=` line".into(),
        match crate::tracking::missing_slug_line(
            &repo.checkout,
            &tracking.hook,
            tracking.key_in_names,
        ) {
            Some(name) => Err(anyhow::anyhow!(
                "{config_path}: [issue_tracking] key_in_names is on, but {name} never writes \
                 `slug=` — groups, branches and worktrees will be named without a prefix. Add a \
                 slug= line the way the shipped samples do, or clear key_in_names."
            )),
            None => Ok(None),
        },
    ));
    // The two shipped hooks each shell out to a binary this project does not
    // otherwise need — `gh` is already checked below for `spoolway stack`,
    // so only the one hook that would go unnoticed is checked here: `jira.sh`
    // needs both `acli` and, since `workitem create --json` is the only way
    // it gets a ticket key back, `jq` beside it.
    if tracking.hook.trim() == crate::cli::Tracker::Jira.hook_name() {
        for binary in ["acli", "jq"] {
            findings.push(Finding::Check(
                format!("`{binary}` is on PATH"),
                match which(binary) {
                    Some(path) => Ok(Some(path)),
                    None => Err(anyhow::anyhow!(
                        "`{binary}` is not on PATH — issue_tracking.hook names `jira.sh`, and \
                         it cannot open or close a ticket without it"
                    )),
                },
            ));
        }
    }
    findings
}

/// The first run of `<digit>(.<digit>)*` in `text`, read as a version — every
/// shape a shipped hook's own tool answers `--version` with: `gh`'s carries a
/// build date in parens after it, `acli`'s a `-stable` suffix, `jq`'s a `jq-`
/// prefix and no space at all. `None` when nothing in `text` reads as one,
/// which is the unreadable-answer case [`tool_version_finding`] turns into a
/// note rather than a failure.
///
/// `pub(crate)` so `commands::queue`'s own submit gate — see
/// `queue::unmet_requirements` — reads a hook's declared floor and a tool's
/// live answer the same way this check does, rather than a second
/// hand-rolled parse drifting from this one.
pub(crate) fn parse_version(text: &str) -> Option<Vec<u64>> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end]
        .split('.')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().ok())
        .collect()
}

/// [`parse_version`]'s own output, back as the dotted string a person wrote —
/// what a passing row and a failing one both name the version found as.
///
/// `pub(crate)`, alongside [`parse_version`], for the same reuse.
pub(crate) fn format_version(version: &[u64]) -> String {
    version
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// Whether `found` is below `floor`, component by component with a missing
/// trailing one read as `0` — the same padding [`crate::release::is_newer`]
/// gives a version comparison, and for the same reason: comparing two
/// `Vec<u64>` lexicographically instead would read `found`'s missing `.0` as
/// smaller than `floor`'s explicit one, failing a `jq-1.6` against a floor of
/// `1.6.0` that a person reading either string would call equal.
///
/// `pub(crate)`, alongside [`parse_version`], for the same reuse.
pub(crate) fn below_floor(found: &[u64], floor: &[u64]) -> bool {
    for index in 0..found.len().max(floor.len()) {
        let found = found.get(index).copied().unwrap_or(0);
        let floor = floor.get(index).copied().unwrap_or(0);
        if found != floor {
            return found < floor;
        }
    }
    false
}

/// One `# spoolway-requires:` declaration, turned into the row it produces —
/// pure, so the message shapes are tested without a real PATH or a real
/// binary to run. `answered` is what `<tool> --version` already printed,
/// combining stdout and stderr the way a tool's own version banner may land
/// on either; `None` when the tool could not even be asked — not on PATH, or
/// its `--version` failed to run at all — and turns into no finding here at
/// all: [`required_tool_checks`]'s own caller, `issue_tracking_checks`,
/// already has a not-on-PATH row for `gh`, `acli` and `jq` (see
/// `gh_status` and the jira PATH checks), and doubling that gap under a
/// second name is exactly what the acceptance criteria rule out.
fn tool_version_finding(
    required: &crate::tracking::RequiredTool,
    hook_display: &str,
    answered: Option<&str>,
) -> Option<Finding> {
    let answered = answered?;
    let label = format!("`{}` is at least {}", required.tool, required.floor);
    // The non-goal ruling out a general constraint grammar covers the floor
    // too: a `spoolway-requires` line this project itself never wrote could
    // still name something that does not parse as a plain version.
    let Some(floor) = parse_version(&required.floor) else {
        return Some(Finding::Note(format!(
            "{hook_display} declares `spoolway-requires: {} >= {}`, which does not read as a \
             version",
            required.tool, required.floor
        )));
    };
    let Some(found) = parse_version(answered) else {
        return Some(Finding::Note(format!(
            "{hook_display} requires `{}` >= {}, but `{} --version` answered \"{}\", which does \
             not read as a version",
            required.tool,
            required.floor,
            required.tool,
            first_line(answered)
        )));
    };
    Some(Finding::Check(
        label,
        if below_floor(&found, &floor) {
            Err(anyhow::anyhow!(
                "`{}` is {}, and {hook_display} requires {} >= {}. Upgrade {}, or clear \
                 issue_tracking.hook.",
                required.tool,
                format_version(&found),
                required.tool,
                required.floor,
                required.tool,
            ))
        } else {
            Ok(Some(format_version(&found)))
        },
    ))
}

/// Every `# spoolway-requires:` line the configured hook's own text declares,
/// checked against what each named tool actually answers to `--version` on
/// this machine — the one check here that runs a tool rather than only
/// stat-ing or reading it, the same way [`gh_status`] does for `spoolway
/// stack`'s own `gh`.
///
/// `None` from [`crate::tracking::hook_path_in`] — a blank or malformed
/// `hook` — or a script that cannot be read produce no findings at all: both
/// are a different check's failure already, reported elsewhere in
/// `issue_tracking_checks`.
fn required_tool_checks(checkout: &Path, hook_name: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let Some(path) = crate::tracking::hook_path_in(checkout, hook_name) else {
        return findings;
    };
    let Ok(script) = std::fs::read_to_string(&path) else {
        return findings;
    };
    let hook_display = relative(checkout, &path);
    for parsed in crate::tracking::required_tools(&script) {
        match parsed {
            Ok(required) => {
                // Not on PATH is a different check's failure — `gh_status`
                // when the hook is `github.sh`, the jira PATH checks when it
                // is `jira.sh` — so this reports nothing rather than a
                // second, differently-worded row for the same gap.
                if which(&required.tool).is_none() {
                    continue;
                }
                let answered = std::process::Command::new(&required.tool)
                    .arg("--version")
                    .output()
                    .ok()
                    .map(|out| {
                        format!(
                            "{}{}",
                            String::from_utf8_lossy(&out.stdout),
                            String::from_utf8_lossy(&out.stderr)
                        )
                    });
                findings.extend(tool_version_finding(
                    &required,
                    &hook_display,
                    answered.as_deref(),
                ));
            }
            Err(raw) => findings.push(Finding::Note(format!(
                "{hook_display} has an unreadable `# spoolway-requires:` line (`{raw}`)"
            ))),
        }
    }
    findings
}

/// A patch layer changes what runs without `git status` ever showing it —
/// reported here as a standing note naming every overridden artifact,
/// whichever way `dispatch`'s own gate turned out: acknowledging the gate
/// only means a person has agreed to run under the layer, not that the
/// layer stops being worth mentioning to somebody reading `doctor` cold.
fn override_layer_note(repo: &Repo) -> Vec<Finding> {
    let rows = match collect_override_rows(repo) {
        Ok(rows) => rows,
        Err(err) => {
            return vec![Finding::Note(format!(
                "the override layer could not be read: {err:#}"
            ))];
        }
    };
    if rows.is_empty() {
        return Vec::new();
    }
    let names: Vec<&str> = rows.iter().map(|row| row.target.as_str()).collect();
    let mut findings = vec![Finding::Note(format!(
        "overrides are active: {}",
        names.join(", ")
    ))];
    for row in &rows {
        for item in &row.ignored {
            findings.push(Finding::Note(item.notice()));
        }
    }
    findings
}

/// One note naming every project home under `~/.spoolway/` whose checkout
/// no longer exists, so a person can find the folders and delete them.
///
/// Nothing in spoolway ever removes a home, so a deleted scratch repository
/// leaves one behind for good. This only reports: it is a note, never a
/// failure, and deletes nothing. A home-mode workspace counts only when every
/// one of its clones is gone, since one live clone still uses the workspace.
fn orphaned_home_note() -> Vec<Finding> {
    let gone: Vec<String> = crate::repo::recorded_checkouts()
        .into_iter()
        .filter(|(_, roots)| roots.iter().all(|root| !root.exists()))
        .map(|(home, _)| home.display().to_string())
        .collect();
    if gone.is_empty() {
        return Vec::new();
    }
    vec![Finding::Note(format!(
        "{} project home(s) under ~/.spoolway/ record a checkout that is gone; \
         delete a folder to remove it: {}",
        gone.len(),
        gone.join(", ")
    ))]
}

/// One note naming every key the checkout's config holds that this binary
/// does not know.
///
/// Read off the raw file for the same reason [`worktree_root_note`] is: the
/// loaded [`Config`] has already forgotten them. A newer binary's key and a
/// typo look the same from here, so the note says both and leaves the choice
/// to the person. Nothing is wrong enough to fail a check — the file loads
/// and every key is kept — which is why this is a note and not a failure.
fn unknown_key_notes(checkout: &Path) -> Vec<Finding> {
    let Ok(raw) = std::fs::read_to_string(Config::path_in(checkout)) else {
        return Vec::new();
    };
    let names: Vec<String> = crate::config::unknown_keys(&raw)
        .iter()
        .map(|path| path.join("."))
        .collect();
    if names.is_empty() {
        return Vec::new();
    }
    vec![Finding::Note(format!(
        "this checkout's config holds {} key(s) this spoolway does not know: {} — they are \
         kept and otherwise ignored; a newer spoolway may read them, and if not they are typos",
        names.len(),
        names.join(", "),
    ))]
}

/// Retired: the guard this used to size — how many times a lane may be
/// LAUNCHED at the step a task is on before a person is asked instead — is a
/// constant now (`dispatch::MAX_LAUNCHES`), never a number anybody tuned in
/// practice. Both spellings still parse and both are dropped on the next
/// save; the only way a project finds out is here.
fn retired_key_notes(checkout: &Path, tasks: &[Task]) -> Vec<Finding> {
    let mut findings = Vec::new();
    if names_key(checkout, "max_attempts") || names_key(checkout, "max_launches") {
        findings.push(Finding::Note(
            "dispatch.max_launches (and its old name, max_attempts) is retired in this \
             checkout's config"
                .into(),
        ));
    }
    findings.extend(worktree_root_note(checkout, tasks));
    findings
}

/// `dispatch.worktree_root` is retired, dropped unconditionally by the next
/// `spoolway sync`. A blank value was never a real decision — nothing
/// configured there a project would want to clean up — so this says nothing
/// until the file actually names a path, and then names that path, the same
/// way `spoolway sync` itself does once it runs (see [`crate::sync::config`]).
/// Read off the raw file rather than the loaded [`Config`]: the struct keeps
/// the retired field only so an existing file still parses, not as something
/// any other part of this command should read.
///
/// A queued task can still have its own worktree sitting under the old
/// path — `dispatch` never moves a lane already cut, only new ones — so
/// this never calls the old directory safe to remove on its own: it checks
/// `tasks` for one whose `worktree_path` sits under the configured path and,
/// when it finds any, names them instead. Only once none do does it say so.
fn worktree_root_note(checkout: &Path, tasks: &[Task]) -> Vec<Finding> {
    let configured = std::fs::read_to_string(Config::path_in(checkout))
        .ok()
        .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok())
        .and_then(|doc| {
            doc.get("dispatch")?
                .get("worktree_root")?
                .as_str()
                .map(str::to_string)
        })
        .filter(|path| !path.trim().is_empty());
    let Some(path) = configured else {
        return Vec::new();
    };
    let still_there = crate::config::tasks_under_worktree_root(&path, tasks);
    let note = if still_there.is_empty() {
        format!(
            "dispatch.worktree_root in this checkout's config names {path} — the setting is \
             retired, every worktree now lands under the project home, and the next `spoolway \
             sync` drops the key; no queued task has a worktree under the old path"
        )
    } else if still_there.len() == 1 {
        format!(
            "dispatch.worktree_root in this checkout's config names {path} — the setting is \
             retired, every worktree now lands under the project home, and the next `spoolway \
             sync` drops the key; queued task {} still has a worktree there",
            still_there[0],
        )
    } else {
        format!(
            "dispatch.worktree_root in this checkout's config names {path} — the setting is \
             retired, every worktree now lands under the project home, and the next `spoolway \
             sync` drops the key; {} queued tasks still have a worktree there: {}",
            still_there.len(),
            still_there.join(", "),
        )
    };
    vec![Finding::Note(note)]
}

/// `pipelines are valid` and `task dependency graph` bracket one note about
/// an unattended run with no ceiling at all — grouped together because all
/// three read `config.unattended`, and `doctor` reports them in this order.
fn pipeline_graph_checks(pipelines: &Pipelines, config: &Config, graph: &Graph) -> Vec<Finding> {
    let mut findings = vec![Finding::Check(
        "pipelines are valid".into(),
        pipelines
            .validate()
            .map(|()| Some(format!("{:?}", pipelines.names()))),
    )];
    // An unattended run has exactly two things that can stop it short of an
    // empty queue — one in tokens, one in money — and a project that turned
    // the mode on without setting either has asked for a run which nothing
    // ends. Worth saying out loud here because it cannot be noticed anywhere
    // else — every setting here is valid, each is fine on its own, and the
    // combination only shows up as a bill. Either ceiling alone is enough;
    // only their both being off is worth a note.
    if config.unattended.enabled
        && config.unattended.max_output_tokens == 0
        && config.unattended.max_cost_usd <= 0.0
    {
        findings.push(Finding::Note(
            "unattended.enabled is on with no unattended.max_output_tokens and no \
             unattended.max_cost_usd"
                .into(),
        ));
    }
    findings.push(Finding::Check(
        "task dependency graph".into(),
        graph.validate().map(|()| Some(graph.summary())),
    ));
    findings
}

/// The branch (or branches) work is based on, whether the forge already knows
/// about them, and — the one thing that actually shells out to `gh` — whether
/// `gh` itself is usable when something here would call it.
///
/// Grouped together because the branches computed for the first two checks
/// (`bases`) feed the second, and because all three answer one question a
/// person asks together: can a task queued right now actually get its work
/// out?
fn branch_and_forge_checks(
    repo: &Repo,
    tasks: &[Task],
    pipelines: &Pipelines,
    tracking: &crate::config::IssueTrackingConfig,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // The branches work is actually based on — what the queue names, which with
    // several plans in flight is several branches and none of them necessarily
    // the one this checkout is on. An empty queue has nothing to report yet, so
    // the branch a task queued here would get stands in for it.
    let mut bases: Vec<String> = tasks.iter().filter_map(|t| t.front.base.clone()).collect();
    bases.sort();
    bases.dedup();
    if bases.is_empty() {
        findings.push(Finding::Check(
            "on a usable branch".into(),
            repo.branch().map(Some),
        ));
        bases.extend(repo.branch());
    } else {
        findings.push(Finding::Check(
            "work is based on".into(),
            Ok(Some(bases.join(", "))),
        ));
    }

    // Task pull requests are opened against their base, so the forge has to
    // know about it before the first one.
    if repo.has_remote() {
        for base in &bases {
            let listed = repo
                .git(&["ls-remote", "--heads", "origin", base])
                .map(|listed| {
                    Some(if listed.trim().is_empty() {
                        format!("`{base}` is not on origin yet — dispatch will push it")
                    } else {
                        format!("`{base}` is on origin")
                    })
                });
            findings.push(Finding::Check(format!("`{base}` is publishable"), listed));
        }
    } else {
        // A note, not a problem. Nothing in a pipeline says it reaches a forge
        // any more — what `handover` does with a branch is its own step's
        // business — so spoolway cannot tell a project that finishes by
        // merging into its own checkout from one whose hand-off is about to
        // fail. Saying which it is here would be guessing, and reporting a
        // deliberate setup as a fault sends somebody to add a remote nothing
        // would use.
        findings.push(Finding::Note("no git remote".into()));
    }

    // `spoolway stack` shells out to `gh` for everything past the push — the
    // pull request and the stack registration both go through it — and a
    // missing or unauthenticated `gh` fails there with no earlier symptom.
    // But a project whose pipeline never reaches a forge — no step calls
    // `spoolway stack`, and issue tracking does not name the `github` hook —
    // has nothing to lose from that, so this is a hard problem only when
    // something here would actually run `gh`.
    if reaches_forge(pipelines, tracking) {
        findings.push(Finding::Check(
            "gh is on PATH and authenticated".into(),
            gh_status(),
        ));
    } else if let Err(err) = gh_status() {
        findings.push(Finding::Note(format!("`gh` is not usable ({err:#})")));
    }

    findings
}

/// Whether a lane can be started at all — the one check that never depends on
/// the pipeline or the config, only on whether the terminal multiplexer this
/// machine has is one spoolway knows how to drive.
fn mux_check(mux: &dyn Mux) -> Finding {
    Finding::Check(
        "lanes can be started".into(),
        if mux.is_available() {
            Ok(Some(mux.name().into()))
        } else {
            Err(anyhow::anyhow!("{}", mux.unavailable()))
        },
    )
}

/// [`mux_check`], tolerant of `backend()` itself failing to resolve this
/// project's stamped home — the same "lanes can be started" row a person
/// reads either way, failed for that reason instead of the backend's own
/// unavailability. `doctor` is the one command that must never bail out on
/// this rather than say so as a row.
fn mux_finding(mux: &Result<Box<dyn Mux>>) -> Finding {
    match mux {
        Ok(mux) => mux_check(mux.as_ref()),
        Err(err) => Finding::Check(
            "lanes can be started".into(),
            Err(anyhow::anyhow!("{err:#}")),
        ),
    }
}

/// Open a throwaway pane on a scratch directory, run a trivial command in
/// it, wait for the wrapper's own pid file, and close everything again —
/// the one check here that finds out a lane can *really* start rather than
/// only that the backend answers. Reuses the same wrapper and wait
/// [`crate::command_step::Runs`] gives a real command step, so this is
/// exactly the path a lane's own turn takes, not a stand-in for it.
///
/// `live` is [`LiveCheckMode::Skip`] for `--no-live`: skips this row alone,
/// leaving every other check unchanged, for a machine where opening a real
/// pane is slow or noisy (CI, say) but the rest of `doctor` is still worth
/// running.
///
/// A backend with no real pane to test — headless, whose panes are names
/// rather than processes — answers with a note instead of a check: nothing
/// was opened, so there is nothing to say passed or failed.
///
/// Also skipped, with its own note, when [`Mux::in_own_pane`] says this
/// process is not actually inside the backend it is about to open a pane
/// in — a herdr server answers `is_available` from any shell, with or
/// without `HERDR_*` set, so without this gate a plain `doctor` run outside
/// herdr would open a throwaway pane in the caller's real session instead
/// of a sandbox nobody is watching. [`LiveCheckMode::Forced`] (`--live`) is
/// the only way past that gate, for the one caller who really is asking to
/// reach into a herdr session from outside a pane in it.
fn live_check(mux: &dyn Mux, live: LiveCheckMode) -> Finding {
    if live == LiveCheckMode::Skip {
        return Finding::NoteVerbose("--no-live: skipped the live pane check".into());
    }
    if !mux.is_available() {
        // Already a `FAIL` on `lanes can be started` — nothing more to say.
        return Finding::NoteVerbose("no live pane check: this backend is not available".into());
    }
    // A herdr server answers `agent list` (and so `is_available`) from any
    // shell, with or without `HERDR_*` set — the same reason
    // `commands::dispatch`'s own pane gate reads `in_own_pane` rather than
    // `is_available` before it will start a lane. Opening a throwaway pane
    // here for a caller not actually inside this session would land it in
    // the person's real herdr instead of a sandbox nobody is watching.
    // `--live` is the deliberate override for the caller who means to reach
    // outside their own pane anyway.
    if live != LiveCheckMode::Forced && !mux.in_own_pane() {
        return Finding::NoteVerbose(
            "no live pane check: not inside a herdr pane — run doctor from inside herdr, or \
             pass --live, to check this"
                .into(),
        );
    }
    match live_pane(mux) {
        Ok(Some(note)) => Finding::Check("a lane really starts".into(), Ok(Some(note))),
        Ok(None) => Finding::NoteVerbose(format!(
            "no live pane check: {} has no real pane",
            mux.name()
        )),
        Err(err) => Finding::Check("a lane really starts".into(), Err(err)),
    }
}

/// The three calls [`live_check`] makes: [`Mux::create_pane`] a throwaway
/// workspace on a scratch directory, [`Mux::run_in_pane`] to actually run a
/// trivial command in it, then [`Mux::close_pane`] twice — the split pane
/// the command ran in, then the workspace's own root pane, which under
/// every real backend takes its now-empty tab and workspace with it.
///
/// `Ok(None)` from a backend whose [`Workspace::tab_id`] is absent —
/// headless, which hands back a pane id that names no real process — since
/// there is nothing here for [`Mux::run_in_pane`] to run a script in.
///
/// The scratch directory is removed on every way out — the early `Ok(None)`,
/// every `?` that bails before that, and the final outcome whether it is
/// `Ok` or `Err` — rather than left for [`crate::scratch::root`]'s own later
/// sweep: that sweep is for a run a crash cut short, not for a check that
/// ran to completion and has nothing left to explain by leaving its folder
/// behind.
fn live_pane(mux: &dyn Mux) -> Result<Option<String>> {
    let dir = crate::scratch::root("doctor-live");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let result = live_pane_in(&dir, mux);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// [`live_pane`]'s own body, pulled apart so the directory it works in can be
/// cleaned up on every exit from that function alike, including the ones
/// this inner call reaches through `?`.
fn live_pane_in(dir: &Path, mux: &dyn Mux) -> Result<Option<String>> {
    let workspace = mux.create_pane(dir, "doctor")?;
    let Some(tab_id) = workspace.tab_id.clone() else {
        return Ok(None);
    };

    let runs = crate::command_step::Runs::new(dir);
    let key = "doctor · live";
    let env = std::collections::BTreeMap::new();
    let script = runs.script_for_pane(key, "true", &env)?;

    let started = std::time::Instant::now();
    let outcome = mux
        .run_in_pane(&tab_id, dir, "doctor", "doctor", &script, &env)
        .and_then(|pane| {
            pane.ok_or_else(|| {
                anyhow::anyhow!("{} reports panes but ran nothing in one", mux.name())
            })
        })
        .and_then(|pane| runs.await_started(key).map(|_pid| pane));
    // Nothing routes on this run's code — the check's own outcome is read
    // above — so a clearing that could not happen is not this check's to
    // fail on.
    let _ = runs.forget(key);

    // Best-effort either way: a pane left standing after a failed check is a
    // worse trail to leave than a close call whose own error is swallowed.
    if let Ok(pane) = &outcome {
        let _ = mux.close_pane(pane);
    }
    let _ = mux.close_pane(&workspace.pane_id);
    // Stopped only now, after both closes: the row says how long the whole
    // check took — opening the pane, running the command, closing
    // everything again — not just the time to see it start.
    let elapsed = started.elapsed();

    outcome.map(|pane| {
        Some(format!(
            "pane {pane}, closed in {:.1}s",
            elapsed.as_secs_f64()
        ))
    })
}

/// Per agent profile a pipeline actually references: whether its binary is on
/// PATH, whether every step that runs on it names a model, and whether its
/// permission mode is one spoolway recognises. Three checks per agent rather
/// than one, because each is fixed a different way and a person should not
/// have to guess which of three things "agent `x` is broken" means.
fn agent_checks(repo: &Repo, pipelines: &Pipelines, config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (agent, steps) in pipelines.referenced_agents() {
        let outcome = config.agent(agent).and_then(|profile| {
            let found = which(&profile.kind);
            match found {
                Some(path) => Ok(Some(format!("{} at {path}", profile.kind))),
                None => Err(anyhow::anyhow!(
                    "`{}` is not on PATH, but {steps:?} need it",
                    profile.kind
                )),
            }
        });
        findings.push(Finding::Check(format!("agent `{agent}`"), outcome));

        // spoolway names no model of its own: every step that runs on this
        // agent has to name one, and finding that out here beats finding it
        // out from an agent that was handed an empty `--model`.
        // `blocked` is materialised even in attended mode, where it parks for
        // a person and launches no agent. Config checks enforce its model once
        // unattended mode actually staffs it; do not misdirect an attended
        // project to pipeline YAML for this config-derived blank.
        //
        // Grouped by pipeline, and checked against that pipeline's own steps
        // directly, rather than flattened into one `{missing:?}` debug list of
        // bare step ids the way this used to read: a step id can repeat across
        // pipelines that have nothing else to do with each other, and the fix
        // for a missing model always lives in one specific pipeline's own
        // file — so the message names that pipeline and points at that file's
        // real absolute path, not a `<name>.yml` placeholder that may not even
        // be the one dir this checkout uses.
        let mut problems = Vec::new();
        for (name, pipeline) in &pipelines.pipelines {
            let missing: Vec<&str> = pipeline
                .steps
                .iter()
                .filter(|step| step.agent.as_deref() == Some(agent))
                .filter(|step| step.run.is_none())
                .filter(|step| config.unattended.enabled || step.id != crate::pipeline::BLOCKED)
                .filter(|step| !step.model.as_deref().is_some_and(|m| !m.trim().is_empty()))
                .map(|step| step.id.as_str())
                .collect();
            if missing.is_empty() {
                continue;
            }
            let path = pipeline
                .private_file
                .clone()
                .unwrap_or_else(|| Pipelines::file_in(&repo.checkout, name));
            let (noun, verb, pronoun) = match missing.len() {
                1 => ("step", "names", "it"),
                _ => ("steps", "name", "them"),
            };
            problems.push(format!(
                "pipeline `{name}` {noun} {} {verb} no model, running on `{agent}` — give \
                 {pronoun} one in {}",
                missing.join(", "),
                path.display()
            ));
        }
        let model = if problems.is_empty() {
            Ok(Some("every step names a model".into()))
        } else {
            Err(anyhow::anyhow!(problems.join("; ")))
        };
        findings.push(Finding::Check(
            format!("agent `{agent}` has a model"),
            model,
        ));

        // A wrong mode is only ever caught here or at the lane, and at the lane
        // it is an agent dying on an unrecognised flag several passes into a
        // run — long after whoever set it has stopped watching.
        let permissions = config
            .agent(agent)
            .and_then(|profile| profile.permission_mode_status());
        findings.push(Finding::Check(
            format!("agent `{agent}` permission mode"),
            permissions,
        ));
    }
    findings
}

/// The note for every `[models]` row no step routes to. Kept out of
/// `model_health_checks` because that function also feeds `cheap_findings`,
/// and an unrouted row is dead config that costs nothing at run time: it is
/// worth naming in the `spoolway doctor` report, but not worth stopping a
/// person at the warnings popup before a run.
///
/// A row nothing routes to is exactly what a rename leaves behind: a step's
/// model is renamed in a pass that leaves its own row unreached.
fn unrouted_model_notes(pipelines: &Pipelines, config: &Config) -> Vec<Finding> {
    crate::models::unrouted(pipelines, &config.models)
        .into_iter()
        .map(|glob| Finding::Note(format!("model `{glob}` routes to no step")))
        .collect()
}

/// Every check and note that reads a model's own settings rather than an
/// agent profile's: the legacy/template placeholder, a name that resolves to nothing,
/// a step nothing caps, a `slots` model that has not said whether it is
/// `local`, a `session_blocked_ctx` set against a model that cannot
/// honour it, and a step whose `compact_ctx` cannot take effect. Grouped
/// together because all six walk the same `pipelines`/`config.models` data,
/// several of them by way of the same `named` map.
fn model_health_checks(pipelines: &Pipelines, config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();

    // The one unresolvable name that is a failure rather than a note: the
    // placeholder older scaffolds and the annotated template carry, which
    // says "name your local model here" and is not a model. It satisfies checks that ask
    // whether a step names something — that is what a placeholder does — so a
    // project that has not read the comment above it looks configured and is
    // not, and finds out one lane later when an agent is handed a model
    // nothing serves.
    let named = crate::models::named(pipelines);
    let unset: Vec<&str> = named
        .get(crate::models::PLACEHOLDER)
        .cloned()
        .unwrap_or_default();
    // Labelled apart from `agent \`x\` has a model`'s own check, even though
    // both are about a step's `model:` — that one already fails and lists
    // the steps when one is missing outright, and a passing row here with
    // the old label "a model is named for every step" read as contradicting
    // it, right next to it in the report, on a project this placeholder
    // check was never about.
    findings.push(Finding::Check(
        "no step still names the spoolway placeholder model".into(),
        match unset.is_empty() {
            true => Ok(None),
            false => Err(anyhow::anyhow!(
                "{} still name `{}`, which is a spoolway placeholder rather than a \
                 model — put your own local model's name in the pipeline file",
                unset.join(", "),
                crate::models::PLACEHOLDER
            )),
        },
    ));

    // A step naming a model is not the same as that name meaning anything.
    // This is a note rather than a failure — an unpriced, unsized model is a
    // lane that still runs, just unaccounted, exactly like a dropped
    // `{session_id}` — but it is worth a person seeing before `spoolway eval`
    // shows them a bill with a hole in it. The placeholder is left out: it was
    // a problem a moment ago, and saying it twice in two weights reads as two
    // findings.
    let unresolved: Vec<(&str, Vec<&str>)> = named
        .iter()
        .filter(|(model, _)| **model != crate::models::PLACEHOLDER)
        .filter(|(model, _)| {
            crate::models::resolve(&config.models, model).source == crate::models::Source::Unknown
        })
        .map(|(model, steps)| (*model, steps.clone()))
        .collect();
    if !unresolved.is_empty() {
        for (model, steps) in &unresolved {
            findings.push(Finding::Note(format!(
                "model `{model}` resolves to nothing, and {} will run it",
                steps.join(", ")
            )));
        }
    }

    // Nothing caps this step at all. The two counts that could — the model's
    // own `slots` and its profile's `concurrency` — are both a fallback for
    // the other, so a step whose model sets no `slots` and whose profile sets
    // no `concurrency` runs as many lanes at once as the queue offers it.
    //
    // That combination is reachable by doing nothing rather than by asking for
    // it: the shipped local profiles carry no `concurrency` on purpose, since
    // how much a local server serves at once is the model's fact and not the
    // harness's, and a project that has not yet written its models into
    // `[models]` lands here without touching a setting. Unlimited is still a
    // legal answer — `concurrency = 0` has always meant it — so this is a note
    // and not a failure, but it should be a choice somebody made.
    // Keyed by the pair, and not by the step: the same model on the same
    // profile is one thing to fix however many steps name it, and a line per
    // step said it six times in a shipped pipeline. The steps are still listed,
    // the way the two checks either side of this one list theirs.
    let mut uncapped: std::collections::BTreeMap<(&str, &str), Vec<&str>> =
        std::collections::BTreeMap::new();
    for step in pipelines
        .pipelines
        .values()
        .flat_map(|pipeline| &pipeline.steps)
        .filter(|step| step.kind() == StepKind::Agent)
    {
        let Some(model) = step.model.as_deref().filter(|m| !m.trim().is_empty()) else {
            continue;
        };
        if model == crate::models::PLACEHOLDER {
            continue;
        }
        let Some(agent) = step.agent.as_deref() else {
            continue;
        };
        let Some(profile) = config.agents.get(agent) else {
            continue;
        };
        if profile.concurrency > 0 {
            continue;
        }
        let slots = crate::models::resolve(&config.models, model)
            .price
            .map_or(0, |p| p.slots);
        if slots > 0 {
            continue;
        }
        let steps = uncapped.entry((agent, model)).or_default();
        if !steps.contains(&step.id.as_str()) {
            steps.push(&step.id);
        }
    }
    for ((agent, model), steps) in &uncapped {
        findings.push(Finding::Note(format!(
            "nothing caps `{model}` on agent `{agent}`, and {} will run on it",
            steps.join(", ")
        )));
    }

    // `slots` describes one card's worth of hardware, so a model carrying it
    // is almost certainly local. Setting `local` lifts the 5m
    // `prompt_cache_ttl` default from it, so a carried session is not refused
    // for age, and it quiets this note.
    // Read straight off `config.models` rather than the `named` map: the flag
    // is worth setting on a sized model whether or not a pipeline here points
    // at it yet. `local` itself is never a failure, so this is a note.
    for (glob, price) in &config.models {
        if price.local || price.slots == 0 {
            continue;
        }
        findings.push(Finding::Note(format!(
            "model `{glob}` sets `slots` but not `local`"
        )));
    }

    // `agents.<profile>.session_blocked_ctx` only ever fires against the
    // model that actually answered — resolved the same way
    // `dispatch::ctx_ceiling_hold` reads it, through `models::resolve`. A
    // profile that sets the ceiling against a step whose model resolves no
    // `context_window` believes it has a brake that nothing is pulling: the
    // dispatcher's own check answers `None` for exactly this config, in
    // silence, so this is the one place a person finds out.
    let mut blocked_ceiling_unresolved: std::collections::BTreeSet<(&str, &str)> =
        std::collections::BTreeSet::new();
    for step in pipelines
        .pipelines
        .values()
        .flat_map(|pipeline| &pipeline.steps)
        .filter(|step| step.kind() == StepKind::Agent)
    {
        let Some(agent) = step.agent.as_deref() else {
            continue;
        };
        let Some(profile) = config.agents.get(agent) else {
            continue;
        };
        if profile.session_blocked_ctx == 0 {
            continue;
        }
        let Some(model) = step.model.as_deref().filter(|m| !m.trim().is_empty()) else {
            continue;
        };
        let resolves = crate::models::resolve(&config.models, model)
            .price
            .is_some_and(|price| price.context_window > 0);
        if !resolves {
            blocked_ceiling_unresolved.insert((agent, model));
        }
    }
    for (agent, model) in &blocked_ceiling_unresolved {
        let ceiling = config.agents[*agent].session_blocked_ctx;
        findings.push(Finding::Note(format!(
            "agents.{agent}: session_blocked_ctx = {ceiling}, but `{model}` resolves to no \
             context_window"
        )));
    }

    // One check per step, from the function the dispatcher refuses the same
    // step with: a profile's block and a model's compaction meet only in a step.
    for (pipeline_name, pipeline) in &pipelines.pipelines {
        for step in pipeline
            .steps
            .iter()
            .filter(|step| step.kind() == StepKind::Agent)
        {
            let (Some(agent), Some(model)) = (
                step.agent.as_deref(),
                step.model.as_deref().filter(|m| !m.trim().is_empty()),
            ) else {
                continue;
            };
            let Some(profile) = config.agents.get(agent) else {
                continue;
            };
            // A passing step is a counted check too, so the total reads the
            // same whether or not any step is refused.
            let outcome =
                match crate::models::compaction_problem(&config.models, agent, profile, model) {
                    Some(problem) => Err(anyhow::anyhow!(problem)),
                    None => Ok(None),
                };
            findings.push(Finding::Check(
                format!("pipeline `{pipeline_name}` step `{}`", step.id),
                outcome,
            ));
        }
    }

    findings
}

/// Per configured agent profile — not only the ones a pipeline references —
/// whether its `kind` is one spoolway knows how to launch at all, and, for a
/// kind that launches but that spoolway cannot meter, what that costs a
/// project that adopted it.
fn agent_kind_checks(config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    // A profile's argv is fixed per kind in `agent::ADAPTERS` now, so a lane
    // starting with no prompt can only mean the kind itself is not one
    // spoolway knows how to launch.
    for (name, profile) in &config.agents {
        let adapter = crate::agent::adapter(&profile.kind);
        if adapter.map(|a| a.args.is_empty()).unwrap_or(true) {
            findings.push(Finding::Check(
                format!("agent `{name}` kind"),
                Err(anyhow::anyhow!(
                    "spoolway does not yet know how to launch kind `{}` — add an `args` row to \
                     `agent::ADAPTERS`",
                    profile.kind
                )),
            ));
            continue;
        }

        // A note, and never a failure. An unmetered kind launches, runs,
        // reports and lands its work; what it loses is three readings, two of
        // which already have a fallback the dispatcher takes today. A project
        // running one on purpose should not have a permanently red doctor —
        // but it should not be able to adopt one without ever being told what
        // it gave up either, and this is the only surface that would otherwise
        // stay silent about it.
        if adapter.is_some_and(|a| !a.meters()) {
            findings.push(Finding::Note(format!(
                "agent `{name}` runs kind `{}`, which spoolway cannot account for",
                profile.kind
            )));
        }
    }
    findings
}

/// Every prompt a pipeline's agent steps actually run: whether its file
/// exists. The lint over a prompt's own prose is `spoolway pipeline check`'s
/// alone now, since a surviving finding there fails the build — doctor would
/// only be repeating a check that already has a sharper place to fail.
fn prompt_checks(repo: &Repo, pipelines: &Pipelines) -> Vec<Finding> {
    let mut findings = Vec::new();

    // One row per prompt file rather than one per step that names one:
    // `path_for` resolves a prompt from its name alone and never sees a step,
    // so the step id on a passing row never described what was read. The steps
    // come back when the file is missing, which is the only time they say
    // anything — and in prompt-name order, since step order stops meaning
    // anything once a file is checked once.
    let mut prompts: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for step in pipelines.pipelines.values().flat_map(|p| &p.steps) {
        if step.kind() != StepKind::Agent {
            continue;
        }
        let steps = prompts.entry(step.prompt_name()).or_default();
        if !steps.contains(&step.id.as_str()) {
            steps.push(&step.id);
        }
    }
    for (prompt, steps) in &prompts {
        let path = crate::prompt::path_for(repo, prompt);
        findings.push(Finding::Check(
            format!("prompt `{prompt}`"),
            if path.exists() {
                Ok(None)
            } else {
                Err(anyhow::anyhow!(
                    "{} is missing, but {steps:?} run it — run `spoolway init`",
                    path.display()
                ))
            },
        ));
    }

    // A prompt directory nothing runs is what a release leaves behind when it
    // stops shipping one — `summariser` went with `[stack.summary]` — and
    // what a step rename leaves when its prompt was not renamed with it.
    // `sync` never deletes a prompt, since the project may have written in
    // it; so it is said here, once, rather than found by whoever opens the
    // directory next.
    if let Ok(entries) = crate::prompt::entries(repo) {
        let used = crate::prompt::users(pipelines);
        for entry in entries {
            if used.contains_key(&entry.name) {
                continue;
            }
            let label = if entry.private { " (private)" } else { "" };
            findings.push(Finding::Note(format!(
                "prompt `{}`{label} is run by no step",
                entry.name
            )));
        }
    }

    findings
}

/// Cron jobs: one whose table carries a key no job reads, one that names a
/// routine that is gone, one whose expression will not parse, one whose pipeline is not defined. A store that will not
/// load at all — a name in both, malformed TOML — is a single finding.
fn jobs_checks(repo: &Repo, pipelines: &Pipelines) -> Vec<Finding> {
    let mut findings = Vec::new();

    let jobs = match crate::jobs::load(repo) {
        Ok(jobs) => jobs,
        Err(err) => {
            findings.push(Finding::Check("job stores load".into(), Err(err)));
            return findings;
        }
    };

    for job in &jobs {
        let name = &job.name;

        findings.push(Finding::Check(
            format!("job `{name}` has no unknown keys"),
            match job.unknown_keys_note() {
                Some(note) => Err(anyhow::anyhow!(note)),
                None => Ok(None),
            },
        ));

        // Parses *and* comes round: `0 0 30 2 *` parses cleanly and never
        // fires, and `spoolway jobs list` / the dispatcher's "run doctor"
        // line both need this to be the thing that names it.
        findings.push(Finding::Check(
            format!("job `{name}` schedule fires"),
            match crate::cron::Cron::parse(&job.spec.schedule) {
                Err(why) => Err(anyhow::anyhow!("`{}` — {why}", job.spec.schedule)),
                Ok(cron) => match cron.next_after(chrono::Local::now().naive_local()) {
                    Some(_) => Ok(None),
                    None => Err(anyhow::anyhow!(
                        "`{}` parses but never comes round — check its day-of-month and month \
                         fields in {}",
                        job.spec.schedule,
                        job.source.display()
                    )),
                },
            },
        ));

        findings.push(Finding::Check(
            format!("job `{name}` routine exists"),
            match job.target(repo) {
                Err(why) => Err(why),
                Ok(target) if target.exists() => Ok(None),
                Ok(target) => Err(anyhow::anyhow!(
                    "{} is not there — nothing under `.spoolway/routines/` matches `{}`",
                    target.display(),
                    job.spec.routine
                )),
            },
        ));

        findings.push(Finding::Check(
            format!("job `{name}` pipeline is defined"),
            if pipelines.pipelines.contains_key(&job.spec.pipeline) {
                Ok(None)
            } else {
                Err(anyhow::anyhow!(
                    "`{}` is not a pipeline (defined: {})",
                    job.spec.pipeline,
                    pipelines.names().join(", ")
                ))
            },
        ));
    }

    findings
}

/// Whether anything here would actually shell out to `gh`: a step whose
/// `run:` calls `spoolway stack`, or issue tracking naming the `github` hook.
/// Those are the only two callers `gh` has anywhere in this project — see
/// `commands::stack::gh_program` and `assets/hooks/github.sh` — so a project
/// with neither will never notice whether `gh` works at all.
fn reaches_forge(pipelines: &Pipelines, tracking: &crate::config::IssueTrackingConfig) -> bool {
    tracking.hook.trim() == crate::cli::Tracker::Github.hook_name()
        || crate::commands::dispatch::stack_step(pipelines).is_some()
}

/// Whether `gh` can actually do what `spoolway stack` needs of it: found on
/// PATH, and holding a working login.
///
/// Two different failures rather than one, because they are fixed two
/// different ways — installing the binary and running `gh auth login` are not
/// the same afternoon — and a person seeing only "gh is broken" would not
/// know which one to reach for.
fn gh_status() -> Result<Option<String>> {
    let Some(path) = which("gh") else {
        bail!("`gh` is not on PATH — `spoolway stack` cannot open a pull request without it");
    };
    let output = std::process::Command::new("gh")
        .args(["auth", "status"])
        .output()
        .with_context(|| format!("running `{path} auth status`"))?;
    if !output.status.success() {
        bail!(
            "`gh` is at {path} but is not authenticated: {}",
            first_line(&String::from_utf8_lossy(&output.stderr))
        );
    }
    Ok(Some(path))
}

/// What `spoolway sync` would do here, and above all what it would refuse.
///
/// The report `sync` prints is the paths it wrote and one line, because
/// which files moved is the only question anybody runs it to answer. That
/// leaves one thing homeless: a file it *declined* to touch, where somebody
/// has edited the half a machine reads. Silence there is a project quietly
/// running an old contract, so it surfaces here — which is where
/// `docs/installation.md` has claimed to report it all along.
///
/// Notes rather than problems, both of them. An outstanding sync is a
/// command somebody has not run yet, and a refusal is usually a deliberate
/// edit; neither is a broken project, and neither should fail a `doctor` that
/// a lane runs.
fn doctor_sync(repo: &Repo, report: &mut Report) {
    let dry = crate::cli::SyncArgs {
        dry_run: true,
        replace: Vec::new(),
    };
    let Ok(outcomes) = crate::sync::scan(repo, &dry) else {
        return;
    };
    // The scan reads `repo.root`'s copy, so that is the file whose absence
    // says `init` never ran here.
    let initialised = Config::path_in(&repo.root).is_file();
    for note in sync_notes(&outcomes, initialised) {
        report.note(note);
    }
}

/// The `pipelines load` row's own outcome, once loading has already failed
/// with `err`.
///
/// [`crate::pipeline::Pipelines::refusals`]'s own per-step prose, naming
/// every retired shape a pipeline file still carries — across every file at
/// once rather than only the one `err` itself stopped at — or the original
/// `err`, unchanged, for a load failure none of them explains. Split out
/// from [`doctor`] so it can be tested without driving the whole command.
fn pipelines_load_outcome(root: &Path, err: anyhow::Error) -> Result<Option<String>> {
    let refusals = crate::pipeline::Pipelines::refusals(root);
    if refusals.is_empty() {
        Err(err)
    } else {
        Err(anyhow::anyhow!(refusals.join("; ")))
    }
}

/// The notes [`doctor_sync`] records for a dry `sync` scan — one note
/// counting every file that is behind and naming `spoolway sync`, with the
/// reason `sync` would rewrite each one left out: that reason is often a list
/// of every setting a config has gained since it was written, which has no
/// room in a one-line count.
///
/// A file that is missing altogether from a project that was never
/// initialised — no `config.toml` at all — is not *behind*: nothing was ever
/// written for `sync` to bring forward, and folding it into the "behind"
/// count there would send a person to run `sync` for the wrong reason.
/// Those files get a note of their own naming that fact; the "behind" note
/// keeps only what `sync` is actually for. In an initialised project a
/// missing file is an ordinary thing for `sync` to restore, and stays where
/// it was.
fn sync_notes(outcomes: &[crate::sync::Outcome], initialised: bool) -> Vec<String> {
    let mut notes = Vec::new();
    let mut behind: Vec<&str> = Vec::new();
    let mut never_written: Vec<&str> = Vec::new();
    for outcome in outcomes {
        match outcome {
            crate::sync::Outcome::Wrote { path, detail }
                if !initialised && detail == crate::sync::MISSING =>
            {
                if !never_written.contains(&path.as_str()) {
                    never_written.push(path);
                }
            }
            // One file can be behind for several reasons at once — a config
            // gains a setting and has a note rewritten in the same pass — and
            // this is one line per file, not per reason.
            crate::sync::Outcome::Wrote { path, .. }
            | crate::sync::Outcome::Migrated { path, .. }
            | crate::sync::Outcome::Removed { path, .. } => {
                if !behind.contains(&path.as_str()) {
                    behind.push(path);
                }
            }
            crate::sync::Outcome::Blocked { path, why } => {
                notes.push(format!("{path}: {why}"));
            }
            crate::sync::Outcome::Kept => {}
        }
    }

    notes.extend(
        never_written
            .iter()
            .map(|path| format!("{path} has never been written")),
    );
    if !behind.is_empty() {
        notes.push(format!(
            "{} file(s) are behind this spoolway — run spoolway sync to apply them",
            behind.len()
        ));
    }
    notes
}

/// Both keys `retire-warmth` retired, named for a project that still writes
/// them — parsed straight out of `config.toml` rather than off the loaded
/// [`Config`], since a struct that keeps a retired field only for parsing has
/// already forgotten which profile or glob wrote it.
///
/// `agents.<profile>.session_reuse_uncached` is reported against the models
/// that profile's steps actually run, so a project sees whether it was ever
/// load-bearing rather than a generic "this is gone". `models.<glob>.cache_ttl`
/// and `models.<glob>.session_reuse_idle` are reported per glob: both still
/// parse, under their new name `prompt_cache_ttl`, and are rewritten on the
/// next save either way.
///
/// `config` is the checkout's own loaded copy — the same one `doctor` checks
/// everything else against — so `[models]`'s rates are resolved from the same
/// file the retired keys below were parsed out of, rather than from
/// `repo.config`'s (`repo.root`'s) copy, which can disagree with it.
fn warmth_notes(repo: &Repo, pipelines: &Pipelines, config: &Config, report: &mut Report) {
    let Ok(raw) = std::fs::read_to_string(Config::path_in(&repo.checkout)) else {
        return;
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&raw) else {
        return;
    };

    if let Some(agents) = doc.get("agents").and_then(toml::Value::as_table) {
        for (name, entry) in agents {
            if entry.get("session_reuse_uncached").is_none() {
                continue;
            }
            // The age limit that applies, not the raw field: a model that sets
            // nothing is held to the 5m default, and `"0"` lifts the limit.
            let limited = pipelines
                .pipelines
                .values()
                .flat_map(|p| &p.steps)
                .filter(|step| step.agent.as_deref() == Some(name.as_str()))
                .filter_map(|step| step.model.as_deref())
                .filter(|model| !model.trim().is_empty())
                .find_map(|model| {
                    let price = crate::models::resolve(&config.models, model).price;
                    let limit = crate::usage::ModelPrice::cache_ttl_limit(price.as_ref())?;
                    Some((model, limit))
                });
            match limited {
                Some((model, limit)) => report.note(format!(
                    "agents.{name}.session_reuse_uncached is retired in this checkout's \
                     config, and a carried session under '{model}' is now refused after {}; \
                     set models.'{model}'.prompt_cache_ttl = \"0\" to resume however old",
                    crate::config::human_duration::format(limit)
                )),
                None => report.note(format!(
                    "agents.{name}.session_reuse_uncached is retired in this checkout's \
                     config, and no model this profile runs has a prompt_cache_ttl limit"
                )),
            }
        }
    }

    if let Some(models) = doc.get("models").and_then(toml::Value::as_table) {
        for (glob, entry) in models {
            for old in ["cache_ttl", "session_reuse_idle"] {
                if entry.get(old).is_some() {
                    report.note(format!(
                        "models.'{glob}'.{old} is retired in this checkout's config, renamed \
                         prompt_cache_ttl"
                    ));
                }
            }
        }
    }
}

/// Whether `config.toml` names a key at all, which is not the same question as
/// what the loaded config says: a key absent from the file takes whatever
/// default this spoolway ships, and a key present in it was someone's decision.
///
/// `checkout` because every doctor check reads against the checkout's own
/// copy — see the module doc.
fn names_key(checkout: &Path, key: &str) -> bool {
    std::fs::read_to_string(Config::path_in(checkout))
        .map(|raw| {
            raw.lines().any(|line| {
                line.split('=')
                    .next()
                    .is_some_and(|name| name.trim() == key)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn price_table_age_is_noted_only_after_the_enabled_limit() {
        assert!(price_table_age_note(0, 300).is_none());
        assert!(price_table_age_note(30, 30).is_none());
        assert!(price_table_age_note(30, 29).is_none());
        assert_eq!(
            price_table_age_note(30, 31).as_deref(),
            Some(
                "the price table is 31 days old, past the 30 in `housekeeping.price_max_age_days`"
            )
        );
    }

    /// Two directories, each holding its own `.spoolway/config.toml` with one
    /// key the other's file does not have — standing in for a project's root
    /// and a linked worktree's checkout without paying for real git. What
    /// `doctor` reads is a function of which directory it is handed, not of
    /// anything git-specific, so this is enough to pin that down.
    fn two_configs(
        name: &str,
        root_body: &str,
        checkout_body: &str,
    ) -> (PathBuf, PathBuf, crate::scratch::ScratchRoot) {
        let base = crate::scratch::root(&format!("doctor-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("root");
        let checkout = base.join("checkout");
        for (dir, body) in [(&root, root_body), (&checkout, checkout_body)] {
            let state = dir.join(crate::config::STATE_DIR);
            std::fs::create_dir_all(&state).unwrap();
            std::fs::write(state.join("config.toml"), body).unwrap();
        }
        (root, checkout, base)
    }

    /// `--json`'s reading of a report: a `kind` tag per row, and the same
    /// short-form filtering `render`'s own text form applies — a passing
    /// check and a verbose-only note both left out unless `verbose` is set.
    #[test]
    fn render_json_carries_a_kind_tag_and_honours_verbose() {
        let mut report = Report::default();
        report.check("config parses", Ok(Some("looks fine".to_string())));
        report.check("pipelines are valid", Err(anyhow::anyhow!("bad pipeline")));
        report.note_verbose("no dispatcher running");

        let short = report.render_json(false);
        let rows = short["rows"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "only the failure survives the short form: {rows:?}"
        );
        assert_eq!(rows[0]["kind"], "fail");
        assert_eq!(rows[0]["label"], "pipelines are valid");

        let full = report.render_json(true);
        let rows = full["rows"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            3,
            "verbose keeps the passing check and the note: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r["kind"] == "ok" && r["note"] == "looks fine")
        );
        assert!(rows.iter().any(|r| r["kind"] == "note"));

        assert_eq!(short["checks"], 2);
        assert_eq!(short["problems"], 1);
    }

    /// Builds `<scratch>/.spoolway/` with the given `(folder, project.toml)`
    /// pairs and runs `orphaned_home_note` with that scratch folder standing
    /// in for the home, so the real `~/.spoolway/` is never read.
    fn orphan_note_for(name: &str, homes: &[(&str, Option<String>)]) -> Vec<Finding> {
        let base = crate::scratch::root(&format!("doctor-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let state = base.join(".spoolway");
        for (folder, toml) in homes {
            std::fs::create_dir_all(state.join(folder)).unwrap();
            if let Some(toml) = toml {
                std::fs::write(state.join(folder).join("project.toml"), toml).unwrap();
            }
        }
        crate::platform::test_home::with_home(&base, orphaned_home_note)
    }

    fn note_text(findings: &[Finding]) -> &str {
        assert_eq!(findings.len(), 1, "{findings:?}");
        let Finding::Note(text) = &findings[0] else {
            panic!("expected a note: {findings:?}");
        };
        text
    }

    /// A repo-mode home whose recorded checkout is gone is named by path in
    /// a note; a home whose checkout still exists is not.
    #[test]
    fn orphaned_home_note_names_a_gone_checkout_and_not_a_live_one() {
        let live = crate::scratch::root("doctor-orphan-live-checkout");
        std::fs::create_dir_all(&live).unwrap();
        let findings = orphan_note_for(
            "orphan-basic",
            &[
                (
                    "dead-aaaa",
                    Some("id = \"aaaa\"\nroot = \"/nonexistent/spoolway-dead\"\n".into()),
                ),
                (
                    "live-bbbb",
                    Some(format!(
                        "id = \"bbbb\"\nroot = {:?}\n",
                        live.display().to_string()
                    )),
                ),
            ],
        );
        let text = note_text(&findings);
        assert!(text.contains(".spoolway/dead-aaaa"), "{text}");
        assert!(!text.contains("live-bbbb"), "{text}");
        assert!(text.contains("is gone"), "{text}");
    }

    /// A workspace is orphaned only when every clone is gone: one live clone
    /// keeps it off the list, and all clones gone puts it on.
    #[test]
    fn orphaned_home_note_needs_every_clone_gone() {
        let live = crate::scratch::root("doctor-orphan-live-clone");
        std::fs::create_dir_all(&live).unwrap();
        let clone = |root: &str| format!("[[clones]]\nroot = {root:?}\ndispatcher = \"d\"\n");
        let mixed = format!(
            "id = \"w1\"\n{}{}",
            clone("/nonexistent/clone-a"),
            clone(&live.display().to_string())
        );
        let all_gone = format!(
            "id = \"w2\"\n{}{}",
            clone("/nonexistent/clone-b"),
            clone("/nonexistent/clone-c")
        );
        let findings = orphan_note_for(
            "orphan-workspace",
            &[("mixed", Some(mixed)), ("all-gone", Some(all_gone))],
        );
        let text = note_text(&findings);
        assert!(text.contains(".spoolway/all-gone"), "{text}");
        assert!(!text.contains("mixed"), "{text}");
    }

    /// A folder with no `project.toml`, or one that will not parse, is not a
    /// home that can be called orphaned, and adds no row at all.
    #[test]
    fn orphaned_home_note_skips_folders_it_cannot_read() {
        let findings = orphan_note_for(
            "orphan-skipped",
            &[("logs", None), ("garbled", Some("not [ toml".into()))],
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// No layer at all is nothing to say: `override_layer_note` must not add
    /// a row to a project that has never touched `spoolway pipeline
    /// override` or its siblings.
    #[test]
    fn override_layer_note_is_silent_with_no_layer() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("doctor-override-note-empty");
        assert!(override_layer_note(&repo).is_empty());
    }

    /// A layer present is a standing note naming the overridden artifact —
    /// present whether or not `dispatch`'s own gate has ever been
    /// acknowledged, since this function never reads the acknowledgement at
    /// all.
    #[test]
    fn override_layer_note_names_an_active_layer() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("doctor-override-note-active");
        let overrides = repo.overrides_dir();
        std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
        std::fs::write(
            overrides.join("pipelines/default.yml"),
            "steps:\n  implement:\n    model: fake-opus\n",
        )
        .unwrap();

        let findings = override_layer_note(&repo);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let Finding::Note(text) = &findings[0] else {
            panic!("expected a note: {findings:?}");
        };
        assert!(text.contains("overrides are active"), "{text}");
        assert!(text.contains("pipelines/default.yml"), "{text}");
    }

    /// A stale step entry earns its own note beside the standing "overrides
    /// are active" one — the same reason `override list` prints, since both
    /// read `collect_override_rows`.
    #[test]
    fn override_layer_note_names_a_stale_entry_with_its_reason() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("doctor-override-note-stale");
        std::fs::create_dir_all(repo.checkout.join(".spoolway/pipelines")).unwrap();
        std::fs::write(
            repo.checkout.join(".spoolway/pipelines/default.yml"),
            "steps:\n  - id: implement\n    agent: pi\n    on_pass: done\n",
        )
        .unwrap();
        let overrides = repo.overrides_dir();
        std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
        std::fs::write(
            overrides.join("pipelines/default.yml"),
            "steps:\n  implement:\n    run: echo hi\n",
        )
        .unwrap();

        let findings = override_layer_note(&repo);
        let texts: Vec<String> = findings
            .iter()
            .map(|f| match f {
                Finding::Note(text) => text.clone(),
                other => panic!("expected a note: {other:?}"),
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("overrides are active")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("override ignored")
                && t.contains("implement")
                && t.contains("names both `run:` and `agent:`")),
            "{texts:?}"
        );
    }

    /// `doctor`'s model messages name the real absolute path of the one
    /// pipeline file a missing model actually lives in — `.spoolway/pipelines/
    /// <name>.yml` under this checkout, not the retired single `pipeline.yml`
    /// that `Pipelines::load` now refuses (finding 25), and not a bare Rust
    /// debug list of step ids with no file at all.
    #[test]
    fn the_model_check_points_at_the_pipelines_directory() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("doctor-model-check-points");
        let mut pipelines = crate::pipeline::Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline
                .steps
                .iter_mut()
                .find(|step| step.id == crate::pipeline::BLOCKED)
                .unwrap()
                .model = None;
        }
        let mut config = Config::default();
        config.unattended.enabled = true;
        config.unattended.blocked_model.clear();
        let findings = agent_checks(&repo, &pipelines, &config);

        let notes: Vec<String> = findings
            .iter()
            .filter_map(|f| match f {
                Finding::Check(label, Err(err)) if label.ends_with("has a model") => {
                    Some(format!("{err:#}"))
                }
                _ => None,
            })
            .collect();

        assert!(!notes.is_empty(), "a model check was produced");
        for note in notes {
            assert!(
                note.contains(
                    &repo
                        .checkout
                        .join(".spoolway/pipelines")
                        .display()
                        .to_string()
                ),
                "{note}"
            );
            assert!(note.ends_with(".yml"), "{note}");
            assert!(!note.contains(" in pipeline.yml"), "{note}");
            assert!(!note.contains('['), "{note} reads like a debug list");
        }
    }

    #[test]
    fn an_attended_synthetic_blocked_step_needs_no_model() {
        let (repo, _root_guard) =
            crate::commands::testutil::fixture("doctor-attended-blocked-no-model");
        let mut pipelines = crate::pipeline::Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline
                .steps
                .iter_mut()
                .find(|step| step.id == crate::pipeline::BLOCKED)
                .unwrap()
                .model = None;
        }
        let mut config = Config::default();
        config.unattended.blocked_model.clear();

        let attended = agent_checks(&repo, &pipelines, &config);
        assert!(attended.iter().all(|finding| match finding {
            Finding::Check(label, result) if label.ends_with("has a model") => result.is_ok(),
            _ => true,
        }));

        config.unattended.enabled = true;
        let unattended = agent_checks(&repo, &pipelines, &config);
        assert!(unattended.iter().any(|finding| match finding {
            Finding::Check(label, result) if label.ends_with("has a model") => result.is_err(),
            _ => false,
        }));
    }

    /// The failing "has a model" row names the step that is missing one,
    /// rather than the generic "some step" it used to say with nothing
    /// after the colon — see the `home-mode-messages` task.
    #[test]
    fn the_has_a_model_failure_names_the_missing_step() {
        let (repo, _root_guard) =
            crate::commands::testutil::fixture("doctor-model-check-names-step");
        let mut pipelines = crate::pipeline::Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline
                .steps
                .iter_mut()
                .find(|step| step.id == crate::pipeline::BLOCKED)
                .unwrap()
                .model = None;
        }
        let mut config = Config::default();
        config.unattended.enabled = true;
        config.unattended.blocked_model.clear();

        let findings = agent_checks(&repo, &pipelines, &config);
        let failure = findings
            .iter()
            .find_map(|finding| match finding {
                Finding::Check(label, Err(err)) if label.ends_with("has a model") => {
                    Some(format!("{err:#}"))
                }
                _ => None,
            })
            .expect("the model check fails");
        assert!(failure.contains(crate::pipeline::BLOCKED), "{failure}");
        assert!(!failure.contains("some step running on"), "{failure}");
    }

    /// A command step names no model on purpose, so the shipped `handover`
    /// (a `run:` step in `default`) must never turn up in a `has a model`
    /// failure — this is what moved here once the check started reading
    /// each pipeline's own steps directly instead of a step id shared
    /// across pipelines.
    // covers: step.run — a command step runs no agent, so it carries no model to be missing
    #[test]
    fn a_command_step_is_never_reported_as_missing_a_model() {
        let (repo, _root_guard) =
            crate::commands::testutil::fixture("doctor-command-step-no-model");
        let pipelines = crate::pipeline::Pipelines::builtin();

        let command_steps: Vec<&str> = pipelines
            .pipelines
            .values()
            .flat_map(|pipeline| &pipeline.steps)
            .filter(|step| step.run.is_some())
            .map(|step| step.id.as_str())
            .collect();
        assert!(
            command_steps.contains(&"handover"),
            "the shipped set should still have a command `handover`: {command_steps:?}"
        );

        let config = Config::default();
        let findings = agent_checks(&repo, &pipelines, &config);
        assert!(findings.iter().all(|finding| match finding {
            Finding::Check(label, Err(err)) if label.ends_with("has a model") => {
                let msg = format!("{err:#}");
                !msg.contains("handover")
            }
            _ => true,
        }));
    }

    /// A job table with a misspelt key fails its own `has no unknown keys`
    /// check, naming the job and the key; a clean job's check passes.
    #[test]
    fn a_job_with_an_unknown_key_fails_its_doctor_check() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("doctor-job-unknown-key");
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::write(
            repo.user_jobs_file(),
            "[jobs.typo]\nschedule = \"@daily\"\npipeline = \"default\"\nroutine = \"r\"\nenable = false\n\
             [jobs.clean]\nschedule = \"@daily\"\npipeline = \"default\"\nroutine = \"r\"\n",
        )
        .unwrap();

        let findings = jobs_checks(&repo, &crate::pipeline::Pipelines::builtin());
        let check = |label: &str| {
            findings.iter().find_map(|f| match f {
                Finding::Check(l, result) if l == label => Some(result),
                _ => None,
            })
        };
        let failure = check("job `typo` has no unknown keys")
            .expect("the check exists")
            .as_ref()
            .expect_err("a misspelt key must fail the check");
        assert!(format!("{failure:#}").contains("enable"), "{failure:#}");
        assert!(
            check("job `clean` has no unknown keys")
                .expect("the check exists")
                .is_ok()
        );
    }

    /// A step whose profile blocks at or below its model's `compact_ctx` is
    /// a `FAIL` row naming its pipeline and step, and the same config with
    /// compaction below block draws none. Fails before the check exists.
    // covers: models.<glob>.compact_ctx — doctor refuses a step blocked before it compacts
    #[test]
    fn a_step_blocked_before_it_compacts_fails_doctor() {
        let pipelines = crate::pipeline::Pipelines::builtin();
        let refused = |compact_ctx| -> (usize, Vec<String>) {
            let mut config = Config::default();
            config.models.insert(
                "*".into(),
                crate::usage::ModelPrice {
                    compact_ctx,
                    context_window: 1000,
                    ..Default::default()
                },
            );
            for profile in config.agents.values_mut() {
                profile.kind = "claude".into();
                profile.session_blocked_ctx = 40;
            }
            let steps: Vec<Finding> = model_health_checks(&pipelines, &config)
                .into_iter()
                .filter(
                    |f| matches!(f, Finding::Check(label, _) if label.starts_with("pipeline `")),
                )
                .collect();
            let failed = steps
                .iter()
                .filter_map(|f| match f {
                    Finding::Check(label, Err(why)) => Some(format!("{label}: {why}")),
                    _ => None,
                })
                .collect();
            (steps.len(), failed)
        };

        // Every step is one check, passing or not, so the total is the same.
        let (total, rows) = refused(50);
        assert_eq!(total, refused(30).0);
        assert!(!rows.is_empty(), "no step was refused");
        assert!(
            rows.iter()
                .all(|row| row.contains("session_blocked_ctx = 40")
                    && row.contains("compact_ctx = 50")),
            "{rows:#?}"
        );
        assert_eq!(refused(30).1, Vec::<String>::new());
    }

    /// One note per `[models]` entry that sets `slots` without saying whether
    /// it is `local`. A model that has set `local`, and one that sets no
    /// `slots` — including one that still carries the retired `exclusive` —
    /// draw nothing.
    #[test]
    fn a_sized_model_that_has_not_set_local_is_noted() {
        let pipelines = crate::pipeline::Pipelines::builtin();
        let mut config: Config = toml::from_str("[models.\"*Muse-*\"]\nexclusive = true\n")
            .expect("a retired exclusive must still load");
        config.models.insert(
            "*Qwen3.6-35B-A3B".into(),
            crate::usage::ModelPrice {
                slots: 3,
                ..Default::default()
            },
        );
        config.models.insert(
            "*Ornith-1.5-35B-A3B".into(),
            crate::usage::ModelPrice {
                slots: 3,
                local: true,
                ..Default::default()
            },
        );
        config.models.insert(
            "priced-cloud-*".into(),
            crate::usage::ModelPrice {
                input: 5.0,
                ..Default::default()
            },
        );

        let local_notes: Vec<String> = model_health_checks(&pipelines, &config)
            .iter()
            .filter_map(|f| match f {
                Finding::Note(text) if text.contains("but not `local`") => Some(text.clone()),
                _ => None,
            })
            .collect();

        // One for Qwen; nothing for the `local` Ornith entry, the priced
        // cloud one or the Muse glob that carries only the retired flag.
        assert_eq!(local_notes.len(), 1, "{local_notes:#?}");
        assert!(
            local_notes[0].contains("model `*Qwen3.6-35B-A3B` sets `slots` but not `local`"),
            "{}",
            local_notes[0]
        );
    }

    /// `spoolway doctor` names a `[models]` row nothing routes to, and stays
    /// silent for one a step does reach — the check that would have caught
    /// `[models."*Qwen3.6-35B-A3B"]` sitting dead after `49a4221` renamed the
    /// step that used to reach it.
    #[test]
    fn an_unrouted_model_row_is_noted_and_a_routed_one_is_silent() {
        let pipelines = single_step_pipelines(
            "  - id: build\n    agent: pi\n    model: ornith/Ornith-1.5-35B-A3B\n    \
             on_pass: finish\n  - id: finish\n    run: x\n    on_pass: done\n",
        );
        let mut config = Config::default();
        config.models.insert(
            "Ornith-1.5-35B-A3B".into(),
            crate::usage::ModelPrice::default(),
        );
        config.models.insert(
            "Qwen3.6-35B-A3B".into(),
            crate::usage::ModelPrice::default(),
        );

        let notes: Vec<String> = unrouted_model_notes(&pipelines, &config)
            .iter()
            .filter_map(|f| match f {
                Finding::Note(text) if text.contains("routes to no step") => Some(text.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(notes.len(), 1, "{notes:#?}");
        assert!(notes[0].contains("model `Qwen3.6-35B-A3B`"), "{}", notes[0]);
        assert!(!notes[0].contains("Ornith"), "{}", notes[0]);
    }

    /// A prompt directory no step runs is noted — the `summariser/` a 0.1.0
    /// install keeps after 0.2.0 stopped shipping it — and one a step does run
    /// is not.
    #[test]
    fn a_prompt_no_step_runs_is_noted_and_a_used_one_is_silent() {
        let (repo, _root_guard) = scratch_repo("orphan-prompt");
        for name in ["worker", "summariser"] {
            let file = crate::prompt::directory_form(&repo, name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "do the work\n").unwrap();
        }
        let pipelines = single_step_pipelines(
            "  - id: build\n    agent: pi\n    prompt: worker\n    model: m\n    \
             on_pass: finish\n  - id: finish\n    run: x\n    on_pass: done\n",
        );

        let notes: Vec<String> = prompt_checks(&repo, &pipelines)
            .iter()
            .filter_map(|f| match f {
                Finding::Note(text) if text.contains("run by no step") => Some(text.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(notes.len(), 1, "{notes:#?}");
        assert!(notes[0].contains("prompt `summariser`"), "{}", notes[0]);
        assert!(notes[0].contains("summariser"), "{}", notes[0]);
        assert!(!notes[0].contains("`worker`"), "{}", notes[0]);
    }

    #[test]
    fn names_key_reads_whichever_directory_it_is_given() {
        let (root, checkout, _base_guard) = two_configs(
            "names-key",
            "[dispatch]\nmax_launches = 3\n",
            "[dispatch]\nworktree_root = \"x\"\n",
        );
        assert!(
            names_key(&root, "max_launches"),
            "the root's own file names the key"
        );
        assert!(
            !names_key(&checkout, "max_launches"),
            "the checkout's file does not — reading the wrong directory here is exactly the \
             bug this task fixes"
        );
    }

    /// No queued task has a worktree under the old path — the note still
    /// names the path, but, since nothing of a project's own depends on it
    /// any more, it says so rather than naming any task.
    #[test]
    fn worktree_root_note_names_a_real_path_with_no_tasks_there() {
        let root = crate::scratch::root("doctor-worktree-root-note-path");
        std::fs::create_dir_all(root.join(crate::config::STATE_DIR)).unwrap();
        std::fs::write(
            Config::path_in(&root),
            "[dispatch]\nworktree_root = \"/old/worktrees\"\n",
        )
        .unwrap();

        let notes = worktree_root_note(&root, &[]);
        assert_eq!(notes.len(), 1, "{notes:#?}");
        let Finding::Note(text) = &notes[0] else {
            panic!("{notes:#?}")
        };
        assert!(text.contains("/old/worktrees"), "{text}");
        assert!(text.contains("retired"), "{text}");
        assert!(
            !text.contains("safe to remove") && !text.contains("yours to remove"),
            "a folder must never be called safe to remove outright: {text}"
        );
    }

    /// A queued task still has a worktree cut under the old path — the note
    /// must name that task rather than call the old directory anybody's to
    /// remove.
    #[test]
    fn worktree_root_note_names_a_queued_task_still_using_the_old_path() {
        let root = crate::scratch::root("doctor-worktree-root-note-task");
        std::fs::create_dir_all(root.join(crate::config::STATE_DIR)).unwrap();
        std::fs::write(
            Config::path_in(&root),
            "[dispatch]\nworktree_root = \"/old/worktrees\"\n",
        )
        .unwrap();
        let still_queued = Task::parse(
            PathBuf::from("still-queued.md"),
            "---\nid: still-queued\nstage: queued\nworktree_path: /old/worktrees/still-queued\n\
             ---\n",
        )
        .unwrap();
        let elsewhere = Task::parse(
            PathBuf::from("elsewhere.md"),
            "---\nid: elsewhere\nstage: queued\nworktree_path: /new/worktrees/elsewhere\n---\n",
        )
        .unwrap();

        let notes = worktree_root_note(&root, &[still_queued, elsewhere]);
        assert_eq!(notes.len(), 1, "{notes:#?}");
        let Finding::Note(text) = &notes[0] else {
            panic!("{notes:#?}")
        };
        assert!(text.contains("still-queued"), "{text}");
        assert!(!text.contains("elsewhere"), "{text}");
        assert!(
            !text.contains("safe to remove") && !text.contains("yours to remove"),
            "a folder with a task still in it must never be called removable: {text}"
        );
    }

    /// A `worktree_root` written with a leading `~/` names the same folder as
    /// the absolute path a task records, so the note must still name it.
    #[test]
    fn worktree_root_note_expands_a_leading_tilde_before_matching() {
        let Some(home) = crate::platform::home_dir() else {
            return;
        };
        let root = crate::scratch::root("doctor-worktree-root-note-tilde");
        std::fs::create_dir_all(root.join(crate::config::STATE_DIR)).unwrap();
        std::fs::write(
            Config::path_in(&root),
            "[dispatch]\nworktree_root = \"~/p25-wt\"\n",
        )
        .unwrap();
        let z1 = Task::parse(
            PathBuf::from("z1.md"),
            &format!(
                "---\nid: z1\nstage: paused\nworktree_path: {}\n---\n",
                home.join("p25-wt").join("task-z1").display()
            ),
        )
        .unwrap();

        let notes = worktree_root_note(&root, &[z1]);
        let Finding::Note(text) = &notes[0] else {
            panic!("{notes:#?}")
        };
        assert!(text.contains("z1"), "{text}");
        assert!(!text.contains("no queued task"), "{text}");
    }

    /// A blank value was never anybody's decision — nothing to clean up, so
    /// nothing to say until the ordinary dropped-key report on `sync` covers
    /// it.
    #[test]
    fn worktree_root_note_is_quiet_on_a_blank_value() {
        let root = crate::scratch::root("doctor-worktree-root-note-blank");
        std::fs::create_dir_all(root.join(crate::config::STATE_DIR)).unwrap();
        std::fs::write(Config::path_in(&root), "[dispatch]\nworktree_root = \"\"\n").unwrap();

        assert!(worktree_root_note(&root, &[]).is_empty());
    }

    /// No key at all — the ordinary case for a project that never set it —
    /// is just as quiet.
    #[test]
    fn worktree_root_note_is_quiet_when_absent() {
        let root = crate::scratch::root("doctor-worktree-root-note-absent");
        std::fs::create_dir_all(root.join(crate::config::STATE_DIR)).unwrap();
        std::fs::write(Config::path_in(&root), "[dispatch]\n").unwrap();

        assert!(worktree_root_note(&root, &[]).is_empty());
    }

    fn single_step_pipelines(step_yaml: &str) -> Pipelines {
        let raw = format!("steps:\n{step_yaml}");
        let pipeline: Pipeline = serde_norway::from_str(&raw).unwrap();
        pipeline.validate().unwrap();
        let mut pipelines = std::collections::BTreeMap::new();
        pipelines.insert("default".to_string(), pipeline);
        Pipelines {
            pipelines,
            ignored_overrides: Vec::new(),
        }
    }

    fn tracking(hook: &str) -> crate::config::IssueTrackingConfig {
        crate::config::IssueTrackingConfig {
            hook: hook.to_string(),
            ..Default::default()
        }
    }

    /// A `Repo` whose checkout is a scratch directory, real enough for
    /// `issue_tracking_checks` to stat a hook path against — nothing else
    /// reads `config` or `home` here, so both are defaults.
    fn scratch_repo(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let checkout = crate::scratch::root(&format!("doctor-{name}"));
        let _ = std::fs::remove_dir_all(&checkout);
        std::fs::create_dir_all(&checkout).unwrap();
        (
            Repo {
                borrowed: false,
                root: checkout.to_path_buf(),
                home: checkout.join(".home"),
                checkout: checkout.to_path_buf(),
                config: Config::default(),
            },
            checkout,
        )
    }

    #[test]
    fn failures_after_set_runs_only_the_checks_the_key_can_affect() {
        let (repo, _root) = scratch_repo("failures-after-set");
        let mut config = Config::default();
        config.issue_tracking.hook = "ghost.sh".into();
        config.unattended.enabled = true;
        config.unattended.blocked_model = String::new();

        let tracking = failures_after_set(&repo, &config, "issue_tracking.hook");
        assert!(
            tracking.iter().any(|f| f.contains("script exists")),
            "{tracking:?}"
        );
        assert!(
            !tracking.iter().any(|f| f.contains("staffs")),
            "{tracking:?}"
        );

        let unattended = failures_after_set(&repo, &config, "unattended.blocked_model");
        assert!(
            unattended.iter().any(|f| f.contains("staffs `blocked`")),
            "{unattended:?}"
        );
        assert!(failures_after_set(&repo, &config, "housekeeping.retention_days").is_empty());
    }

    /// A hook named with nothing to hand it: `project_key` blank while
    /// `hook` names something. The one half-set shape a person can reach by
    /// hand, and the only one of the four that fires on `project_key` rather
    /// than on `hook` itself.
    #[test]
    fn issue_tracking_checks_refuses_a_hook_with_a_blank_project_key() {
        let (repo, _root_guard) = scratch_repo("blank-project-key");
        let findings = issue_tracking_checks(&repo, &tracking("record.sh"));
        let Finding::Check(label, outcome) = &findings[0] else {
            panic!("{:?}", findings[0])
        };
        assert_eq!(label, "`[issue_tracking]` is fully configured or fully off");
        let err = outcome.as_ref().unwrap_err();
        assert!(err.to_string().contains("record.sh"), "{err}");
        assert!(err.to_string().contains("project_key"), "{err}");
    }

    /// A hook holding a path rather than a bare filename — refused before it
    /// is ever joined onto `.spoolway/hooks/`, per
    /// `crate::tracking::is_bare_filename`.
    #[test]
    fn issue_tracking_checks_refuses_a_hook_that_is_not_a_bare_filename() {
        let (repo, _root_guard) = scratch_repo("not-bare");
        let findings = issue_tracking_checks(&repo, &tracking("../record.sh"));
        let Finding::Check(label, outcome) = &findings[1] else {
            panic!("{:?}", findings[1])
        };
        assert_eq!(label, "`issue_tracking.hook` is a bare filename");
        let err = outcome.as_ref().unwrap_err();
        assert!(err.to_string().contains("../record.sh"), "{err}");
        assert!(err.to_string().contains("not a bare filename"), "{err}");
    }

    /// A bare, well-formed name whose file was never written — the gap
    /// `init` closes for the two shipped hooks but not for a hand-edited
    /// config naming a script that was renamed or never copied in.
    #[test]
    fn issue_tracking_checks_refuses_a_hook_naming_a_script_that_does_not_exist() {
        let (repo, _root_guard) = scratch_repo("missing-script");
        let findings = issue_tracking_checks(&repo, &tracking("ghost.sh"));
        let Finding::Check(label, outcome) = &findings[2] else {
            panic!("{:?}", findings[2])
        };
        assert_eq!(label, "`issue_tracking.hook` script exists");
        let err = outcome.as_ref().unwrap_err();
        assert!(err.to_string().contains("ghost.sh"), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    /// `jira.sh` is the one shipped hook that shells out to binaries this
    /// project does not otherwise need — checked only when the hook actually
    /// names it, unlike the three checks above, which run for any hook.
    #[test]
    fn issue_tracking_checks_requires_acli_and_jq_for_the_jira_hook() {
        let (repo, _root_guard) = scratch_repo("jira-hook");
        let hook = crate::cli::Tracker::Jira.hook_name();
        let hooks_dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(hooks_dir.join(&hook), "#!/bin/sh\n").unwrap();
        let findings = issue_tracking_checks(&repo, &tracking(&hook));

        let labels: Vec<&str> = findings
            .iter()
            .map(|f| match f {
                Finding::Check(label, _) => label.as_str(),
                _ => "",
            })
            .collect();
        assert!(labels.contains(&"`acli` is on PATH"), "{labels:?}");
        assert!(labels.contains(&"`jq` is on PATH"), "{labels:?}");

        // Whether `acli` and `jq` are installed depends on the machine, so each
        // row is held to what PATH actually holds here: the path it resolves
        // to when found, and a failure naming `jira.sh` when not.
        for binary in ["acli", "jq"] {
            let label = format!("`{binary}` is on PATH");
            let outcome = findings
                .iter()
                .find_map(|f| match f {
                    Finding::Check(l, outcome) if *l == label => Some(outcome),
                    _ => None,
                })
                .unwrap();
            match which(binary) {
                Some(path) => assert_eq!(outcome.as_ref().unwrap(), &Some(path)),
                None => {
                    let err = outcome.as_ref().unwrap_err();
                    assert!(err.to_string().contains("jira.sh"), "{err}");
                }
            }
        }
    }

    /// `parse_version` finds the first run of digits and dots in whatever a
    /// tool's own `--version` prints, tolerant of a name, a `v` prefix and a
    /// build date around it — every shape a shipped hook's own tool answers
    /// with.
    #[test]
    fn parse_version_reads_the_first_dotted_number() {
        assert_eq!(
            parse_version("gh version 2.97.0 (2024-06-03)"),
            Some(vec![2, 97, 0])
        );
        assert_eq!(parse_version("jq-1.6"), Some(vec![1, 6]));
        assert_eq!(
            parse_version("acli version 1.3.30-stable"),
            Some(vec![1, 3, 30])
        );
        assert_eq!(parse_version("no digits here"), None);
    }

    /// A found version below the declared floor fails, naming the tool, the
    /// version found and the hook that requires it; a found version at or
    /// above the floor passes.
    #[test]
    fn tool_version_finding_fails_below_the_floor_and_passes_at_it() {
        let required = crate::tracking::RequiredTool {
            tool: "gh".into(),
            floor: "2.97.0".into(),
        };
        let finding = tool_version_finding(
            &required,
            ".spoolway/hooks/github.sh",
            Some("gh version 2.46.0 (2024-01-01)"),
        )
        .unwrap();
        let Finding::Check(label, outcome) = &finding else {
            panic!("{finding:?}")
        };
        assert_eq!(label, "`gh` is at least 2.97.0");
        let err = outcome.as_ref().unwrap_err().to_string();
        assert!(err.contains("gh` is 2.46.0"), "{err}");
        assert!(err.contains(".spoolway/hooks/github.sh"), "{err}");
        assert!(err.contains("gh >= 2.97.0"), "{err}");

        let passing = tool_version_finding(
            &required,
            ".spoolway/hooks/github.sh",
            Some("gh version 2.97.0 (2024-06-03)"),
        )
        .unwrap();
        let Finding::Check(_, outcome) = &passing else {
            panic!("{passing:?}")
        };
        assert!(outcome.is_ok(), "{outcome:?}");
    }

    /// A found version with fewer components than the floor — `jq-1.6`
    /// against a floor of `1.6.0` — is not below it: comparing the two
    /// `Vec<u64>` lexicographically would read a missing trailing `.0` as
    /// smaller, which is exactly the false failure the "unreadable version
    /// is a note, never a failure" criterion exists to rule out (a short
    /// version is readable, just short).
    #[test]
    fn tool_version_finding_treats_a_missing_trailing_zero_as_equal() {
        let required = crate::tracking::RequiredTool {
            tool: "jq".into(),
            floor: "1.6.0".into(),
        };
        let finding =
            tool_version_finding(&required, ".spoolway/hooks/jira.sh", Some("jq-1.6")).unwrap();
        let Finding::Check(_, outcome) = &finding else {
            panic!("{finding:?}")
        };
        assert!(outcome.is_ok(), "{outcome:?}");
    }

    /// A declared floor that does not parse as `<number>.<number>...` — a
    /// hook writing `# spoolway-requires: gh >= latest` — is a note, never a
    /// failure: the non-goal ruling out a general constraint grammar means
    /// this is reported as unreadable, not force-fit to some interpretation
    /// of "latest".
    #[test]
    fn tool_version_finding_notes_an_unreadable_floor_rather_than_failing() {
        let required = crate::tracking::RequiredTool {
            tool: "gh".into(),
            floor: "latest".into(),
        };
        let finding = tool_version_finding(
            &required,
            ".spoolway/hooks/github.sh",
            Some("gh version 2.97.0 (2024-06-03)"),
        )
        .unwrap();
        let Finding::Note(text) = &finding else {
            panic!("{finding:?}")
        };
        assert!(text.contains("gh >= latest"), "{text}");
        assert!(text.contains(".spoolway/hooks/github.sh"), "{text}");
    }

    /// A tool's `--version` answering something that does not read as a
    /// version is a note, never a failure — an unparseable answer must not
    /// fail a project that is actually fine.
    #[test]
    fn tool_version_finding_notes_an_unreadable_answer_rather_than_failing() {
        let required = crate::tracking::RequiredTool {
            tool: "gh".into(),
            floor: "2.97.0".into(),
        };
        let finding =
            tool_version_finding(&required, ".spoolway/hooks/github.sh", Some("???")).unwrap();
        assert!(matches!(finding, Finding::Note(_)), "{finding:?}");
    }

    /// No answer at all — the tool was not on PATH, or its `--version` could
    /// not even be run — produces no finding here: an existing check already
    /// owns the not-on-PATH failure, and doubling it up would report the same
    /// gap twice under two different names.
    #[test]
    fn tool_version_finding_is_silent_with_no_answer() {
        let required = crate::tracking::RequiredTool {
            tool: "gh".into(),
            floor: "2.97.0".into(),
        };
        assert!(tool_version_finding(&required, ".spoolway/hooks/github.sh", None).is_none());
    }

    /// The whole path through `issue_tracking_checks`, not just
    /// `tool_version_finding` on its own: a hook declaring
    /// `# spoolway-requires: cargo >= <version>` is checked against this
    /// machine's real `cargo` — the one binary this project's own build
    /// guarantees is on PATH wherever its tests run — failing when the floor
    /// is set absurdly high and passing when it plainly is not.
    #[test]
    fn issue_tracking_checks_enforces_a_declared_tool_version() {
        let (repo, _root_guard) = scratch_repo("requires-version");
        let hooks_dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("versioned.sh"),
            "#!/bin/sh\n# spoolway-requires: cargo >= 999.0.0\nexit 0\n",
        )
        .unwrap();
        let findings = issue_tracking_checks(&repo, &tracking("versioned.sh"));
        let outcome = findings
            .iter()
            .find_map(|f| match f {
                Finding::Check(label, outcome) if label == "`cargo` is at least 999.0.0" => {
                    Some(outcome)
                }
                _ => None,
            })
            .expect("no row for the declared cargo version");
        let err = outcome.as_ref().unwrap_err().to_string();
        assert!(err.contains("999.0.0"), "{err}");
        assert!(err.contains("versioned.sh"), "{err}");

        std::fs::write(
            hooks_dir.join("versioned.sh"),
            "#!/bin/sh\n# spoolway-requires: cargo >= 0.0.1\nexit 0\n",
        )
        .unwrap();
        let findings = issue_tracking_checks(&repo, &tracking("versioned.sh"));
        let outcome = findings
            .iter()
            .find_map(|f| match f {
                Finding::Check(label, outcome) if label == "`cargo` is at least 0.0.1" => {
                    Some(outcome)
                }
                _ => None,
            })
            .expect("no row for the declared cargo version");
        assert!(outcome.is_ok(), "{outcome:?}");
    }

    /// A `# spoolway-requires:` line that does not parse as `<tool> >=
    /// <version>` — the third non-goal, ruling out a general constraint
    /// grammar — is reported as its own note naming the raw line, rather than
    /// dropped or force-interpreted.
    #[test]
    fn required_tool_checks_notes_an_unreadable_declaration_line() {
        let checkout = crate::scratch::root("doctor-unreadable-requires-line");
        let _ = std::fs::remove_dir_all(&checkout);
        let hooks_dir = checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("odd.sh"),
            "#!/bin/sh\n# spoolway-requires: gh ~= 2.97.0\nexit 0\n",
        )
        .unwrap();
        let findings = required_tool_checks(&checkout, "odd.sh");
        let note = findings
            .iter()
            .find_map(|f| match f {
                Finding::Note(text) => Some(text),
                _ => None,
            })
            .expect("no note for the unreadable line");
        assert!(note.contains("odd.sh"), "{note}");
        assert!(note.contains("gh ~= 2.97.0"), "{note}");
    }

    /// A declaration naming a tool that is not on PATH at all produces no row
    /// of its own — the not-on-PATH failure belongs to whatever check already
    /// covers that binary (`acli`/`jq` for `jira.sh`, `gh` for `spoolway
    /// stack`), and this must not report the same gap a second time under a
    /// different name.
    #[test]
    fn issue_tracking_checks_defers_to_the_existing_not_on_path_failure() {
        let (repo, _root_guard) = scratch_repo("requires-missing-tool");
        let hooks_dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("versioned.sh"),
            "#!/bin/sh\n# spoolway-requires: definitely-not-a-real-binary >= 1.0.0\nexit 0\n",
        )
        .unwrap();
        let findings = issue_tracking_checks(&repo, &tracking("versioned.sh"));
        assert!(
            findings.iter().all(|f| !matches!(
                f,
                Finding::Check(label, _) if label.contains("definitely-not-a-real-binary")
            )),
            "{findings:?}"
        );
    }

    /// A hook with no `# spoolway-requires:` line at all produces exactly the
    /// rows it produces today — no new row appears just because the check
    /// now exists.
    #[test]
    fn issue_tracking_checks_is_unchanged_with_no_requires_line() {
        let (repo, _root_guard) = scratch_repo("no-requires");
        let hooks_dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(hooks_dir.join("plain.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        let findings = issue_tracking_checks(&repo, &tracking("plain.sh"));
        assert!(
            findings.iter().all(|f| !matches!(
                f,
                Finding::Check(label, _) if label.contains("is at least")
            )),
            "{findings:?}"
        );
    }

    /// `key_in_names` on, but the configured hook script never writes
    /// `slug=`: a standing gap `doctor` names, and one it stays quiet about
    /// both when the script does write the line and when the flag is off.
    #[test]
    fn issue_tracking_checks_reports_key_in_names_without_a_slug_line() {
        let (repo, _root_guard) = scratch_repo("slug-gap");
        let hooks_dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("jira.sh"),
            "#!/bin/sh\n{ echo \"epic=$SPOOLWAY_EPIC\"; echo \"ticket=x\"; } >\"$SPOOLWAY_OUT\"\n",
        )
        .unwrap();

        let on = crate::config::IssueTrackingConfig {
            hook: "jira.sh".into(),
            project_key: "PROJ".into(),
            key_in_names: true,
        };
        let findings = issue_tracking_checks(&repo, &on);
        let gap = findings.iter().find_map(|f| match f {
            Finding::Check(label, outcome)
                if label == "the configured hook writes a `slug=` line" =>
            {
                Some(outcome)
            }
            _ => None,
        });
        let err = gap
            .expect("the slug check is missing")
            .as_ref()
            .unwrap_err();
        assert!(err.to_string().contains("key_in_names is on"), "{err}");
        assert!(err.to_string().contains("jira.sh"), "{err}");

        // Flag off: nothing to report.
        let off = crate::config::IssueTrackingConfig {
            key_in_names: false,
            ..on.clone()
        };
        let quiet = issue_tracking_checks(&repo, &off)
            .into_iter()
            .find_map(|f| match f {
                Finding::Check(label, outcome)
                    if label == "the configured hook writes a `slug=` line" =>
                {
                    Some(outcome)
                }
                _ => None,
            });
        assert!(quiet.unwrap().is_ok());
    }

    /// Neither of `gh`'s two callers is present: no step runs `spoolway
    /// stack`, and issue tracking names no hook at all.
    #[test]
    fn reaches_forge_is_false_with_neither_caller() {
        let pipelines = single_step_pipelines("  - id: a\n    run: x\n    on_pass: done\n");
        assert!(!reaches_forge(&pipelines, &tracking("")));
    }

    /// A step whose `run:` calls `spoolway stack` is one of the two callers,
    /// on its own.
    #[test]
    fn reaches_forge_is_true_with_a_stack_step() {
        let pipelines = single_step_pipelines(
            "  - id: a\n    run: spoolway stack\n    on_pass: b\n  - id: b\n    run: x\n    on_pass: done\n",
        );
        assert!(reaches_forge(&pipelines, &tracking("")));
    }

    /// Issue tracking naming the `github` hook is the other caller, even with
    /// no step reaching for `gh` itself.
    #[test]
    fn reaches_forge_is_true_with_the_github_hook() {
        let pipelines = single_step_pipelines("  - id: a\n    run: x\n    on_pass: done\n");
        assert!(reaches_forge(
            &pipelines,
            &tracking(&crate::cli::Tracker::Github.hook_name())
        ));
    }

    /// A report with one of everything: a passing check, a note both modes
    /// print, a note only the listing prints, and a failure.
    fn mixed() -> Report {
        let mut report = Report::default();
        report.check("config parses", Ok(Some("/p/config.toml".into())));
        report.check("lanes can be started", Ok(None));
        report.note("no git remote");
        report.check("agent `pi`", Err(anyhow::anyhow!("`pi` is not on PATH")));
        report.note_verbose("no dispatcher running");
        report
    }

    /// What the whole change is for: the short report is the exceptions, and
    /// nothing else. Every passing row goes into the closing count instead.
    #[test]
    fn the_short_report_keeps_only_the_exceptions() {
        let out = mixed().render(false);
        assert_eq!(
            out,
            "  note  no git remote\n  FAIL  agent `pi`: `pi` is not on PATH\n\n\
             2 of 3 checks passed.\n"
        );
    }

    /// `-v` brings the listing back, in the order the checks were declared and
    /// with the wording each one already had.
    #[test]
    fn verbose_lists_every_row_in_declaration_order() {
        let out = mixed().render(true);
        assert_eq!(
            out,
            "  ok    config parses — /p/config.toml\n\
             \x20 ok    lanes can be started\n\
             \x20 note  no git remote\n\
             \x20 FAIL  agent `pi`: `pi` is not on PATH\n\
             \x20 note  no dispatcher running\n\n\
             2 of 3 checks passed.\n"
        );
    }

    /// A project with nothing to say about it is one line, with no blank line
    /// above it: there is nothing for the blank line to separate.
    #[test]
    fn a_clean_project_is_one_line() {
        let mut report = Report::default();
        report.check("config parses", Ok(None));
        report.check("on a usable branch", Ok(Some("main".into())));
        assert_eq!(report.render(false), "2 checks passed.\n");
    }

    /// Notes are not checks. They print, and they are not counted either way.
    #[test]
    fn notes_are_not_counted_as_checks() {
        let mut report = Report::default();
        report.check("config parses", Ok(None));
        report.note("no git remote");
        assert_eq!(report.checks(), 1);
        assert_eq!(report.problems(), 0);
        assert_eq!(
            report.render(false),
            "  note  no git remote\n\n1 checks passed.\n"
        );
    }

    /// A failure replaces the reassurance with a tally, and is what the caller
    /// reads to decide the exit status.
    #[test]
    fn a_failure_gives_a_tally_and_a_problem_count() {
        let mut report = Report::default();
        report.check("config parses", Ok(None));
        report.check("agent `pi`", Err(anyhow::anyhow!("not on PATH")));
        report.check("agent `claude`", Err(anyhow::anyhow!("not on PATH")));
        assert_eq!(report.problems(), 2);
        assert!(report.render(false).ends_with("1 of 3 checks passed.\n"));
    }

    /// `render` prints whatever a note's own text holds, embedded newlines
    /// included, in both modes — nothing here assumes a note is one line,
    /// even though every note this project writes now is one.
    #[test]
    fn a_multi_line_note_keeps_its_continuation_lines() {
        let mut report = Report::default();
        report.note("first line\n          second line");
        assert_eq!(
            report.render(false),
            "  note  first line\n\
             \x20         second line\n\n\
             0 checks passed.\n"
        );
    }

    /// A backend built fresh for each call, headless: real, but with no
    /// panes to test — [`crate::headless::Headless::create_pane`] hands back
    /// a `tab_id` of `None`, which is exactly the shape `live_check` has to
    /// answer with a note rather than a check for.
    fn headless_mux(name: &str) -> (crate::headless::Headless, crate::scratch::ScratchRoot) {
        let root = crate::scratch::root(&format!("doctor-live-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mux = crate::headless::Headless::new(&root, &root, root.to_path_buf());
        (mux, root)
    }

    /// `--no-live` skips the row outright, without asking the backend
    /// anything — the row it produces is a note, not a check, so it never
    /// counts towards `problems()` either way.
    #[test]
    fn no_live_skips_the_row_without_touching_the_backend() {
        let (mux, _root_guard) = headless_mux("no-live");
        assert!(matches!(
            live_check(&mux, LiveCheckMode::Skip),
            Finding::NoteVerbose(text) if text.contains("--no-live")
        ));
    }

    /// A backend with no real pane — headless — reads as a note explaining
    /// why, never as a failed check: nothing was opened, so there is
    /// nothing to say passed or failed.
    ///
    /// Unix only, because `Headless` is the one backend with no real pane and
    /// it reports itself unavailable off Unix — on Windows `live_check` never
    /// reaches the branch under test, and answers with the unavailable note
    /// instead.
    #[test]
    fn a_backend_with_no_real_pane_is_a_note_not_a_check() {
        let (mux, _root_guard) = headless_mux("no-real-pane");
        assert!(matches!(
            live_check(&mux, LiveCheckMode::Default),
            Finding::NoteVerbose(text) if text.contains("no real pane")
        ));
    }

    /// Stands in for a herdr server the caller is not actually inside: it
    /// answers `agent list` fine (`is_available` is `true`, exactly as a
    /// server reachable from any shell does, with or without `HERDR_*` set),
    /// but `in_own_pane` is `false`, the signal `commands::dispatch`'s own
    /// pane gate already reads for the same fact. Every pane-opening call
    /// panics: if `live_check` ever reaches one of them for a caller outside
    /// herdr, that is the bug this stands in to catch, not something to
    /// quietly tolerate.
    struct OutsideHerdr;

    impl Mux for OutsideHerdr {
        fn name(&self) -> &'static str {
            "herdr"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable(&self) -> String {
            unimplemented!()
        }
        fn in_own_pane(&self) -> bool {
            false
        }
        fn resident_while_waiting(&self) -> bool {
            true
        }
        fn list_lanes(&self) -> Result<Vec<crate::mux::Lane>> {
            unimplemented!()
        }
        fn create_workspace(
            &self,
            _cwd: &std::path::Path,
            _branch: &str,
            _base: &str,
            _label: &str,
        ) -> Result<crate::mux::Workspace> {
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
        fn create_pane(
            &self,
            _cwd: &std::path::Path,
            _label: &str,
        ) -> Result<crate::mux::Workspace> {
            panic!("live_check opened a pane in a herdr server the caller is not inside");
        }
        fn split_pane(&self, _tab_id: &str, _cwd: &std::path::Path) -> Result<String> {
            unimplemented!()
        }
        fn close_pane(&self, _pane_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn start_lane(
            &self,
            _spec: &crate::mux::LaneSpec<'_>,
            _tick: &mut dyn FnMut(),
        ) -> Result<()> {
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

    /// A caller outside herdr altogether — no `HERDR_*` variables, just a
    /// server that happens to answer — gets no live pane opened in the
    /// person's real session. `live_check` should read `in_own_pane`, the
    /// exact signal `commands::dispatch`'s own pane gate already trusts for
    /// this, and skip with a note instead of calling through to
    /// `live_pane`.
    #[test]
    fn live_check_skips_a_herdr_server_the_caller_is_not_inside() {
        let mux = OutsideHerdr;
        assert!(matches!(
            live_check(&mux, LiveCheckMode::Default),
            Finding::NoteVerbose(text) if text.contains("not inside") || text.contains("herdr")
        ));
    }

    /// The same herdr-shaped backend as [`OutsideHerdr`], but reachable on
    /// purpose: `create_pane` records the directory it was handed in `seen`
    /// and answers with an error instead of panicking. That lets a test tell
    /// `--live` apart from the unforced path by what comes back — a `Check`
    /// naming `live_pane`'s own failure, not the skip note — and lets
    /// `live_pane_removes_its_scratch_directory_even_on_failure` find the
    /// directory without scanning the shared temporary one.
    struct FailsToCreatePane {
        /// What `Mux::in_own_pane` answers — `false` for a caller outside
        /// this herdr session, the case `--live` exists to override.
        in_own_pane: bool,
        seen: std::sync::Mutex<Option<std::path::PathBuf>>,
    }

    impl FailsToCreatePane {
        fn new(in_own_pane: bool) -> Self {
            Self {
                in_own_pane,
                seen: std::sync::Mutex::new(None),
            }
        }
    }

    impl Mux for FailsToCreatePane {
        fn name(&self) -> &'static str {
            "herdr"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable(&self) -> String {
            unimplemented!()
        }
        fn in_own_pane(&self) -> bool {
            self.in_own_pane
        }
        fn resident_while_waiting(&self) -> bool {
            true
        }
        fn list_lanes(&self) -> Result<Vec<crate::mux::Lane>> {
            unimplemented!()
        }
        fn create_workspace(
            &self,
            _cwd: &std::path::Path,
            _branch: &str,
            _base: &str,
            _label: &str,
        ) -> Result<crate::mux::Workspace> {
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
        fn create_pane(
            &self,
            cwd: &std::path::Path,
            _label: &str,
        ) -> Result<crate::mux::Workspace> {
            *self.seen.lock().unwrap() = Some(cwd.to_path_buf());
            Err(anyhow::anyhow!("deliberate failure"))
        }
        fn split_pane(&self, _tab_id: &str, _cwd: &std::path::Path) -> Result<String> {
            unimplemented!()
        }
        fn close_pane(&self, _pane_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn start_lane(
            &self,
            _spec: &crate::mux::LaneSpec<'_>,
            _tick: &mut dyn FnMut(),
        ) -> Result<()> {
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

    /// `--live` is the override: it reaches `live_pane` even for a caller
    /// `in_own_pane` says is outside this herdr session, rather than taking
    /// the skip `live_check_skips_a_herdr_server_the_caller_is_not_inside`
    /// proves for the unforced path.
    #[test]
    fn force_live_bypasses_the_in_own_pane_gate() {
        let mux = FailsToCreatePane::new(false);
        assert!(matches!(
            live_check(&mux, LiveCheckMode::Forced),
            Finding::Check(label, Err(_)) if label == "a lane really starts"
        ));
    }

    /// `live_pane`'s own scratch directory is gone once the check is over,
    /// whether or not it ever got as far as opening a pane — here,
    /// `create_pane` fails immediately, the earliest a real check can fail,
    /// and the directory `live_pane` made before calling it must still be
    /// cleaned up. [`FailsToCreatePane`] records the directory it was handed
    /// in `seen`, rather than this test scanning the shared temporary
    /// directory for it: several `cargo test` processes walk that directory at once —
    /// see `scratch`'s own module doc — so reading it back here would be
    /// exactly the race that module exists to avoid.
    #[test]
    fn live_pane_removes_its_scratch_directory_even_on_failure() {
        let mux = FailsToCreatePane::new(true);
        assert!(live_pane(&mux).is_err());

        let dir = mux
            .seen
            .lock()
            .unwrap()
            .clone()
            .expect("live_pane must call create_pane before failing");
        assert!(
            !dir.exists(),
            "live_pane left its scratch directory behind: {}",
            dir.display()
        );
    }

    /// A file missing from a project `init` never wrote is not *behind* —
    /// `sync` has nothing to bring forward there — so it gets its own note
    /// naming that fact, one line per file, apart from the single count-based
    /// "behind" note the files `sync` is actually for get.
    #[test]
    fn a_never_initialised_project_is_sent_to_init_not_sync() {
        use crate::sync::Outcome;
        let outcomes = vec![
            Outcome::Wrote {
                path: ".spoolway/config.toml".into(),
                detail: crate::sync::MISSING.into(),
            },
            Outcome::Wrote {
                path: ".spoolway/prompts/a.md".into(),
                detail: "rewritten".into(),
            },
        ];

        let notes = sync_notes(&outcomes, false);
        assert_eq!(
            notes,
            vec![
                ".spoolway/config.toml has never been written".to_string(),
                "1 file(s) are behind this spoolway — run spoolway sync to apply them".to_string(),
            ]
        );

        // In an initialised project a missing file is `sync`'s to restore,
        // so it lands under "behind" like any other.
        let notes = sync_notes(&outcomes, true);
        assert_eq!(
            notes,
            vec![
                "2 file(s) are behind this spoolway — run spoolway sync to apply them".to_string(),
            ]
        );
    }

    /// A file that will not deserialise at all keeps `Pipelines::refusals`'
    /// own per-file detail, even beside a file that carries a retired shape.
    #[test]
    fn pipelines_load_outcome_falls_back_to_refusals_beside_a_genuine_parse_failure() {
        let root = crate::scratch::root("doctor-pipelines-load-outcome-mixed");
        let _ = std::fs::remove_dir_all(&root);
        let dir = crate::pipeline::Pipelines::dir_in(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("broken.yml"),
            "steps:\n  - id: a\n    unknown_key: yes\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("default.yml"),
            "steps:\n  \
             - id: implement\n    agent: pi\n    on_pass: implement\n  \
             - id: z\n    run: x\n    on_pass: done\n",
        )
        .unwrap();

        let err = pipelines_load_outcome(&root, anyhow::anyhow!("stale error, superseded"))
            .expect_err("broken.yml cannot be migrated");
        let message = format!("{err:#}");
        assert!(
            message.contains("broken.yml:") && message.contains("default.yml:"),
            "{message}"
        );
        assert!(!message.contains("stale error"), "{message}");
    }

    /// A load failure outside the four retired shapes — nothing for
    /// `Pipelines::refusals` to say — falls back to the original error
    /// unchanged, exactly what `doctor` always printed here.
    #[test]
    fn pipelines_load_outcome_falls_back_to_the_original_error_otherwise() {
        let root = crate::scratch::root("doctor-pipelines-load-outcome-fallback");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let err = pipelines_load_outcome(&root, anyhow::anyhow!("no pipelines defined"))
            .expect_err("nothing here to migrate");
        assert_eq!(format!("{err:#}"), "no pipelines defined");
    }

    /// A file `sync` refused — hand-edited where only a machine reads — is
    /// the one outcome that is a refusal, and it still surfaces here even
    /// though the "behind" note above only ever counts what a sync would
    /// write.
    #[test]
    fn a_blocked_file_is_still_reported() {
        use crate::sync::Outcome;
        let outcomes = vec![Outcome::Blocked {
            path: ".spoolway/pipelines/default.yml".into(),
            why: "its machine-readable block was edited by hand".into(),
        }];

        let notes = sync_notes(&outcomes, true);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains(".spoolway/pipelines/default.yml")
                && notes[0].contains("edited by hand"),
            "{notes:?}"
        );
    }

    /// A settled binding is an `Ok` check naming the home it settled on —
    /// and, when that home actually carries a record, the id and root the
    /// stamp and `project.toml` agreed on, not only that a directory by
    /// that name exists.
    #[test]
    fn a_bound_project_is_an_ok_check_naming_its_home() {
        let root = crate::scratch::root("doctor-bound");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let repo = Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home: root.join(".home"),
        };

        // No record yet — the plain home path and the mode are all there
        // is to say.
        let Finding::Check(label, outcome) = registration_check(&repo, None) else {
            panic!("registration is a check, not a note");
        };
        assert_eq!(label, "bound to its home");
        assert_eq!(
            outcome.unwrap(),
            Some(format!("{} — repo mode", repo.home.display()))
        );

        // A home that actually carries a binding reports what it settled
        // on, not only that the directory exists.
        std::fs::create_dir_all(&repo.home).unwrap();
        std::fs::write(
            repo.home.join("project.toml"),
            format!(
                "id = \"a1b2c3\"\nroot = \"{}\"\n",
                root.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let Finding::Check(_, outcome) = registration_check(&repo, None) else {
            panic!("registration is a check, not a note");
        };
        let note = outcome.unwrap().unwrap();
        assert!(note.contains("id a1b2c3"), "{note}");
        assert!(note.contains(&root.display().to_string()), "{note}");
    }

    /// A home-mode checkout's registration names the mode, not an id and a
    /// root nothing stamped — "doctor says which mode the project is in",
    /// answered right where a person already looks for what this checkout
    /// is bound to.
    #[test]
    fn a_home_mode_project_is_an_ok_check_naming_the_mode() {
        let root = crate::scratch::root("doctor-home-mode");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let home = crate::scratch::root("doctor-home-mode-home");
        let _ = std::fs::remove_dir_all(&home);
        let workspace = home.join(".spoolway").join("ws");
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(
            workspace.join(crate::repo::BINDING_FILE),
            format!(
                "id = \"ws\"\nclones = [{{ root = \"{}\", dispatcher = \"api\" }}]\n",
                root.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let repo = Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home: workspace.join("dispatchers").join("api"),
        };

        let note = crate::platform::test_home::with_home(&home, || {
            let Finding::Check(label, outcome) = registration_check(&repo, None) else {
                panic!("registration is a check, not a note");
            };
            assert_eq!(label, "bound to its home");
            outcome.unwrap().unwrap()
        });
        assert!(note.contains("home mode"), "{note}");
        assert!(
            note.contains(&workspace.join("config").display().to_string()),
            "{note}"
        );
    }

    /// A repo-mode project, listed in no workspace at all, must not fail
    /// `registration_check` merely because some *other* workspace's
    /// `project.toml` elsewhere under the same `~/.spoolway/` is unreadable.
    /// `registration_check` used to reach the strict `workspace_clone_
    /// checked`, which refuses whenever nothing matches and some workspace
    /// file nearby could not be read — the right call for a checkout that
    /// might be the one that file would have named, wrong here, since
    /// `Repo::root` already proved this checkout is a working project
    /// before `doctor` ever built a `Repo` to check.
    #[test]
    fn registration_check_is_unaffected_by_an_unrelated_unreadable_workspace_file() {
        let root = crate::scratch::root("doctor-repo-mode-broken-sibling");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let home = crate::scratch::root("doctor-repo-mode-broken-sibling-home");
        let _ = std::fs::remove_dir_all(&home);
        let broken = home.join(".spoolway").join("a-1x");
        std::fs::create_dir_all(broken.join("config")).unwrap();
        std::fs::write(broken.join(crate::repo::BINDING_FILE), "garbage = [\n").unwrap();
        let repo = Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home: root.join(".spoolway"),
        };

        let note = crate::platform::test_home::with_home(&home, || {
            let Finding::Check(label, outcome) = registration_check(&repo, None) else {
                panic!("registration is a check, not a note");
            };
            assert_eq!(label, "bound to its home");
            outcome
                .unwrap_or_else(|err| {
                    panic!("an unrelated broken file must not fail this: {err:#}")
                })
                .unwrap()
        });
        assert!(note.contains("repo mode"), "{note}");
    }

    /// A home-mode checkout has no `.spoolway/` of its own — the model
    /// check's own pipeline path and the `config parses` note must name the
    /// workspace's shared `config/`, not a path under this checkout that
    /// does not exist, and never call it "this checkout's own copy".
    #[test]
    fn home_mode_pipeline_and_config_messages_name_the_workspace_not_the_checkout() {
        let root = crate::scratch::root("doctor-home-mode-messages");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let home = crate::scratch::root("doctor-home-mode-messages-home");
        let _ = std::fs::remove_dir_all(&home);
        let workspace = home.join(".spoolway").join("ws");
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(
            workspace.join(crate::repo::BINDING_FILE),
            format!(
                "id = \"ws\"\nclones = [{{ root = \"{}\", dispatcher = \"api\" }}]\n",
                root.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let repo = Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home: workspace.join("dispatchers").join("api"),
        };

        crate::platform::test_home::with_home(&home, || {
            let path = Pipelines::file_in(&repo.checkout, "default");
            assert_eq!(
                path,
                workspace.join("config/pipelines/default.yml"),
                "{}",
                path.display()
            );

            let owner = config_owner_label(&repo);
            assert_eq!(owner, "the workspace's shared config", "{owner}");
            assert_ne!(owner, "this checkout's own copy");
        });
    }

    /// Whatever `bind` could not settle — every one of the seven states the
    /// `binding-record` task defines, surfaced here as `Repo::discover_lenient`'s
    /// own `home_error` — is a failed check carrying `bind`'s own message,
    /// not a fact this rederives from `repo.home` on its own.
    #[test]
    fn an_unsettled_binding_is_a_failed_check_naming_binds_own_error() {
        let root = crate::scratch::root("doctor-unbound");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let repo = Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home: root.join(".home"),
        };
        let home_error = anyhow::anyhow!("no home holds the id abc123");

        let Finding::Check(label, outcome) = registration_check(&repo, Some(&home_error)) else {
            panic!("registration is a check, not a note");
        };
        assert_eq!(label, "bound to its home");
        let err = outcome.expect_err("bind could not settle this checkout's binding");
        assert!(
            format!("{err:#}").contains("no home holds the id abc123"),
            "{err:#}"
        );
    }

    /// `doctor` on a project whose home could not be resolved must never
    /// touch `repo.home`: `Repo::home()` (which every one of
    /// `repo.tasks()`/`repo.archive_dir()`/`repo.lock_file()` goes through)
    /// creates its directory the instant it is asked, and `repo.home` here
    /// is nothing but the basename-keyed placeholder a failed resolution
    /// falls back to — creating it would recreate exactly the unsafe,
    /// wrong-directory write id-keying this project's home exists to
    /// prevent.
    #[test]
    fn a_home_resolution_failure_creates_nothing_under_the_placeholder_home() {
        let root = crate::scratch::root("doctor-home-unavailable-fs");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let home = root.join(".home");
        assert!(!home.exists());
        let repo = Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home,
        };
        let home_error = anyhow::anyhow!("permission denied reading spoolway-id");

        // Every result but success is fine here: a real failure resolving
        // the home is always at least one failed check.
        let _ = doctor(
            &repo,
            Ok(Pipelines::builtin()),
            None,
            Some(home_error),
            false,
            false,
            LiveCheckMode::Skip,
        );

        assert!(
            !repo.home.exists(),
            "doctor must never create the placeholder home it cannot resolve"
        );
    }

    /// A broken home must not cascade into a false config/pipeline failure:
    /// `Config::load`/`Pipelines::load` both read the overrides layer out of
    /// the home that just failed to settle, so reaching for them again here
    /// could report this project's own perfectly good tracked files as
    /// broken. The tracked-only loaders
    /// read the one thing still answerable without a home.
    #[test]
    fn a_home_resolution_failure_does_not_cascade_into_config_or_pipeline_failures() {
        let root = crate::scratch::root("doctor-home-unavailable-cascade");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let repo = Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home: root.join(".home"),
        };
        let home_error = anyhow::anyhow!("permission denied reading spoolway-id");

        let findings = home_unavailable_findings(&repo, None, &home_error);
        let find = |label: &str| {
            findings
                .iter()
                .find_map(|f| match f {
                    Finding::Check(l, outcome) if l == label => Some(outcome),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no {label:?} check in {findings:?}"))
        };
        assert!(
            find("bound to its home").is_err(),
            "the real failure is reported"
        );
        assert!(
            find("config parses").is_ok(),
            "a checkout with no config.toml still loads its (default) tracked config: {:?}",
            find("config parses")
        );
        let pipelines_err = find("pipelines are valid").as_ref().expect_err(
            "a checkout with no pipelines/ now says so instead of borrowing the built-ins",
        );
        let pipelines_msg = format!("{pipelines_err:#}");
        assert!(
            pipelines_msg.contains("no pipelines defined"),
            "the tracked-only loader's own read, not a home-read failure cascading in: {pipelines_msg:?}"
        );
        assert!(
            !pipelines_msg.contains("spoolway-id"),
            "a broken home must not surface here as a pipeline failure: {pipelines_msg:?}"
        );
    }

    /// The same claim as
    /// [`a_home_resolution_failure_does_not_cascade_into_config_or_pipeline_failures`],
    /// but against `Repo::discover_lenient`'s own tuple rather than a
    /// synthetic `None` `config_error` — the cascade this guards against
    /// was `discover_lenient`'s `Config::load` failing for the same reason
    /// `project_home` just did, which a hand-built `None` can never
    /// reproduce.
    #[test]
    fn a_real_home_resolution_failure_does_not_cascade_into_a_config_finding() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::scratch::root("doctor-home-unavailable-real-cascade");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-q", "-b", "main"]);
        // The tracked control plane `Repo::root`'s ancestor walk insists on
        // — real, well-formed, and exactly what proves this run's failure
        // is the stamped home alone, not this file too.
        let state = root.join(crate::config::STATE_DIR);
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("config.toml"), "").unwrap();
        crate::repo::stamped_id(&root).unwrap();
        let id_file = root.join(".git").join("spoolway-id");
        let mut perms = std::fs::metadata(&id_file).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&id_file, perms.clone()).unwrap();

        let lenient = Repo::discover_lenient(&root);

        perms.set_mode(0o644);
        std::fs::set_permissions(&id_file, perms).unwrap();

        let (repo, config_error, home_error) =
            lenient.expect("a home resolution failure is a finding, not a fatal error");
        let home_error = home_error.expect("the real read failure is handed back");

        let findings = home_unavailable_findings(&repo, config_error, &home_error);
        let has = |label: &str| {
            findings
                .iter()
                .any(|f| matches!(f, Finding::Check(l, _) if l == label))
        };
        assert!(
            !has("the project's own config parses"),
            "a broken home alone must not also read as a broken config: {findings:?}"
        );
        let Some(Finding::Check(_, outcome)) = findings
            .iter()
            .find(|f| matches!(f, Finding::Check(l, _) if l == "config parses"))
        else {
            panic!("no 'config parses' check in {findings:?}");
        };
        assert!(
            outcome.is_ok(),
            "the tracked config itself is perfectly fine: {outcome:?}"
        );
    }

    /// Acceptance criterion 6 of `workspace-scan-strict`, `doctor`'s own
    /// corner: `registration_check` reaches
    /// `crate::repo::workspace_clone_lenient` for every project — repo mode
    /// included. A `~/.spoolway/` holding everything a 0.6.0 (or legacy)
    /// install could leave beside a real workspace must not turn into a
    /// finding here either.
    #[test]
    fn registration_check_is_unaffected_by_a_mixed_home_from_older_installs() {
        let root = crate::scratch::root("doctor-mixed-home");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let home = crate::scratch::root("doctor-mixed-home-dollar-home");
        let _ = std::fs::remove_dir_all(&home);
        let state = home.join(".spoolway");
        std::fs::create_dir_all(&state).unwrap();

        // A 0.6.0 repo-mode home: an ordinary `Binding`, no `clones`, no
        // `config/`/`dispatchers/` beside it.
        let repo_mode = state.join("repo-mode-home-abc123");
        std::fs::create_dir_all(&repo_mode).unwrap();
        std::fs::write(
            repo_mode.join("project.toml"),
            "id = \"abc123\"\nroot = \"/some/other/checkout\"\n",
        )
        .unwrap();
        // A legacy home: no `project.toml` at all.
        std::fs::create_dir_all(state.join("spoolway")).unwrap();
        // A folder that is no home at all.
        std::fs::create_dir_all(state.join("logs")).unwrap();

        let repo = Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: Config::default(),
            home: root.join(".home"),
        };

        let finding =
            crate::platform::test_home::with_home(&home, || registration_check(&repo, None));
        let Finding::Check(label, outcome) = finding else {
            panic!("registration_check always returns a Check");
        };
        assert_eq!(label, "bound to its home");
        let message = outcome.expect(
            "a mixed ~/.spoolway/ must not turn into a finding for a project none of it lists",
        );
        assert!(
            message.is_some_and(|m| m.contains("repo mode")),
            "this checkout is in repo mode and none of the mixed siblings should change that"
        );
    }

    /// [`warnings_from_rows`]'s own mapping, task `warnings-screen`'s
    /// acceptance criterion 1: a passing row has no room on the screen, a
    /// failure becomes a problem, a note in the `doctor_sync` range becomes a
    /// file note, an ordinary note becomes a setting, and a `verbose_only`
    /// note — describing the machine rather than the project — is dropped
    /// like the short report already drops it.
    #[test]
    fn warnings_from_rows_sorts_ok_fail_and_note_into_their_sections() {
        let rows = vec![
            Row::Ok {
                label: "config parses".into(),
                note: None,
            },
            Row::Fail {
                label: "prompt `archivist`".into(),
                why: "missing".into(),
            },
            Row::Note {
                text: "config.toml is behind".into(),
                verbose_only: false,
            },
            Row::Note {
                text: "unattended has no ceiling".into(),
                verbose_only: false,
            },
            Row::Note {
                text: "no dispatcher running".into(),
                verbose_only: true,
            },
        ];
        // Row 2 (index 2) is the one `doctor_sync` row in this hand-built
        // set — the range a real call gets from `report.rows.len()` either
        // side of its own `doctor_sync` call.
        let warnings = warnings_from_rows(rows, 2..3);
        assert_eq!(
            warnings,
            vec![
                Warning::Problem("prompt `archivist`: missing".into()),
                Warning::File("config.toml is behind".into()),
                Warning::Setting("unattended has no ceiling".into()),
            ]
        );
    }

    /// Acceptance criterion 1: with an active override layer on top of an
    /// otherwise ordinary project, [`cheap_findings`] carries neither the
    /// override-layer note — `dispatch`'s own overrides gate already has
    /// this, one screen earlier — nor anything from the two checks that
    /// reach the network (`gh auth status`, and a `git ls-remote` per base)
    /// or the one that opens a real pane.
    #[test]
    fn cheap_findings_excludes_network_pane_and_override_layer_checks() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("doctor-cheap-findings");
        let overrides = repo.overrides_dir();
        std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
        std::fs::write(
            overrides.join("pipelines/default.yml"),
            "steps:\n  implement:\n    model: fake-opus\n",
        )
        .unwrap();

        let pipelines = Pipelines::builtin();
        let config = Config::default();
        let warnings = cheap_findings(&repo, &pipelines, &config);

        fn text(w: &Warning) -> &str {
            match w {
                Warning::Setting(t) | Warning::File(t) | Warning::Problem(t) => t.as_str(),
            }
        }
        assert!(
            warnings
                .iter()
                .all(|w| !text(w).contains("overrides are active")),
            "the override layer has its own screen already: {warnings:?}"
        );
        assert!(
            warnings.iter().all(|w| {
                !text(w).contains("gh auth")
                    && !text(w).contains("is publishable")
                    && !text(w).contains("a lane really starts")
            }),
            "no network or live-pane check may appear on this path: {warnings:?}"
        );
    }

    /// A `[models]` row no step routes to is dead config that costs nothing at
    /// run time, so the warnings gate stays quiet about it; `spoolway doctor`
    /// still names the row.
    #[test]
    fn an_unrouted_model_row_stays_off_the_warnings_gate() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("doctor-unrouted-model-gate");
        let pipelines = Pipelines::builtin();
        let mut config = Config::default();
        config.models.insert(
            "Ornith-1.5-35B-A3B".into(),
            crate::usage::ModelPrice::default(),
        );

        let doctor_notes = unrouted_model_notes(&pipelines, &config)
            .iter()
            .filter(|f| matches!(f, Finding::Note(t) if t.contains("routes to no step")))
            .count();
        assert_eq!(doctor_notes, 1, "the doctor report must still name the row");

        let warnings = cheap_findings(&repo, &pipelines, &config);
        let routed: Vec<&Warning> = warnings
            .iter()
            .filter(|w| match w {
                Warning::Setting(t) | Warning::File(t) | Warning::Problem(t) => {
                    t.contains("routes to no step")
                }
            })
            .collect();
        assert!(routed.is_empty(), "{routed:?}");
    }
}

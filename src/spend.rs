//! `spoolway spend`: the lane ledger read back out, grouped and totalled.
//!
//! This is a separate read of the same `usage.jsonl` [`crate::eval`]'s
//! version screen is, through the same [`collect_scoped`] and [`window_of`]
//! that screen uses — one ledger, two different questions asked of it.
//! `eval --by` is a deprecated alias for [`run`], kept so a script or skill
//! written before the split keeps working.

use anyhow::{Context, Result, bail};

use crate::cli::{SpendArgs, SpendBy};
use crate::fmt::{csv_field, money, money_plain, tokens_human};
use crate::repo::Repo;

/// The filters `spoolway spend` reads the ledger through.
pub struct Filters<'a> {
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub month: Option<&'a str>,
    pub all: bool,
    pub project: Option<&'a str>,
}

/// `spoolway spend`'s entry point: build the filters `SpendArgs` carries and
/// hand off to [`print`], the same way `eval --by`'s deprecated alias does.
pub fn run(repo: &Repo, args: &SpendArgs, json: bool) -> Result<()> {
    if json && args.csv {
        bail!("`--csv` and `--json` are two different exports of the same rows — pick one");
    }
    let filters = Filters {
        since: args.since.as_deref(),
        until: args.until.as_deref(),
        month: args.month.as_deref(),
        all: args.all,
        project: args.project.as_deref(),
    };
    print(repo, args.by, &filters, json, args.csv)
}

/// What the pipeline has spent, read back out of the lane ledger.
///
/// Every figure here is collected, never estimated: token counts come from the
/// transcripts the agents themselves wrote, and a model nobody priced is
/// reported as unpriced rather than folded into the total as zero.
///
/// `by` of `None` is a bare cut: resolved below to `step`, or `project` when
/// more than one project is in scope — the same "auto" `SpendBy::Auto` used
/// to be a hidden enum variant for, before the spend table had a command of
/// its own to be bare on.
pub fn print(
    repo: &Repo,
    by: Option<SpendBy>,
    filters: &Filters,
    json: bool,
    csv: bool,
) -> Result<()> {
    // Reading is also what catches the ledger up: an interactive session is
    // enrolled by the first spoolway command run in it and swept by every read
    // afterwards, so what a skill has spent since then is on the ledger before
    // anything is grouped. Idempotent, and to this project's ledger only.
    crate::usage::sweep(repo);

    let (mut entries, scope) = collect(repo, filters)?;

    let window = window_of(filters.month, filters.since, filters.until)?;
    let bounded = window.from.is_some() || window.until.is_some();
    entries.retain(|entry| window.contains(&entry.ts));
    entries.sort_by(|a, b| a.ts.cmp(&b.ts));

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("{}", nothing_to_report(repo, filters, &scope, bounded));
        return Ok(());
    }

    if by == Some(SpendBy::Lane) {
        return match csv {
            true => print_runs_csv(&entries),
            false => print_runs(&entries, scope.many()),
        };
    }

    // Reading several projects at once and then grouping by task would merge
    // two projects' identically named tasks into one row without saying so.
    // Project is the meaningful cut there, so it is the default — an explicit
    // cut still wins, because summing every project's review step is a fair
    // question to ask.
    let by = by.unwrap_or(match scope.many() {
        true => SpendBy::Project,
        false => SpendBy::Step,
    });

    // Two tables, two units. A skill line has no run, no outcome and no wall
    // time, so filing it in the STEP column would put it under headings that
    // describe none of it — it goes in a block of its own, below. The `skill`
    // cut asks for the opposite, and folds the whole pipeline into a single
    // row so the skills can be compared against it.
    let (skills, lanes): (Vec<&crate::usage::Entry>, Vec<&crate::usage::Entry>) =
        entries.iter().partition(|entry| entry.is_skill());
    let mixed = by != SpendBy::Skill && !skills.is_empty();

    // Group in a BTreeMap so rows come out in a stable order whatever the
    // ledger's write order was.
    let mut groups: std::collections::BTreeMap<String, Totals> = std::collections::BTreeMap::new();
    let mut all = Totals::default();
    let everything: Vec<&crate::usage::Entry> = entries.iter().collect();
    let grouped: &[&crate::usage::Entry] = match by {
        SpendBy::Skill => &everything,
        _ => &lanes,
    };
    for entry in grouped {
        let key = match by {
            SpendBy::Task => entry.task.clone(),
            // `entry.plan` is the ledger's own field name — it carries a
            // task's `group:` verbatim, banked at
            // [`crate::dispatch::Dispatcher`]'s own report time.
            SpendBy::Group => entry.plan.clone().unwrap_or_else(|| "—".into()),
            SpendBy::Step => entry.step.clone(),
            SpendBy::Model => entry.model.clone(),
            SpendBy::Project => entry.project.clone(),
            SpendBy::Month => crate::usage::month_of(&entry.ts),
            // Every lane, whatever it ran, in one row: this cut is about the
            // skills, and the pipeline is the thing they are measured against.
            SpendBy::Skill => match entry.skill_label() {
                Some(skill) => skill.to_string(),
                None => "pipeline".to_string(),
            },
            // Resolved above, and `Lane` returned before this loop.
            SpendBy::Lane => unreachable!(),
        };
        groups.entry(key).or_default().add(entry);
        all.add(entry);
    }

    // Distinct sessions, not rows: one session banks a line every time it is
    // swept, so counting rows would report a single afternoon as six.
    let mut by_skill: std::collections::BTreeMap<String, Totals> =
        std::collections::BTreeMap::new();
    let mut skills_total = Totals::default();
    if mixed {
        for entry in &skills {
            let key = entry.skill_label().unwrap_or_default().to_string();
            by_skill.entry(key).or_default().add(entry);
            skills_total.add(entry);
        }
    }

    let label = match by {
        SpendBy::Task => "TASK",
        SpendBy::Group => "GROUP",
        SpendBy::Step => "STEP",
        SpendBy::Model => "MODEL",
        SpendBy::Project => "PROJECT",
        SpendBy::Month => "MONTH",
        SpendBy::Skill => "SKILL",
        SpendBy::Lane => unreachable!(),
    };
    // Only the labels that will actually be printed: a ledger with no skill
    // line on it must not have its columns widened by the words `pipeline` and
    // `skills`, which it will never show.
    let mut longest = groups
        .keys()
        .chain(by_skill.keys())
        .map(String::len)
        .max()
        .unwrap_or(5)
        .max(label.len())
        .max("total".len());
    if mixed {
        longest = longest.max("pipeline".len()).max("skills".len());
    }
    // A table counted in sessions borrows three characters of the label column
    // for its wider heading, so the label column has to have three to spare —
    // see `session_head`. Only where such a table is printed at all, which is
    // what keeps a ledger with no skill line on it printing what it always
    // printed.
    let width = match by == SpendBy::Skill || mixed {
        true => longest + 3,
        false => longest,
    };

    // The flat export a person actually pastes into a spreadsheet: one row
    // per group already computed above, whatever `by` cut them by. No colour,
    // no `+?` — `Totals::csv_cost` says a floor the plain way a CSV cell can,
    // by naming the row's own unpriced count instead.
    if csv {
        return print_group_csv(by, &groups, &all, mixed, &by_skill, &skills_total);
    }

    // the `skill` cut is the one grouping whose rows are not lanes, so it counts
    // the session and drops WALL — the same unit the skills block uses, for
    // the same reason, since it is the same question asked the other way
    // round.
    if by == SpendBy::Skill {
        println!("{}", session_head(label, width));
        for (key, totals) in &groups {
            println!("{}", totals.session_row(key, width));
        }
        println!("{}", all.session_row("total", width));
    } else if !groups.is_empty() {
        println!(
            "{:<width$}  {:>5}  {:>9}  {:>9}  {:>9}  {:>9}  {:>10}  {:>8}",
            label, "LANES", "IN", "OUT", "CACHE R", "CACHE W", "COST USD", "WALL"
        );
        for (key, totals) in &groups {
            println!("{}", totals.row(key, width));
        }
    }

    if by == SpendBy::Skill {
        // One table, already totalled.
    } else if groups.is_empty() {
        // A project that has planned but never dispatched. There is no
        // pipeline table to put a subtotal under, and the skills block is
        // then the whole report — so it carries the total itself.
        println!("{}", session_head("SKILL", width));
        for (key, totals) in &by_skill {
            println!("{}", totals.session_row(key, width));
        }
        println!("{}", skills_total.session_row("total", width));
    } else if !mixed {
        // The only table there is, so its last row is the grand total — which
        // is exactly what a project with no skill line on its ledger prints.
        println!("{}", all.row("total", width));
    } else {
        println!("{}", all.row("pipeline", width));
        println!();
        println!("{}", session_head("SKILL", width));
        for (key, totals) in &by_skill {
            println!("{}", totals.session_row(key, width));
        }
        println!("{}", skills_total.session_row("skills", width));

        // One grand total over both tables. No count and no WALL: neither
        // column means the same thing on both sides of it.
        let mut both = all.clone();
        both.merge(&skills_total);
        println!();
        println!("{}", both.plain_row("total", width));
    }

    // Say plainly which part of that total is a guess-free blank rather than a
    // zero, and what to do about it.
    // A line that spent nothing is missing nothing from the total, whatever it
    // says about a price — an enrolment line names no model at all, and listing
    // it here would ask for a price for the empty string.
    let unpriced: std::collections::BTreeSet<&str> = entries
        .iter()
        .filter(|e| e.cost_usd.is_none() && !e.tokens.is_zero())
        .map(|e| e.model.as_str())
        .collect();
    if !unpriced.is_empty() {
        println!(
            "\nNot in the total — no price configured for: {}",
            unpriced.into_iter().collect::<Vec<_>>().join(", ")
        );
        println!("Add one with `spoolway config set models.<model-glob>.input <usd per 1M>`.");
    }

    if let Some(hint) = elsewhere(repo, filters) {
        println!("\n{hint}");
    }

    Ok(())
}

/// The header over a table counted in sessions rather than lanes.
///
/// SESSIONS is three characters wider than LANES, and is written into the
/// label column's own slack rather than given a column of its own — so the
/// token columns stay in line with the pipeline table above it.
fn session_head(label: &str, width: usize) -> String {
    format!(
        "{:<label_w$}  {:>8}  {:>9}  {:>9}  {:>9}  {:>9}  {:>10}",
        label,
        "SESSIONS",
        "IN",
        "OUT",
        "CACHE R",
        "CACHE W",
        "COST USD",
        label_w = width - 3,
    )
}

/// A one-line note that this report is not everything, when that is true.
///
/// Deliberately not a general "see --help" footer: a hint printed on every run
/// stops being read by the third one. This says something the report itself
/// cannot — that other projects have spend in them — and says nothing at all
/// when there is nothing to say.
fn elsewhere(repo: &Repo, filters: &Filters) -> Option<String> {
    use std::io::IsTerminal;

    // Piped or redirected output belongs to a script, which did not ask for
    // advice and may well be parsing these lines.
    if !std::io::stdout().is_terminal() || filters.all || filters.project.is_some() {
        return None;
    }

    let others: Vec<String> = crate::usage::registry::list()
        .into_iter()
        .filter(|root| *root != repo.root)
        .filter(|root| crate::usage::project_has_ledger(root))
        .map(|root| crate::usage::registry::name_of(&root))
        .collect();
    if others.is_empty() {
        return None;
    }

    // Naming them is the useful part — `--all` is guessable, the fact that
    // `webshop` has been spending money is not.
    let named = match others.len() {
        n if n > 3 => format!("{} and {} more", others[..3].join(", "), n - 3),
        _ => others.join(", "),
    };
    Some(format!(
        "Not shown: {named}. `--all` includes every project; `spoolway eval --help` has the rest."
    ))
}

/// Which ledgers this invocation is reading.
pub struct Scope {
    /// Human description for the header and the nothing-found message.
    pub what: String,
    projects: usize,
}

impl Scope {
    fn many(&self) -> bool {
        self.projects > 1
    }
}

/// Load the ledgers this invocation asks for.
///
/// The default is this project alone, which is not a filter but the shape of
/// the storage: a ledger lives inside its project. `--all` and `--project` are
/// what reach past that, through the registry.
fn collect(repo: &Repo, filters: &Filters) -> Result<(Vec<crate::usage::Entry>, Scope)> {
    collect_scoped(repo, filters.project, filters.all)
}

/// The window a `--month`, `--since` and `--until` triple describes.
///
/// Shared with `spoolway eval`'s own version screen, which takes the same
/// three forms for the same reason: a person asking what August cost and a
/// person asking what August's pipeline versions cost mean the same August.
pub fn window_of(
    month: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<crate::usage::Window> {
    let window = match month {
        Some(month) => {
            crate::usage::parse_month(month).with_context(|| format!("`--month {month}`"))?
        }
        None => crate::usage::Window {
            from: since
                .map(|raw| crate::usage::parse_instant(raw, false))
                .transpose()
                .context("`--since`")?,
            until: until
                .map(|raw| crate::usage::parse_instant(raw, true))
                .transpose()
                .context("`--until`")?,
        },
    };
    if let (Some(from), Some(until)) = (window.from, window.until)
        && from >= until
    {
        bail!("that window ends before it starts");
    }
    Ok(window)
}

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
        return Ok((
            crate::usage::read_project(&root),
            Scope { what, projects: 1 },
        ));
    }

    if all {
        let roots = registry::list();
        let entries: Vec<crate::usage::Entry> = roots
            .iter()
            .flat_map(|r| crate::usage::read_project(r))
            .collect();
        // Only projects that actually spent something are worth counting as
        // being in scope: an empty ledger should not turn a single-project
        // report into a multi-project one.
        let with_entries = entries
            .iter()
            .map(|e| e.project.clone())
            .collect::<std::collections::BTreeSet<String>>()
            .len();
        return Ok((
            entries,
            Scope {
                what: format!("{} projects", roots.len()),
                projects: with_entries,
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
            projects: 1,
        },
    ))
}

/// What to say when the window came back empty, which has several quite
/// different causes worth telling apart.
fn nothing_to_report(repo: &Repo, filters: &Filters, scope: &Scope, bounded: bool) -> String {
    if bounded {
        return format!("No lanes in that window, in {}.", scope.what);
    }
    if filters.all || filters.project.is_some() {
        return format!("Nothing recorded in {} yet.", scope.what);
    }

    let path = crate::usage::ledger_path(repo);
    format!(
        "Nothing recorded yet — {} appears once a lane has finished.\n\
         If lanes have run, `spoolway doctor` says whether their agent kind is one spoolway \
         knows how to launch at all — a lane on a kind it does not spends unaccounted.\n\
         Other projects are not included here; `spoolway spend --all` reads them too.",
        path.display()
    )
}

/// The STEP column's own width: the widest label actually printed there,
/// never the fixed twelve characters a hardcoded `{:<12}` used to hold. A
/// skill line's STEP is the skill name — `spoolway-plan`, `spoolway-calibrate`
/// — which routinely runs past twelve, and every column right of a fixed
/// width shifted out of line for it. Computed the same way TASK's own width
/// already was.
fn step_width(entries: &[crate::usage::Entry]) -> usize {
    entries
        .iter()
        .map(|e| match e.skill_label() {
            Some(skill) => skill.len(),
            None => e.step.len(),
        })
        .max()
        .unwrap_or(4)
        .max(4)
}

fn runs_header(with_project: bool, pw: usize, width: usize, sw: usize) -> String {
    // The project column only earns its width when more than one is in scope.
    let project = |text: &str| match with_project {
        true => format!("{text:<pw$}  "),
        false => String::new(),
    };
    format!(
        "{:<19}  {}{:<width$}  {:<sw$}  {:>5}  {:>9}  {:>10}",
        "WHEN",
        project("PROJECT"),
        "TASK",
        "STEP",
        "TURNS",
        "TOKENS",
        "COST"
    )
}

fn runs_row(
    entry: &crate::usage::Entry,
    with_project: bool,
    pw: usize,
    width: usize,
    sw: usize,
) -> String {
    let project = |text: &str| match with_project {
        true => format!("{text:<pw$}  "),
        false => String::new(),
    };
    let when = entry.ts.get(..19).unwrap_or(&entry.ts).replace('T', " ");
    // A skill line has no task and no step. What it *was* goes in the STEP
    // column all the same — a row that says only when and how much says
    // nothing at the grain this view exists for.
    let (task, step) = match entry.skill_label() {
        Some(skill) => ("—", skill),
        None => (entry.task.as_str(), entry.step.as_str()),
    };
    format!(
        "{:<19}  {}{:<width$}  {:<sw$}  {:>5}  {:>9}  {:>10}",
        when,
        project(&entry.project),
        task,
        step,
        entry.turns,
        tokens_human(entry.tokens.total()),
        money(entry.cost_usd),
    )
}

fn print_runs(entries: &[crate::usage::Entry], with_project: bool) -> Result<()> {
    let width = entries
        .iter()
        .map(|e| e.task.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let pw = entries
        .iter()
        .map(|e| e.project.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let sw = step_width(entries);

    println!("{}", runs_header(with_project, pw, width, sw));
    for entry in entries {
        println!("{}", runs_row(entry, with_project, pw, width, sw));
    }
    Ok(())
}

/// `spoolway spend lane --csv`: the same one-row-per-lane shape [`print_runs`]
/// prints, flattened — raw timestamps, raw tokens, and a blank cost cell
/// rather than the table's `-` when a lane's model has no configured price.
fn print_runs_csv(entries: &[crate::usage::Entry]) -> Result<()> {
    println!("when,project,task,step,turns,tokens,cost");
    for entry in entries {
        let (task, step) = match entry.skill_label() {
            Some(skill) => ("—", skill),
            None => (entry.task.as_str(), entry.step.as_str()),
        };
        println!(
            "{},{},{},{},{},{},{}",
            csv_field(&entry.ts),
            csv_field(&entry.project),
            csv_field(task),
            csv_field(step),
            entry.turns,
            entry.tokens.total(),
            entry.cost_usd.map_or(String::new(), |c| format!("{c:.2}")),
        );
    }
    Ok(())
}

/// `spoolway spend --csv`: one row per group already computed for the plain
/// table, flattened the same way [`print_runs_csv`] flattens the lane table —
/// a `kind` column tells a pipeline row from a skill row apart when the two
/// tables are mixed, since a CSV has no second heading to say so instead.
fn print_group_csv(
    by: SpendBy,
    groups: &std::collections::BTreeMap<String, Totals>,
    all: &Totals,
    mixed: bool,
    by_skill: &std::collections::BTreeMap<String, Totals>,
    skills_total: &Totals,
) -> Result<()> {
    println!("kind,cut,lanes,sessions,in,out,cache_r,cache_w,cost,wall_s");
    let kind = if by == SpendBy::Skill {
        "skill"
    } else {
        "pipeline"
    };
    for (key, totals) in groups {
        println!("{}", totals.csv_row(kind, key));
    }
    println!("{}", all.csv_row(kind, "total"));
    if mixed {
        for (key, totals) in by_skill {
            println!("{}", totals.csv_row("skill", key));
        }
        println!("{}", skills_total.csv_row("skill", "total"));
    }
    Ok(())
}

#[derive(Default, Clone)]
struct Totals {
    lanes: u32,
    tokens: crate::usage::Tokens,
    cost: f64,
    /// Lanes in this group that could be priced, and lanes that could not.
    ///
    /// Both are needed to print an honest figure. A group where nothing is
    /// priced is unknown, not free; a group where only some lanes are priced
    /// has a floor rather than a total, and saying `0` for either would hide
    /// exactly the spend this command exists to surface.
    priced: u32,
    unpriced: u32,
    wall_s: i64,
    /// Distinct sessions behind these rows, for the groups counted in
    /// sessions rather than lanes. One interactive session banks a line every
    /// time it is swept, so its row count says how often somebody ran
    /// `spoolway spend`, not how much work was done.
    sessions: std::collections::BTreeSet<String>,
}

impl Totals {
    fn add(&mut self, entry: &crate::usage::Entry) {
        self.lanes += 1;
        self.tokens.add(&entry.tokens);
        self.wall_s += entry.wall_s;
        self.sessions.insert(entry.session.clone());
        match entry.cost_usd {
            Some(cost) => {
                self.cost += cost;
                self.priced += 1;
            }
            // A line that spent nothing is missing nothing from the total,
            // whatever it says about a price — an enrolment line names no
            // model at all, so it must not turn a group's total into a floor
            // the way a real unpriced model does. See `money`'s footer note.
            None if entry.tokens.is_zero() => {}
            None => self.unpriced += 1,
        }
    }

    /// Fold another group in whole. Used for the one grand total that spans
    /// the pipeline table and the skills block.
    fn merge(&mut self, other: &Totals) {
        self.lanes += other.lanes;
        self.tokens.add(&other.tokens);
        self.wall_s += other.wall_s;
        self.cost += other.cost;
        self.priced += other.priced;
        self.unpriced += other.unpriced;
        self.sessions.extend(other.sessions.iter().cloned());
    }

    /// This group's cost, plain — the currency is named once in the column's
    /// own `COST USD` header rather than in every cell. Whether it is only a
    /// floor is not marked on the number any more: the footer names the
    /// models responsible, for the whole report at once.
    fn money(&self) -> String {
        match self.priced {
            0 => "—".to_string(),
            _ => money_plain(Some(self.cost)),
        }
    }

    /// This group's cost as a CSV cell: blank when nothing in the group is
    /// priced, otherwise the raw number — the same floor the table's own
    /// cost prints now, with the `unpriced` column beside it (in
    /// [`Totals::csv_row`]) telling a reader when it is a floor rather than
    /// a total.
    fn csv_cost(&self) -> String {
        match self.priced {
            0 => String::new(),
            _ => format!("{:.2}", self.cost),
        }
    }

    fn csv_row(&self, kind: &str, key: &str) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{}",
            kind,
            csv_field(key),
            self.lanes,
            self.sessions.len(),
            self.tokens.input,
            self.tokens.output,
            self.tokens.cache_read,
            self.tokens.cache_write(),
            self.csv_cost(),
            self.wall_s.max(0),
        )
    }

    fn row(&self, key: &str, width: usize) -> String {
        format!(
            "{:<width$}  {:>5}  {:>9}  {:>9}  {:>9}  {:>9}  {:>10}  {:>8}",
            key,
            self.lanes,
            tokens_human(self.tokens.input),
            tokens_human(self.tokens.output),
            tokens_human(self.tokens.cache_read),
            tokens_human(self.tokens.cache_write()),
            self.money(),
            crate::config::format_duration(std::time::Duration::from_secs(
                self.wall_s.max(0) as u64
            )),
        )
    }

    /// A skills-block row: counted in sessions, and with no WALL — a session's
    /// open hours are how long you had the window up, not model time.
    fn session_row(&self, key: &str, width: usize) -> String {
        format!(
            "{:<label$}  {:>8}  {:>9}  {:>9}  {:>9}  {:>9}  {:>10}",
            key,
            self.sessions.len(),
            tokens_human(self.tokens.input),
            tokens_human(self.tokens.output),
            tokens_human(self.tokens.cache_read),
            tokens_human(self.tokens.cache_write()),
            self.money(),
            label = width - 3,
        )
    }

    /// The grand total over two tables whose units differ: tokens and money
    /// only, since those are all that mean the same thing on both sides.
    fn plain_row(&self, key: &str, width: usize) -> String {
        format!(
            "{:<width$}  {:>5}  {:>9}  {:>9}  {:>9}  {:>9}  {:>10}",
            key,
            "",
            tokens_human(self.tokens.input),
            tokens_human(self.tokens.output),
            tokens_human(self.tokens.cache_read),
            tokens_human(self.tokens.cache_write()),
            self.money(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::*;

    /// The failure this guards against is the quiet one: a run of free local
    /// lanes plus one unpriced cloud lane must still show the `0` it
    /// actually priced, not blur it into the wholly-unknown `—`. The cell
    /// itself no longer marks that this total is a floor — `Totals.unpriced`
    /// records that, and the footer note names the model responsible — but
    /// the number has to stay the real, partial one.
    #[test]
    fn a_partly_priced_group_shows_its_priced_amount_not_an_unknown() {
        let mut totals = Totals::default();
        totals.add(&lane(Some(0.0)));
        totals.add(&lane(None));
        assert_eq!(totals.money(), "0");
    }

    #[test]
    fn a_wholly_unpriced_group_is_unknown_rather_than_free() {
        let mut totals = Totals::default();
        totals.add(&lane(None));
        assert_eq!(totals.money(), "—");
    }

    #[test]
    fn a_fully_priced_group_says_so_plainly() {
        let mut totals = Totals::default();
        totals.add(&lane(Some(17.52)));
        totals.add(&lane(Some(26.81)));
        assert_eq!(totals.money(), "44.33");
    }

    /// A CSV cell carries no floor marker of its own, so it prints the same
    /// raw number a full total would — the reader is trusted to have already
    /// seen the note about which model went unpriced.
    #[test]
    fn a_partly_priced_group_s_csv_cost_is_a_floor_with_no_qualifier() {
        let mut totals = Totals::default();
        totals.add(&lane(Some(1.0)));
        totals.add(&lane(None));
        assert_eq!(totals.csv_cost(), "1.00");
    }

    #[test]
    fn a_wholly_unpriced_group_s_csv_cost_is_blank_not_zero() {
        let mut totals = Totals::default();
        totals.add(&lane(None));
        assert_eq!(totals.csv_cost(), "");
    }

    #[test]
    fn a_csv_row_leads_with_the_kind_and_the_key() {
        let mut totals = Totals::default();
        totals.add(&lane(Some(2.5)));
        let row = totals.csv_row("pipeline", "review");
        assert!(row.starts_with("pipeline,review,1,"), "row was: {row:?}");
    }

    /// Local lanes really are free, and that has to read as a fact rather than
    /// as a missing figure.
    #[test]
    fn a_priced_zero_is_not_an_unknown() {
        let mut totals = Totals::default();
        totals.add(&lane(Some(0.0)));
        assert_eq!(totals.money(), "0");
    }

    /// A zero-token enrolment line names no model at all — it has nothing to
    /// price and nothing missing from the total — so it must not turn a group
    /// with real, priced spend into a floor the way an actually unpriced
    /// model would. This is the interactive row's own `+?` bug: three
    /// zero-token lines behind an otherwise fully-priced group used to be
    /// enough to mark it a floor forever.
    #[test]
    fn a_zero_token_unpriced_line_never_turns_a_total_into_a_floor() {
        let mut zero_token = lane(None);
        zero_token.tokens = crate::usage::Tokens::default();

        let mut totals = Totals::default();
        totals.add(&lane(Some(17.52)));
        totals.add(&zero_token);
        assert_eq!(totals.money(), "17.52");
    }

    /// STEP used to be a fixed twelve characters — `{:<12}` — so a label
    /// longer than that, like `spoolway-calibrate`, pushed every column right
    /// of it out of line. This pins the property that broke: the header and
    /// every row must draw to the same width, whatever the widest label is.
    #[test]
    fn the_step_column_is_as_wide_as_its_widest_label() {
        let mut short = lane(Some(1.0));
        short.skill = None;
        short.task = "login".into();
        short.step = "review".into();
        let mut long = lane(Some(2.0));
        long.skill = Some("spoolway-calibrate".into());

        let entries = vec![short, long];
        let sw = step_width(&entries);
        assert_eq!(sw, "spoolway-calibrate".len());

        let width = "login".len();
        let header = runs_header(false, 7, width, sw);
        for entry in &entries {
            let row = runs_row(entry, false, 7, width, sw);
            assert_eq!(
                header.chars().count(),
                row.chars().count(),
                "header: {header:?}\nrow:    {row:?}"
            );
        }
    }
}

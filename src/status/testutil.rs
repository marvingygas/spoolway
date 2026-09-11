//! Fixtures shared by the board's model and view tests.

use std::collections::VecDeque;

use super::view::{RecentEvent, Verdict, strip_ansi};
use super::*;

/// `text` as a reader sees it, with the escape codes taken out whole.
/// [`strip_ansi`] itself is the production copy of this, needed now to
/// keep a confirm panel's overlay lined up on visible columns.
pub fn strip(text: &str) -> String {
    strip_ansi(text)
}

/// A queue on disk, two tasks deep: one at a step, one waiting on it.
pub fn fixture(name: &str) -> Repo {
    let root = crate::scratch::root(&format!("board-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    crate::scratch::git_init(&root, &["-b", "group/demo"]);
    Repo {
        home: root.join(".home"),
        checkout: root.clone(),
        root,
        config: crate::config::Config::default(),
    }
}

pub fn add(repo: &Repo, id: &str, depends_on: &[&str], stage: Option<&str>) {
    add_to(repo, id, depends_on, stage, Some("demo"));
}

pub fn add_to(
    repo: &Repo,
    id: &str,
    depends_on: &[&str],
    stage: Option<&str>,
    group: Option<&str>,
) {
    let mut frontmatter = format!(
        "id: {id}\ntitle: {id}, done\ngroup: {}\n",
        group.unwrap_or("demo")
    );
    if !depends_on.is_empty() {
        frontmatter += &format!("depends_on: [{}]\n", depends_on.join(", "));
    }
    let doc = format!("---\n{frontmatter}---\n## Goal\n\nDo the thing.\n");
    let path = repo.root.join(format!(".{id}-doc.md"));
    std::fs::write(&path, doc).unwrap();

    let args = crate::cli::QueueAddArgs {
        from: vec![path.display().to_string()],
    };
    crate::commands::queue_add(repo, &Pipelines::builtin(), &args, &repo.root, false).unwrap();
    // `group` is set above so the document always validates; a task
    // attributed to no group at all — the shape a replay once left behind
    // — is still simulated below, by clearing it after the fact.
    if stage.is_some() || group.is_none() {
        let mut task = repo.task(id).unwrap();
        if let Some(stage) = stage {
            task.set_stage(stage, None);
        }
        if group.is_none() {
            task.front.group = None;
        }
        task.save().unwrap();
    }
}

pub fn lane(name: &str, cwd: &Path) -> crate::mux::Lane {
    crate::mux::Lane {
        name: name.into(),
        kind: "claude".into(),
        status: crate::mux::LaneStatus::Working,
        pane_id: "w1:p1".into(),
        tab_id: "w1:t1".into(),
        workspace_id: "w1".into(),
        cwd: cwd.to_path_buf(),
    }
}

/// One ledger line, with only the fields a cost reading looks at set.
pub fn banked(task: &str, step: &str, session: &str, cost_usd: Option<f64>) -> crate::usage::Entry {
    crate::usage::Entry {
        ts: "2026-08-12T09:00:00Z".into(),
        task: task.into(),
        plan: None,
        step: step.into(),
        pipeline: "default".into(),
        agent: "claude".into(),
        kind: "claude".into(),
        model: "claude-sonnet-5".into(),
        session: session.into(),
        round: 1,
        wall_s: 0,
        turns: 0,
        tokens: crate::usage::Tokens::default(),
        cost_usd,
        ctx_peak: None,
        version: None,
        commit: None,
        outcome: None,
        run: None,
        trial: None,
        skill: None,
        project: String::new(),
    }
}

/// `n` arrivals, oldest first, each its own task so none of them coalesce,
/// and tagged in their `step` so a test can say which survived.
pub fn arrivals(n: usize) -> VecDeque<RecentEvent> {
    (0..n)
        .map(|i| RecentEvent::Arrival {
            at: format!("10:0{i}"),
            id: format!("task-{i}"),
            step: format!("note {i}"),
            verdict: Verdict::None,
            position: None,
        })
        .collect()
}

pub fn row(id: &str) -> Row {
    Row {
        id: id.into(),
        group: Some("demo".into()),
        issue_url: None,
        parallel: false,
        stage: "implement".into(),
        step_loop: None,
        pipeline: "default".into(),
        state: State::Queued,
        depth: 0,
        steps_left: 0,
        dependents: 0,
        ctx: None,
        out: None,
        cost: None,
        lane_time: None,
        unbanked: Unbanked::default(),
        next: String::new(),
        resumable: false,
    }
}

/// A real headless lane for `login · implement`, running a script that
/// sleeps — what a test needs to press keys against something actually
/// live. Hands back the backend and the lane's name.
#[cfg(unix)]
pub fn live_headless_lane(repo: &Repo) -> (Box<dyn crate::mux::Mux>, String) {
    let bin = repo.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("pi");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    std::process::Command::new("chmod")
        .args(["+x", &script.display().to_string()])
        .status()
        .unwrap();

    let mux = crate::mux::backend(repo);
    let pane = mux.create_pane(&repo.root, "login").unwrap();
    let name = crate::mux::lane_name("implement", "login");
    mux.start_lane(&crate::mux::LaneSpec {
        name: &name,
        label: "implementer",
        kind: "pi",
        pane_id: &pane.pane_id,
        args: &["--session-id".to_string(), "s1".to_string()],
        env: &BTreeMap::new(),
        path_prefix: Some(&bin),
    })
    .unwrap();
    mux.prompt(&name, "go").unwrap();
    (mux, name)
}

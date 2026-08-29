//! Fixtures shared by the command families' tests.

use super::*;

/// A project on a branch of its own. Real git, because queueing now reads
/// the branch of the checkout it was run in to decide a task's base.
pub fn fixture(name: &str) -> Repo {
    let root = crate::scratch::root(&format!("commands-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    crate::scratch::git_init(&root, &["-b", "plan/demo"]);
    // A scratch home beside the checkout rather than the real
    // `~/.spoolway/<basename>/` — every test writes its queue, archive and
    // plans here through `Repo`'s own accessors, which create it on demand.
    let home = root.join(".home");
    Repo {
        checkout: root.clone(),
        root,
        config: Config::default(),
        home,
    }
}

pub fn add(repo: &Repo, id: &str, depends_on: &[&str]) {
    let mut frontmatter = format!("id: {id}\ntitle: {id}, done\ngroup: demo\n");
    if !depends_on.is_empty() {
        frontmatter += &format!("depends_on: [{}]\n", depends_on.join(", "));
    }
    let doc = format!("---\n{frontmatter}---\n## Goal\n\nDo the thing.\n");

    let path = repo.root.join(format!(".{id}-doc.md"));
    std::fs::write(&path, doc).unwrap_or_else(|e| panic!("writing {id}'s document: {e:#}"));

    let args = QueueAddArgs {
        from: vec![path.display().to_string()],
    };
    queue_add(repo, &Pipelines::builtin(), &args, &repo.root, false)
        .unwrap_or_else(|e| panic!("queueing {id}: {e:#}"));
}

pub fn queued(repo: &Repo, id: &str) -> Task {
    repo.task(id).unwrap()
}

pub fn lane(cost: Option<f64>) -> crate::usage::Entry {
    crate::usage::Entry {
        ts: "2026-08-04T07:00:00+00:00".into(),
        task: "login".into(),
        plan: None,
        step: "review".into(),
        pipeline: "default".into(),
        agent: "claude".into(),
        kind: "claude".into(),
        model: "claude-opus-5".into(),
        session: "s".into(),
        round: 1,
        wall_s: 60,
        turns: 1,
        // A lane that really ran, not a zero-token enrolment line — a caller
        // testing `cost_usd: None` almost always wants "an unpriced model",
        // which a zero-token line is not. See `Totals::add`'s own comment on
        // why the two must never be conflated.
        tokens: crate::usage::Tokens {
            input: 100,
            output: 200,
            ..Default::default()
        },
        cost_usd: cost,
        ctx_peak: None,
        version: Some("3f9a1c04".into()),
        commit: Some("a91c33e".into()),
        outcome: Some("pass".into()),
        run: None,
        trial: None,
        skill: None,
        project: "demo".into(),
    }
}

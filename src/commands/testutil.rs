//! Fixtures shared by the command families' tests.

use super::*;

/// A project on a branch of its own. Real git, because `check_document_base`
/// looks a task's `base:` up against the repository's own local branches —
/// `refs/heads/plan/demo` included, which needs a commit to exist at all: an
/// unborn branch has no ref for `check_document_base`'s `rev-parse` to find.
pub fn fixture(name: &str) -> Repo {
    let root = crate::scratch::root(&format!("commands-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    crate::scratch::git_init(&root, &["-b", "plan/demo"]);
    crate::repo::run(
        &root,
        "git",
        &["commit", "-q", "--allow-empty", "-m", "seed"],
    )
    .unwrap();
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
    let mut frontmatter = format!("id: {id}\ntitle: {id}, done\ngroup: demo\npipeline: default\n");
    if !depends_on.is_empty() {
        frontmatter += &format!("depends_on: [{}]\n", depends_on.join(", "));
    }
    let doc = format!("---\n{frontmatter}---\n## Goal\n\nDo the thing.\n");

    let path = repo.root.join(format!(".{id}-doc.md"));
    std::fs::write(&path, doc).unwrap_or_else(|e| panic!("writing {id}'s document: {e:#}"));

    // `--base plan/demo` — [`fixture`]'s own checkout branch — so a caller
    // needs no opinion of its own about a base to get a task queued.
    let args = QueueAddArgs {
        from: vec![path.display().to_string()],
        base: Some("plan/demo".to_string()),
        dry_run: false,
    };
    queue_add(repo, &Pipelines::builtin(), &args, &repo.root, false)
        .unwrap_or_else(|e| panic!("queueing {id}: {e:#}"));
}

pub fn queued(repo: &Repo, id: &str) -> Task {
    repo.task(id).unwrap()
}

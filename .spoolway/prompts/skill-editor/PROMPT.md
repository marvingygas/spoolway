# skill-editor

You change a shipped skill, as the task asks. You are done when
every copy says the new thing and the installed copies match the shipped ones.
On a fix visit, clear every finding the review raised, and nothing else.

## Domain knowledge

- The shipped skills live in `assets/skills/<provider>/<skill>/`, one folder
  per provider: `claude`, `codex` and `pi`. `src/install.rs` builds them into
  the binary. Edit these files.
- `.claude/skills/` and `.agents/skills/` are installed copies. Never edit
  them by hand. Refresh them after your edit:
  `cargo build --release --locked`, then
  `./target/release/spoolway install claude --force` and
  `./target/release/spoolway install codex --force`.
- An edit made only to an installed copy is lost: `spoolway init` writes the
  shipped text back over it, and CI fails on the diff.
- The three copies say the same thing. They differ only where they ask the
  person a question: `claude` names `AskUserQuestion`, `codex` names
  `request_user_input`, and `pi` prints the question and ends the turn.
  Carry every change into all three, and keep that one difference.
- Plan assets ship from `assets/skills/claude/<skill>/assets/` only.
- Write new text in plain English: one idea per sentence, short common
  words, plain verbs such as "reads", "writes" and "moves". No jargon and
  no filler.
- `cargo test --locked install::` checks the installed copies against the
  shipped ones, and the skill size limits.

## Never

- Never rewrite lines the task does not touch.
- Never change a skill that lives only in `.claude/skills/`, such as
  `create-pr`. It has no shipped copy, and it is out of scope here.

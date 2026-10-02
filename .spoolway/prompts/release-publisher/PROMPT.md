# Release recovery

## What you are looking at

Recover from a failed `publish` or `released` command step. `scripts/release-publish.sh` rehearses
the release commit, tags it and watches the tag's own workflow; `scripts/release-verify.sh` proves
the tag, the six packages, the archives and the published body. You run only when one of those two
exited non-zero — read its log under the task's own step output to see exactly which script it was
and why, then read `docs/releasing.md` in full and the readiness through candidate handoffs. Those
steps authorize exactly the recorded source commit, version and notes — nothing newer, and nothing
you may retag.

You never tag or push `refs/tags/v<version>` yourself: a pass sends the task back to `publish`,
whose script does it. The script is idempotent — with the tag already on `origin` it only re-checks
the tag's own workflow run and hands on to `released`; without it, it rehearses, tags and pushes. You
have three outcomes. Pass once what stopped the script is cleared, so its next run can finish. Fail
to `version` once nothing here can be finished and the untagged release commit is reverted off
`main`, so the next cycle starts clean. Block when only a person can take the next step, and name
that step on the board.

## How to do it here

1. Work from the source checkout path under WHAT YOU HAVE. Fetch and read exactly which script failed
   and why: the rehearsal was red, the tag push itself failed with nothing on `origin` (no
   `refs/tags/v<version>` pushed), main moved before the tag, the tag's own workflow run failed after
   the push, or the post-publication verifier found a package, asset or release body missing or wrong
   on an otherwise tagged release.
2. **A tag push that failed for a reason outside the commit** — credentials, the network, a hosted
   outage — with the rehearsal green and `origin/main` still at the release commit. Clear the cause
   and pass; `publish` reuses the local tag and pushes it. Everything below in this rule is for a
   red rehearsal, where the commit itself cannot ship.

   **A red rehearsal with nothing tagged on `origin` yet.** By now `await-release` has
   already seen the release commit merged, so `main` carries an untagged `chore(release): v<version>`
   commit with the bumped version files and `CHANGELOG.md` section. There is no in-place repair: that
   commit has a fixed parent, and only `scripts/release-publish.sh` can rehearse and tag, which runs
   again only after a fresh `candidate`. Diagnose the cause as far as you can. Then remove the release
   commit exactly as rule 4 does: one revert pull request, a person's merge, the version files and
   `CHANGELOG.md` and `docs/migrations.md` confirmed back at the previous release, scratch
   `release-notes.md` and `migrations.md` deleted. Then fail
   to `version` with the full diagnosis. Say plainly whether the cause is a defect on `main`: the next
   cycle goes `version` → `preflight`, not through readiness, so a real defect has to be named for
   `preflight` to fail it to `fix`.
3. **A partial publish** — `refs/tags/v<version>` is already on `origin`, but the tag workflow left some
   packages, assets or the release unpublished. Read the real registry and release-page state directly;
   never trust the failed run's own report of what it reached. Preserve every package already
   published — never republish or replace a version already on the registry. Retry with
   `gh run rerun <run-id> --failed` where the cause was transient. If the workflow itself needs a code
   fix, record the old and new SHAs and the reason, land the fix through a reviewed pull request the
   same way `release-fixer` would (on a branch of its own, checks green), push it, and wait for a person
   to merge it yourself with `scripts/release-await-merge.sh <branch>` — never merge it. A workflow fix
   is the one case that may need the tag moved to a new commit. If it does, block: never pass while the
   tag sits on the abandoned commit. On the board, say that a person moves `v<version>` from the
   abandoned commit to the corrected one (`docs/releasing.md` has the manual tagging steps), name both
   SHAs and the reason, and that `recover` confirms the rest once they resume it. Once the tag is on the
   right commit and the registry and release page are complete and correct, pass: `publish` re-checks
   the tag's run and `released` re-reads the public state.
4. **Main moved before the tag.** `scripts/release-publish.sh` refuses to tag an older commit once
   `origin/main` has moved past the recorded release commit. Do not fold the new commits into it,
   force-push, or rewrite `main`. Open one revert pull request that removes only the release
   commit, require its protected checks, review its final diff, push it, and wait for a person to merge
   it yourself with `scripts/release-await-merge.sh <branch>` — never merge it. That wait can last hours
   and a foreground command is capped at 10 minutes, so run it in the background and poll it. Once
   merged, verify both version files, `CHANGELOG.md` and `docs/migrations.md` are back at the previous
   release, delete scratch `release-notes.md` and `migrations.md`, and fail this step so the task
   returns to `version` with a fresh candidate to choose, naming both the abandoned release SHA and
   the new main SHA. If the revert conflicts, its checks cannot go green, or a person closes it
   unmerged, that exact unreconciled state is a genuine block.
5. **The post-publication verifier failed on an otherwise complete publish.** A missing asset, a stale
   published body, or a registry read that is merely flaky is not the same as a partial publish; retry
   the bounded transient failure once, and if the gap is real, treat it as a partial publish under rule 3.
6. Report which script failed, the exact cause, every command you ran and its result, any pull request
   you opened and its merge, and whether you pass, fail to `version` or block, and why.

## Never

- Never create, push, or move a release tag yourself, however the recovery goes — that action belongs
  to `scripts/release-publish.sh` alone, or is left for a person when a workflow fix must move it.
- Never merge a pull request yourself. Push it, then wait for a person with
  `scripts/release-await-merge.sh`.
- Never republish or replace a package version already on the registry, or hand-edit a published
  release body or its assets.
- Never fold new `main` commits into an abandoned release commit, force-push `main`, or rewrite history.
- Never pass while the release commit cannot be tagged — `main` moved past it, or its rehearsal is
  red. `publish` would only refuse it again.
- Never treat an unavailable credential or GitHub outage as permission to bypass protection or skip
  verification. Retry a bounded transient failure once; block on the exact external condition when it
  remains real.

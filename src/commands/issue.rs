//! `spoolway issue show` — read one issue out of a project's own tracker,
//! through the `[issue_tracking]` hook's `fetch` event, and print it as JSON.
//! Writes nothing: this is a thin CLI wrapper over
//! [`crate::tracking::fetch_issue`], the synchronous run that does the whole
//! job, the same way `queue add` is a thin caller over `open_ticket`.

use super::*;

/// `spoolway issue show <reference>`. Prints one JSON object on stdout and
/// nothing else on success; every failure — no hook configured, a hook with
/// no `fetch` branch, or the hook itself failing — is a plain error naming
/// which of the three it was.
pub fn issue_show(repo: &Repo, reference: &str) -> Result<()> {
    match crate::tracking::fetch_issue(repo, reference)? {
        crate::tracking::FetchResult::NoHook => bail!(
            "no `[issue_tracking]` hook is configured in {} — set `hook` before an issue can \
             be read out of a tracker.",
            Config::path_in(&repo.checkout).display()
        ),
        crate::tracking::FetchResult::NoFetchBranch => bail!(
            "the hook script `{}` has no `fetch` branch — see `spoolway doctor` for what to \
             add.",
            repo.config.issue_tracking.hook.trim()
        ),
        crate::tracking::FetchResult::Answered(raw) => {
            // Parsed and re-printed rather than passed through verbatim: a
            // hook that wrote something that is not one JSON object failed
            // just as surely as one that exited non-zero, and this is where
            // that is caught — spoolway parses the *shape* of what a hook
            // wrote here, never a tracker's own ref, which stays the
            // script's job throughout. `serde_json`'s `preserve_order`
            // feature is what keeps this reprint in the hook's own key
            // order rather than alphabetising it — without it `Value`'s map
            // is a `BTreeMap`, and every shipped hook's `ref, url, title,
            // state, labels, body, comments` would print `body, comments,
            // labels, ref, state, title, url` instead.
            let value: serde_json::Value = serde_json::from_str(&raw).with_context(|| {
                format!(
                    "the hook wrote something that is not valid JSON to `SPOOLWAY_OUT` for \
                     `{reference}`: {raw:?}"
                )
            })?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        crate::tracking::FetchResult::Failed { exit_code } => {
            let code = exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "no code".to_string());
            bail!(
                "the fetch hook exited {code} for `{reference}` — nothing was read. The log is \
                 in {}.",
                repo.tracking_dir().display()
            );
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Repo {
        let root = crate::scratch::root(&format!("issue-show-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".spoolway/hooks")).unwrap();
        Repo {
            checkout: root.clone(),
            root: root.clone(),
            config: Config::default(),
            home: root.join(".home"),
        }
    }

    fn with_hook(repo: &mut Repo, name: &str, script: &str) {
        let path = repo.checkout.join(".spoolway/hooks").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        repo.config.issue_tracking.hook = name.to_string();
    }

    /// No hook at all names which of the two failures this is.
    #[test]
    fn no_hook_is_refused_by_name() {
        let repo = fixture("no-hook");
        let err = issue_show(&repo, "57").unwrap_err();
        assert!(
            format!("{err:#}").contains("no `[issue_tracking]` hook is configured"),
            "{err:#}"
        );
    }

    /// A hook with no `fetch` branch is refused by name too, naming the
    /// script rather than failing on a mysterious empty answer.
    #[test]
    fn no_fetch_branch_is_refused_by_name() {
        let mut repo = fixture("no-fetch");
        with_hook(&mut repo, "old.sh", "exit 0");
        let err = issue_show(&repo, "57").unwrap_err();
        assert!(
            format!("{err:#}").contains("`old.sh` has no `fetch` branch"),
            "{err:#}"
        );
    }

    /// A hook that answers with valid JSON has it printed back, pretty and
    /// on its own — parsed and re-serialised, not merely echoed, so a hook
    /// that wrote garbage is caught here rather than left to whoever reads
    /// stdout next.
    #[test]
    fn a_clean_answer_is_printed_as_json() {
        let mut repo = fixture("clean");
        with_hook(
            &mut repo,
            "fetch.sh",
            r#"if [ "$SPOOLWAY_EVENT" = fetch ]; then
                 printf '{"ref":"57","title":"Rework the session store"}' > "$SPOOLWAY_OUT"
                 exit 0
               fi"#,
        );

        // Capturing stdout would need a subprocess; this project's own
        // `command_step` tests exercise the underlying run instead, so this
        // only checks the call succeeds and does not error on valid JSON —
        // the parse itself is exercised directly below.
        issue_show(&repo, "57").unwrap();
    }

    /// A hook that writes something that is not JSON at all is refused, the
    /// same as a bad exit code — nothing not shaped right ever reaches
    /// stdout.
    #[test]
    fn a_non_json_answer_is_refused() {
        let mut repo = fixture("garbage");
        with_hook(
            &mut repo,
            "fetch.sh",
            r#"if [ "$SPOOLWAY_EVENT" = fetch ]; then
                 printf 'not json' > "$SPOOLWAY_OUT"
                 exit 0
               fi"#,
        );
        let err = issue_show(&repo, "57").unwrap_err();
        assert!(format!("{err:#}").contains("not valid JSON"), "{err:#}");
    }

    /// The parse-and-reprint in `issue_show` above goes through
    /// `serde_json::Value`, whose map only keeps a hook's own key order
    /// because `Cargo.toml` turns on `preserve_order` — a feature flag nothing
    /// else pins. Dropping it would silently alphabetise every hook's answer
    /// with no test failing, since the tests above check field values, not
    /// their order. This one would.
    #[test]
    fn value_round_trip_keeps_the_hook_s_own_key_order() {
        let raw = r#"{"ref":"57","url":"u","title":"t","state":"open","labels":[],"body":"b","comments":[]}"#;
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), raw);
    }
}

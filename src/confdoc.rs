//! Editing `config.toml` as a document rather than as a struct.
//!
//! Serialising a [`crate::config::Config`] answers "what would this file say if
//! it were written today", which is the wrong question for a file that already
//! exists: a struct holds no comments, no key order and no blank lines, so
//! writing one back replaces every one of those with whatever the binary
//! happens to say. That was how `config set` used to write, and it meant
//! setting an interval also silently materialised defaults, dropped retired
//! keys, and overwrote a comment a person had written by hand — a migration
//! performed as a side effect of an unrelated edit, at a moment nobody was
//! being asked about a migration.
//!
//! So the two edits are split. [`set`] changes one key here, and every other
//! byte of the file is copied through unread — that is what `config set` does,
//! and setting a value is never the moment to migrate anything.
//!
//! Bringing a whole file forward is `spoolway update`, and there the answer is
//! the opposite one: it rewrites the file from [`crate::config::Config::render`]
//! and keeps only what a person set. **Every comment in `config.toml` is
//! spoolway's**, which is what makes the file's explanations trustworthy — a
//! note that could be a stale copy of one from three releases ago, or somebody
//! else's sentence in the same position, explains nothing. Prose about a
//! project's own choices belongs where it is read: a prompt, a pipeline, or
//! project documentation. What [`compare`] is for is saying afterwards what that rewrite
//! actually did, key by key, so it can be read rather than diffed.

use anyhow::{Context, Result};
use toml::Value;
use toml_edit::{DocumentMut, Item, Table};

use crate::config::comment_block;

/// Read a dotted path out of a serialised config.
pub fn at<'v>(value: &'v Value, parts: &[&str]) -> Option<&'v Value> {
    let mut cursor = value;
    for part in parts {
        cursor = cursor.get(part)?;
    }
    Some(cursor)
}

/// Set one key in the document, leaving every other byte where it was.
///
/// A key already in the file keeps its comment: what is being changed is a
/// value, and the note above it is as true afterwards as it was before. A key
/// the file never had is added with its note, because a setting that appears
/// without its explanation is the one thing this file is meant never to have.
pub fn set(text: &str, parts: &[&str], value: &Value, note: Option<&str>) -> Result<String> {
    let mut doc: DocumentMut = text.parse().context("this file is not valid TOML")?;
    let (leaf, path) = parts.split_last().context("a config key cannot be empty")?;

    let mut table = doc.as_table_mut();
    for part in path {
        table = table
            .entry(part)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .with_context(|| format!("`{part}` is not a table"))?;
    }

    // Read before the write, because inserting replaces the whole entry —
    // the key, its decor and the comment standing in that decor included.
    let existing = table
        .key(leaf)
        .and_then(|key| key.leaf_decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .map(str::to_string);
    let had_it = table.contains_key(leaf);

    table.insert(leaf, item(value)?);

    let prefix = match had_it {
        true => existing,
        false => note.map(comment_block),
    };
    if let Some(prefix) = prefix
        && let Some(mut key) = table.key_mut(leaf)
    {
        key.leaf_decor_mut().set_prefix(prefix);
    }

    Ok(doc.to_string())
}

/// Take one key out of the document, leaving every other byte where it was —
/// its comment with it, since a note explaining a key that is no longer there
/// is the same noise the key was.
///
/// The counterpart to [`set`] for a value whose *unset* form is its absence:
/// `agents.<profile>.concurrency` and every zero in `[models]` are omitted
/// rather than written, so setting one back to zero has to delete the line and
/// not write `= 0`. Removing a key the file does not have is not an error —
/// the document already says what it was asked to say.
pub fn remove(text: &str, parts: &[&str]) -> Result<String> {
    let mut doc: DocumentMut = text.parse().context("this file is not valid TOML")?;
    let (leaf, path) = parts.split_last().context("a config key cannot be empty")?;

    let mut table = doc.as_table_mut();
    for part in path {
        let Some(next) = table.get_mut(part).and_then(Item::as_table_mut) else {
            return Ok(doc.to_string());
        };
        table = next;
    }
    table.remove(leaf);
    Ok(doc.to_string())
}

/// What rewriting a config file did to it, key by key.
///
/// A rewrite is one blunt act — the whole file, from the struct — so what it
/// did is not visible in the act itself. This reads it back out of the two
/// documents, so `update` can say "these five settings are new, this one is
/// gone" rather than only "written", and a person can decide whether the diff
/// is worth opening.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Refresh {
    /// Settings this spoolway has that the file did not.
    pub added: Vec<String>,
    /// Settings the file had that this spoolway no longer knows — the retired
    /// keys, kept parseable only so an old file still loads on the way here.
    pub dropped: Vec<String>,
    /// Settings whose explanation above them changed: a note the binary
    /// reworded, one that was never written down, or a sentence somebody put
    /// there by hand. All three read the same from here and all three are
    /// replaced, because every comment in this file is spoolway's.
    pub renoted: Vec<String>,
}

impl Refresh {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.dropped.is_empty() && self.renoted.is_empty()
    }
}

/// Read what changed between a config file and its rewritten self.
///
/// Values are not compared, because nothing in a rewrite may change one: what
/// is rendered is the config loaded from this very file, so every value in
/// `after` came out of `before`. [`crate::config::Config::agrees_with`] is what
/// proves that; this only describes the rest.
pub fn compare(before: &str, after: &str) -> Result<Refresh> {
    let old: DocumentMut = before.parse().context("this file is not valid TOML")?;
    let new: DocumentMut = after
        .parse()
        .context("the rewritten config is not valid TOML")?;

    let mut was = Vec::new();
    leaves_of_doc(old.as_table(), &mut Vec::new(), &mut was);
    let mut is = Vec::new();
    leaves_of_doc(new.as_table(), &mut Vec::new(), &mut is);

    let dotted = |path: &Vec<String>| path.join(".");
    let mut refresh = Refresh {
        added: is.iter().filter(|p| !was.contains(p)).map(dotted).collect(),
        dropped: was.iter().filter(|p| !is.contains(p)).map(dotted).collect(),
        renoted: Vec::new(),
    };

    for path in was.iter().filter(|path| is.contains(path)) {
        let parts: Vec<&str> = path.iter().map(String::as_str).collect();
        if comment_above(&old, &parts) != comment_above(&new, &parts) {
            refresh.renoted.push(dotted(path));
        }
    }

    Ok(refresh)
}

/// The same walk over the document, so the two lists compare directly.
fn leaves_of_doc(table: &Table, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    for (key, item) in table.iter() {
        path.push(key.to_string());
        match item {
            Item::Table(child) if !child.is_empty() => leaves_of_doc(child, path, out),
            _ => out.push(path.clone()),
        }
        path.pop();
    }
}

/// The comment standing above a key, as one line of words.
///
/// Flattened rather than compared line for line, so a note that is only
/// re-wrapped still reads as ours — the wrap width is this binary's business,
/// not a person's edit.
fn comment_above(doc: &DocumentMut, parts: &[&str]) -> Option<String> {
    let mut table = doc.as_table();
    for part in &parts[..parts.len() - 1] {
        table = table.get(part)?.as_table()?;
    }
    let prefix = table
        .key(parts.last()?)?
        .leaf_decor()
        .prefix()?
        .as_str()?
        .to_string();

    let text: Vec<&str> = prefix
        .lines()
        .filter_map(|line| line.trim().strip_prefix('#'))
        .map(str::trim)
        .collect();
    (!text.is_empty()).then(|| text.join(" "))
}

/// A serialised value as something the document can hold.
fn item(value: &Value) -> Result<Item> {
    Ok(match value {
        Value::Table(table) => {
            let mut out = Table::new();
            for (key, child) in table {
                out.insert(key, item(child)?);
            }
            Item::Table(out)
        }
        scalar => Item::Value(scalar_item(scalar)?),
    })
}

fn scalar_item(value: &Value) -> Result<toml_edit::Value> {
    Ok(match value {
        Value::String(text) => text.as_str().into(),
        Value::Integer(number) => (*number).into(),
        Value::Float(number) => (*number).into(),
        Value::Boolean(flag) => (*flag).into(),
        Value::Datetime(stamp) => stamp.to_string().into(),
        Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for child in items {
                array.push(scalar_item(child)?);
            }
            array.into()
        }
        Value::Table(table) => {
            let mut inline = toml_edit::InlineTable::new();
            for (key, child) in table {
                inline.insert(key, scalar_item(child)?);
            }
            inline.into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Setting a key back to the value whose spelling is its absence takes the
    /// line out, comment and all, and leaves the rest of the document alone.
    ///
    /// The bug this pins: `save_key` looked the new value up in the serialised
    /// config and failed with "not in the config it was just set in" when that
    /// value was the omitted one, so `concurrency` and every `[models]` rate
    /// could be set to something but never back to nothing.
    #[test]
    fn removing_a_key_takes_its_comment_with_it() {
        let text = "\
[agents.pi]
kind = \"pi\"
# How many lanes this profile may run at once.
concurrency = 4
session_reuse_ctx = 50
";
        let edited = remove(text, &["agents", "pi", "concurrency"]).unwrap();

        assert_eq!(
            edited,
            "\
[agents.pi]
kind = \"pi\"
session_reuse_ctx = 50
"
        );

        // Removing what is not there is not an error: the document already
        // says what it was asked to say.
        assert_eq!(
            remove(&edited, &["agents", "pi", "concurrency"]).unwrap(),
            edited
        );
        assert_eq!(
            remove(&edited, &["agents", "nope", "concurrency"]).unwrap(),
            edited
        );
    }

    #[test]
    fn setting_a_key_leaves_every_other_byte_alone() {
        let text = "\
[dispatch]
# a comment somebody wrote about this
interval = \"1m\"

# and one about the next thing
auto_commit = true
";
        let edited = set(
            text,
            &["dispatch", "interval"],
            &Value::String("30s".into()),
            crate::confkv::note("dispatch.interval"),
        )
        .unwrap();

        assert_eq!(
            edited,
            "\
[dispatch]
# a comment somebody wrote about this
interval = \"30s\"

# and one about the next thing
auto_commit = true
"
        );
    }

    #[test]
    fn a_key_the_file_never_had_arrives_with_its_note() {
        let text = "[dispatch]\ninterval = \"10s\"\n";
        let edited = set(
            text,
            &["dispatch", "auto_commit"],
            &Value::Boolean(false),
            crate::confkv::note("dispatch.auto_commit"),
        )
        .unwrap();

        assert!(edited.contains("auto_commit = false"));
        assert!(edited.contains("# Whether spoolway commits"));
        assert!(edited.starts_with("[dispatch]\ninterval = \"10s\"\n"));
    }

    /// What `update` reads back out of a rewrite, so its report is the rewrite's
    /// own account of itself rather than a guess made before it ran.
    #[test]
    fn compare_names_the_settings_a_rewrite_added() {
        let before = "[dispatch]\ninterval = \"10s\"\n";
        let after = Config::default().render().unwrap();

        let refresh = compare(before, &after).unwrap();

        assert!(refresh.added.contains(&"dispatch.auto_commit".to_string()));
        assert!(
            refresh
                .added
                .contains(&"agents.claude.permission_mode".to_string())
        );
        assert!(refresh.dropped.is_empty());
    }

    #[test]
    fn compare_names_the_retired_settings_a_rewrite_dropped() {
        let mut before = Config::default().render().unwrap();
        before.push_str("\n[sandbox]\nenabled = true\n");
        let after = Config::default().render().unwrap();

        let refresh = compare(&before, &after).unwrap();

        assert_eq!(refresh.dropped, vec!["sandbox.enabled".to_string()]);
        assert!(refresh.added.is_empty());
    }

    /// A comment is spoolway's, so one that has fallen behind the binary and one
    /// somebody wrote by hand are the same thing from here: both are named, and
    /// both are replaced by the rewrite this is describing.
    #[test]
    fn compare_names_a_comment_that_is_not_the_one_the_binary_writes() {
        let before = "\
[dispatch]
# Ten seconds because our lanes are quick and we like the board fresh.
interval = \"10s\"
";
        let after = Config::default().render().unwrap();

        let refresh = compare(before, &after).unwrap();

        assert!(refresh.renoted.contains(&"dispatch.interval".to_string()));
    }

    /// The wrap width is the binary's business. A note re-wrapped and nothing
    /// else is the same note, and saying otherwise would report a change to
    /// every key every time the width moved.
    #[test]
    fn a_note_that_is_only_rewrapped_is_the_same_note() {
        let note = crate::confkv::note("dispatch.auto_commit").unwrap();
        let before = format!("[dispatch]\n# {note}\nauto_commit = true\n");
        let after = "[dispatch]\n".to_string()
            + &crate::config::comment_block(note)
            + "auto_commit = true\n";

        let refresh = compare(&before, &after).unwrap();

        assert!(refresh.is_empty(), "{refresh:?}");
    }

    /// The whole point of the rewrite: values are the one thing carried across.
    #[test]
    fn a_rewritten_config_still_holds_what_the_project_set() {
        let mut text = Config::default().render().unwrap();
        text = text.replace("interval = \"10s\"", "interval = \"45s\"");
        text.push_str("\n[sandbox]\nenabled = true\n");

        let mut current: Config = toml::from_str(&text).unwrap();
        current.dispatch.auto_commit = false;
        let rewritten = current.render().unwrap();
        current.agrees_with(&rewritten).unwrap();

        assert!(rewritten.contains("interval = \"45s\""));
        assert!(rewritten.contains("auto_commit = false"));
        assert!(!rewritten.contains("[sandbox]"));
    }
}

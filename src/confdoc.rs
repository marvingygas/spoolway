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
//! So the two edits are split. [`set`] changes one key here, and the rest of
//! the file is copied through unread — that is what `config set` does, and
//! setting a value is never the moment to migrate anything. The exception is a
//! key the file never had: it is written where the rendering would put it, so
//! the keys after it move down and a table header the rendering would not
//! write is dropped.
//!
//! Bringing a whole file forward is `spoolway sync`, and there the answer is
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

/// Read a dotted path out of a serialised config.
pub fn at<'v>(value: &'v Value, parts: &[&str]) -> Option<&'v Value> {
    let mut cursor = value;
    for part in parts {
        cursor = cursor.get(part)?;
    }
    Some(cursor)
}

/// Set one key in the document, leaving every other byte where it was except
/// where a key the file never had is placed, as described below.
///
/// A key already in the file keeps its comment: what is being changed is a
/// value, and the note above it is as true afterwards as it was before.
///
/// A key the file never had is written where `rendered` — the config as
/// [`crate::config::Config::render`] writes it — would have put it: after the
/// keys that come before it there, and under whatever comment `rendered`
/// stands above it, which is none for nearly every key. `spoolway sync`
/// rewrites a file from that same rendering, so a key added anywhere else, or
/// with a note the rendering does not carry, would leave a file `sync` and
/// `doctor` call behind although nothing in it is out of date.
pub fn set(text: &str, parts: &[&str], value: &Value, rendered: &str) -> Result<String> {
    let mut doc: DocumentMut = text.parse().context("this file is not valid TOML")?;
    let template: DocumentMut = rendered
        .parse()
        .context("the rendered config is not valid TOML")?;
    let (leaf, path) = parts.split_last().context("a config key cannot be empty")?;

    // Tables that held nothing before this key arrived: a rendering writes no
    // header for a table that holds only tables, so once one of these has a
    // table inside it, its bare header is the line `sync` would remove.
    let mut emptied = Vec::new();
    // The first table on the path the file did not have; every one below it is
    // new as well.
    let mut created_from = None;
    let mut table = doc.as_table_mut();
    for (depth, part) in path.iter().enumerate() {
        if created_from.is_none() && !table.contains_key(part) {
            created_from = Some(depth);
        }
        table = table
            .entry(part)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .with_context(|| format!("`{part}` is not a table"))?;
        if table.is_empty() {
            emptied.push(depth);
        }
    }

    // Read before the write, because inserting replaces the whole entry —
    // the key, its decor and the comment standing in that decor included.
    let existing = table
        .key(leaf)
        .and_then(|key| key.leaf_decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .map(str::to_string);
    let had_it = table.contains_key(leaf);

    if had_it {
        table.insert(leaf, item(value)?);
        if let Some(prefix) = existing
            && let Some(mut key) = table.key_mut(leaf)
        {
            key.leaf_decor_mut().set_prefix(prefix);
        }
    } else {
        let template = path.iter().try_fold(template.as_table(), |table, part| {
            table.get(part).and_then(Item::as_table)
        });
        insert_in_render_order(table, leaf, item(value)?, template);
    }

    // Walked again from the top: the borrow above ends with the write.
    for depth in emptied {
        let mut table = Some(doc.as_table_mut());
        for part in &path[..=depth] {
            table = table
                .and_then(|table| table.get_mut(part))
                .and_then(Item::as_table_mut);
        }
        let Some(table) = table else { continue };
        let has_comment = table
            .decor()
            .prefix()
            .and_then(|prefix| prefix.as_str())
            .is_some_and(|prefix| prefix.contains('#'));
        if !has_comment && table.iter().all(|(_, item)| !item.is_value()) {
            table.set_implicit(true);
        }
    }

    // Never a top-level table: those are the config's own sections, and a file
    // that lacks one is already behind a `sync` that would add it, so the
    // bytes above a one-key edit stay where they were.
    if let Some(from) = created_from.filter(|from| *from > 0) {
        place_new_tables(&mut doc, &template, path, from);
    }

    Ok(doc.to_string())
}

/// Stand the tables `set` just created, `path[from..]`, where the rendering
/// lists them among their sibling tables.
///
/// A document prints its tables in the order of their positions, and a table
/// with none is printed after the ones before it, so a new `[models."aaa"]`
/// would follow `[models."foo"]` where the rendering, which sorts its rows,
/// writes it first. The new tables are given the position of the first table
/// that follows them in the rendering and that the file has, and every table
/// from there on moves up to make room. With no such sibling the tables keep
/// the place they were created in, which is last.
fn place_new_tables(doc: &mut DocumentMut, template: &DocumentMut, path: &[&str], from: usize) {
    let Some(parent) = path[..from]
        .iter()
        .try_fold(template.as_table(), |table, part| {
            table.get(part).and_then(Item::as_table)
        })
    else {
        return;
    };
    let siblings: Vec<&str> = parent
        .iter()
        .filter(|(_, item)| item.is_table())
        .map(|(key, _)| key)
        .collect();
    let Some(at) = siblings.iter().position(|key| *key == path[from]) else {
        return;
    };
    let Some(file_parent) = path[..from].iter().try_fold(doc.as_table(), |table, part| {
        table.get(part).and_then(Item::as_table)
    }) else {
        return;
    };
    let Some(first) = siblings[at + 1..].iter().find_map(|key| {
        file_parent
            .get(key)
            .and_then(Item::as_table)
            .and_then(first_position)
    }) else {
        return;
    };

    shift_positions(doc.as_table_mut(), first, (path.len() - from) as isize);
    for (offset, depth) in (from..path.len()).enumerate() {
        let mut table = Some(doc.as_table_mut());
        for part in &path[..=depth] {
            table = table
                .and_then(|table| table.get_mut(part))
                .and_then(Item::as_table_mut);
        }
        if let Some(table) = table {
            table.set_position(Some(first + offset as isize));
        }
    }
}

/// Where `table` first appears in the printed document.
///
/// A table written only to hold other tables, as `[models."bbb"]` is when the
/// file has just `[models."bbb".above_100k_tokens]`, is implicit and has no
/// position of its own. It prints where its first descendant does, so that is
/// the place a table meant to stand before it must take.
fn first_position(table: &Table) -> Option<isize> {
    table.position().or_else(|| {
        table
            .iter()
            .filter_map(|(_, item)| item.as_table())
            .filter_map(first_position)
            .min()
    })
}

/// Move every table at position `from` or after it `by` places later.
fn shift_positions(table: &mut Table, from: isize, by: isize) {
    if let Some(position) = table.position()
        && position >= from
    {
        table.set_position(Some(position + by));
    }
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(child) => shift_positions(child, from, by),
            Item::ArrayOfTables(array) => {
                for child in array.iter_mut() {
                    shift_positions(child, from, by);
                }
            }
            _ => {}
        }
    }
}

/// Add `leaf` to `table` at the place `template`, the same table as the
/// rendering writes it, lists it.
///
/// A rendering writes a struct's fields in field order, and a file that only
/// ever grew by `config set` does not: the new key would land last. The key is
/// put after the last key of the file that the rendering lists before it, and
/// the file's keys that came after that point are written back behind it, with
/// their comments, so the order of everything else is the file's own.
fn insert_in_render_order(table: &mut Table, leaf: &str, value: Item, template: Option<&Table>) {
    let rendered: Vec<&str> = template
        .map(|template| {
            template
                .iter()
                .filter(|(_, item)| item.is_value())
                .map(|(key, _)| key)
                .collect()
        })
        .unwrap_or_default();
    let prefix = template
        .and_then(|template| template.key(leaf))
        .and_then(|key| key.leaf_decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .map(str::to_string);

    let present: Vec<String> = table
        .iter()
        .filter(|(_, item)| item.is_value())
        .map(|(key, _)| key.to_string())
        .collect();
    let before = rendered.iter().position(|key| *key == leaf);
    let anchor = before.and_then(|at| {
        present
            .iter()
            .rposition(|key| rendered[..at].contains(&key.as_str()))
    });
    // A key the rendering has no place for is appended, as it always was.
    let behind: Vec<String> = match (before, anchor) {
        (None, _) => Vec::new(),
        (Some(_), Some(anchor)) => present[anchor + 1..].to_vec(),
        (Some(_), None) => present,
    };

    let moved: Vec<(toml_edit::Key, Item)> = behind
        .iter()
        .filter_map(|key| table.remove_entry(key))
        .collect();
    table.insert(leaf, value);
    if let Some(prefix) = prefix
        && let Some(mut key) = table.key_mut(leaf)
    {
        key.leaf_decor_mut().set_prefix(prefix);
    }
    for (key, item) in moved {
        table.insert_formatted(&key, item);
    }
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

    // Table-like, not `as_table_mut`: that is `None` for an inline table
    // (`issue_tracking = { …, on_fail = "" }`), so a retired key written
    // there was never removed and the file kept failing to load. Removing
    // from an inline table keeps it inline.
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for part in path {
        let Some(next) = table.get_mut(part).and_then(Item::as_table_like_mut) else {
            return Ok(doc.to_string());
        };
        table = next;
    }
    table.remove(leaf);
    Ok(doc.to_string())
}

/// Carry the keys at `paths` from `old` into `new`, each with the comment
/// standing above it.
///
/// For the one rewrite that starts from the struct, `spoolway sync`: a struct
/// holds only what this binary knows, so a key it does not know — a typo, or
/// a setting a newer binary added — is not in what it renders. Every comment
/// in this file is spoolway's, but a comment above a key spoolway has never
/// heard of is the only explanation of it there is, so it travels with the
/// key. The key lands at the end of its table.
///
/// A path under `pricing`, the old spelling of `models`, lands under `models`,
/// because that is the only spelling the rewrite writes.
///
/// A path whose parent table is not in `new` is skipped: its parent was
/// itself dropped, a retired table say, and a key with no table to
/// sit in has nowhere to go. [`compare`] then lists it as dropped.
///
/// Returns the rewritten text and the paths, as they now stand in it, that
/// were actually carried over — so a caller reports what it kept, not what it
/// was asked to keep.
pub fn keep(old: &str, new: &str, paths: &[Vec<String>]) -> Result<(String, Vec<Vec<String>>)> {
    let old: DocumentMut = old.parse().context("this file is not valid TOML")?;
    let mut new: DocumentMut = new
        .parse()
        .context("the rewritten config is not valid TOML")?;

    let mut kept = Vec::new();
    'paths: for path in paths {
        let Some((leaf, parents)) = path.split_last() else {
            continue;
        };
        let mut from: &dyn toml_edit::TableLike = old.as_table();
        let mut found = true;
        for part in parents {
            match from.get(part).and_then(Item::as_table_like) {
                Some(next) => from = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        let (true, Some(item), Some(key)) = (found, from.get(leaf), from.key(leaf)) else {
            continue;
        };
        let mut into: &mut dyn toml_edit::TableLike = new.as_table_mut();
        let mut landed: Vec<String> = parents.to_vec();
        if let Some(first) = landed.first_mut()
            && first == "pricing"
        {
            *first = "models".to_string();
        }
        for part in &landed {
            let Some(next) = into.get_mut(part).and_then(Item::as_table_like_mut) else {
                continue 'paths;
            };
            into = next;
        }
        let prefix = key.leaf_decor().prefix().cloned();
        let mut carried = item.clone();
        unpositioned(&mut carried);
        into.insert(leaf, carried);
        if let (Some(prefix), Some(mut key)) = (prefix, into.key_mut(leaf)) {
            key.leaf_decor_mut().set_prefix(prefix);
        }
        landed.push(leaf.clone());
        kept.push(landed);
    }
    Ok((new.to_string(), kept))
}

/// Forget where a table stood in the document it was read from.
///
/// `toml_edit` writes tables in order of their position, and a parsed table
/// remembers its position in the *old* file. Left alone, a carried table lands
/// wherever that number happens to fall among the rendered tables, which
/// depends on the file `sync` is replacing — so the next sync, reading the new
/// file, would place it somewhere else and rewrite the file again. With none,
/// `toml_edit` gives it the position of the table it walks just before it.
/// `keep` appends the carried table to its parent, so that is the last table
/// already in the parent, or the parent itself: a carried `[dispatch.future]`
/// follows `[dispatch]`, and a carried top-level table goes last. Either way it
/// is the same place on every sync.
fn unpositioned(item: &mut Item) {
    match item {
        Item::Table(table) => {
            table.set_position(None);
            for (_, child) in table.iter_mut() {
                unpositioned(child);
            }
        }
        Item::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                table.set_position(None);
                for (_, child) in table.iter_mut() {
                    unpositioned(child);
                }
            }
        }
        _ => {}
    }
}

/// The dotted path of every setting in `text`, as [`compare`] counts them.
pub fn leaf_paths(text: &str) -> Result<Vec<String>> {
    let doc: DocumentMut = text.parse().context("this file is not valid TOML")?;
    let mut leaves = Vec::new();
    leaves_of_doc(doc.as_table(), &mut Vec::new(), &mut leaves);
    Ok(leaves.iter().map(|path| path.join(".")).collect())
}

/// What rewriting a config file did to it, key by key.
///
/// A rewrite is one blunt act — the whole file, from the struct — so what it
/// did is not visible in the act itself. This reads it back out of the two
/// documents, so `sync` can say "these five settings are new, this one is
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

/// [`leaves_of_doc`] for the inline tables met on the way.
fn leaves_of_inline(
    table: &toml_edit::InlineTable,
    path: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    for (key, value) in table.iter() {
        path.push(key.to_string());
        match value {
            toml_edit::Value::InlineTable(child) if !child.is_empty() => {
                leaves_of_inline(child, path, out);
            }
            _ => out.push(path.clone()),
        }
        path.pop();
    }
}

/// The same walk over the document, so the two lists compare directly.
fn leaves_of_doc(table: &Table, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    for (key, item) in table.iter() {
        path.push(key.to_string());
        match item {
            Item::Table(child) if !child.is_empty() => leaves_of_doc(child, path, out),
            // An inline table is walked like a plain one: left as a leaf, a
            // retired key inside it would never show up as dropped, and
            // `sync` would remove it without saying so.
            Item::Value(toml_edit::Value::InlineTable(child)) if !child.is_empty() => {
                leaves_of_inline(child, path, out);
            }
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

    /// A key inside an inline table is removed from inside it, and the table
    /// stays inline, with everything beside the key untouched.
    #[test]
    fn removing_a_key_from_an_inline_table_keeps_the_table_inline() {
        let text =
            "a = 1\nissue_tracking = { hook = \"\", on_fail = \"pause\", key_in_names = false }\n";
        let out = remove(text, &["issue_tracking", "on_fail"]).unwrap();
        assert_eq!(
            out,
            "a = 1\nissue_tracking = { hook = \"\", key_in_names = false }\n"
        );
    }

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
lane_quiet = \"1m\"

# and one about the next thing
auto_commit = true
";
        let edited = set(
            text,
            &["dispatch", "lane_quiet"],
            &Value::String("30s".into()),
            &Config::default().render().unwrap(),
        )
        .unwrap();

        assert_eq!(
            edited,
            "\
[dispatch]
# a comment somebody wrote about this
lane_quiet = \"30s\"

# and one about the next thing
auto_commit = true
"
        );
    }

    /// A key the file never had lands where the rendering lists it and carries
    /// no comment, because `sync` writes it that way: any other spelling would
    /// leave a file that is called behind when nothing in it is out of date.
    #[test]
    fn a_key_the_file_never_had_arrives_where_the_rendering_puts_it() {
        let text = "[dispatch]\nlane_quiet = \"10s\"\npriority = \"any\"\n";
        let rendered = Config::default().render().unwrap();
        let edited = set(
            text,
            &["dispatch", "auto_commit"],
            &Value::Boolean(false),
            &rendered,
        )
        .unwrap();

        assert_eq!(
            edited,
            "[dispatch]\nlane_quiet = \"10s\"\nauto_commit = false\npriority = \"any\"\n"
        );
    }

    /// A model row's first field makes its table, and the bare `[models]`
    /// header the file held while it was empty goes, as it does in a rendering.
    #[test]
    fn a_first_model_row_leaves_no_bare_models_header() {
        let text = "[dispatch]\nlane_quiet = \"10s\"\n\n[models]\n";
        let rendered = Config::default().render().unwrap();
        let edited = set(
            text,
            &["models", "foo-model", "input"],
            &Value::Float(5.0),
            &rendered,
        )
        .unwrap();

        assert_eq!(
            edited,
            "[dispatch]\nlane_quiet = \"10s\"\n\n[models.foo-model]\ninput = 5.0\n"
        );
    }

    /// A model row whose glob sorts before one the file has is written before
    /// it, as the rendering sorts its rows; written last, `sync` would move it.
    #[test]
    fn a_new_model_row_stands_where_the_rendering_sorts_it() {
        let mut config = Config::default();
        config.models.insert(
            "foo-model".into(),
            crate::usage::ModelPrice {
                input: 5.0,
                ..Default::default()
            },
        );
        let text = config.render().unwrap();
        config.models.insert(
            "aaa-model".into(),
            crate::usage::ModelPrice {
                input: 1.0,
                ..Default::default()
            },
        );
        let rendered = config.render().unwrap();

        let edited = set(
            &text,
            &["models", "aaa-model", "input"],
            &Value::Float(1.0),
            &rendered,
        )
        .unwrap();

        assert_eq!(edited, rendered);
    }

    /// A row the file holds only as a tier table, `[models."bbb".above_100k_tokens]`,
    /// has no header and no position of its own. A row that sorts before it
    /// still has to stand above that tier table; given no place, it was
    /// written after it and `sync` moved it.
    #[test]
    fn a_new_model_row_stands_before_a_row_held_only_as_a_tier() {
        let mut config = Config::default();
        config.models.insert(
            "bbb".into(),
            crate::usage::ModelPrice {
                tier: Some(crate::usage::PriceTier {
                    above_k: 100,
                    rates: crate::usage::Rates {
                        input: 2.0,
                        ..Default::default()
                    },
                }),
                ..Default::default()
            },
        );
        let text = config.render().unwrap();
        config.models.insert(
            "aaa".into(),
            crate::usage::ModelPrice {
                input: 1.0,
                ..Default::default()
            },
        );
        let rendered = config.render().unwrap();

        let edited = set(
            &text,
            &["models", "aaa", "input"],
            &Value::Float(1.0),
            &rendered,
        )
        .unwrap();

        assert_eq!(edited, rendered);
    }

    /// What `sync` reads back out of a rewrite, so its report is the rewrite's
    /// own account of itself rather than a guess made before it ran.
    #[test]
    fn compare_names_the_settings_a_rewrite_added() {
        let before = "[dispatch]\nlane_quiet = \"10m\"\n";
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
# Ten minutes because our lanes are quick and we like the board fresh.
lane_quiet = \"10m\"
";
        let after = Config::default().render().unwrap();

        let refresh = compare(before, &after).unwrap();

        assert!(refresh.renoted.contains(&"dispatch.lane_quiet".to_string()));
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
        text = text.replace("lane_quiet = \"15m\"", "lane_quiet = \"45m\"");
        text.push_str("\n[sandbox]\nenabled = true\n");

        let mut current: Config = toml::from_str(&text).unwrap();
        current.dispatch.auto_commit = false;
        let rewritten = current.render().unwrap();
        current.agrees_with(&rewritten).unwrap();

        assert!(rewritten.contains("lane_quiet = \"45m\""));
        assert!(rewritten.contains("auto_commit = false"));
        assert!(!rewritten.contains("[sandbox]"));
    }
}

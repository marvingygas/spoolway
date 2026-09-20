//! `spoolway herdr bind`/`unbind`: this plugin's own keybindings, written
//! straight into herdr's `config.toml` as `[[keys.command]]` blocks — herdr
//! has no manifest key section, so `herdr-plugin.toml` declares none; see its
//! own closing comment.
//!
//! herdr's real config path is `~/.config/herdr/config.toml`, not
//! `~/.herdr/config.toml` as this task's own context named it — checked
//! against a live herdr 0.9.0 with `strace`, which opens exactly that path
//! on `herdr config check`, and confirmed by `--default-config`'s own
//! header comment (`# Place this file at ~/.config/herdr/config.toml`).
//! [`config_path`] resolves it off [`crate::platform::home_dir`] rather than
//! a herdr subcommand, because herdr has none that prints it.
//!
//! Every block `bind` writes runs the plugin's binary directly — never
//! `herdr plugin action invoke` or `herdr plugin pane open` — one less
//! indirection than the manifest's own four actions take.
//!
//! The file is never reparsed and rewritten as a whole TOML document: a
//! `[[keys.command]]` block a person wrote by hand — for `lazygit`, say — and
//! every comment and unrelated table has to survive byte-for-byte, so both
//! commands work as a text edit, finding and inserting or removing whole
//! `[[keys.command]]` blocks by their own `key =`/`command =` lines, rather
//! than round-tripping the file through `toml::Value` the way
//! `crate::confkv` does for `config.toml` — a file spoolway owns outright,
//! unlike this one.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::ask;
use crate::cli::HerdrKeysArgs;

/// One of the four keys `bind` wires up, and the spoolway subcommand it
/// runs. Mnemonic, not systematic: `s`et up, `d`ispatch, `q`ueue, and
/// doctor's own `k` — `d` and `q` are both taken.
struct Binding {
    key: &'static str,
    verb: &'static str,
}

const BINDINGS: &[Binding] = &[
    Binding {
        key: "prefix+alt+s",
        verb: "init",
    },
    Binding {
        key: "prefix+alt+d",
        verb: "dispatch",
    },
    Binding {
        key: "prefix+alt+q",
        verb: "queue",
    },
    Binding {
        key: "prefix+alt+k",
        verb: "doctor",
    },
];

/// Every binding opens as a popup, sized the way `herdr --default-config`'s
/// own `[[keys.command]]` example is.
const WIDTH: &str = "80%";
const HEIGHT: &str = "80%";

pub fn herdr_bind(args: &HerdrKeysArgs) -> Result<()> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let existing = existing_keys(&text);

    let program = resolve_program()?;
    let mut to_write: Vec<(&Binding, String)> = Vec::new();
    let mut skipped: Vec<(&str, String)> = Vec::new();
    for binding in BINDINGS {
        if let Some((_, command)) = existing.iter().find(|(key, _)| key == binding.key) {
            skipped.push((binding.key, command.clone()));
        } else {
            to_write.push((binding, format!("{program} {}", binding.verb)));
        }
    }

    println!(
        "  {} — {} binding{} to add{}",
        display_path(&path),
        to_write.len(),
        plural(to_write.len()),
        if skipped.is_empty() {
            String::new()
        } else {
            format!(" ({} skipped)", skipped.len())
        }
    );
    println!();
    for (binding, command) in &to_write {
        println!("  {:<14}popup   {command}", binding.key);
    }
    if !skipped.is_empty() {
        if !to_write.is_empty() {
            println!();
        }
        for (key, command) in &skipped {
            if command.is_empty() {
                println!("  skipped {key}: already bound");
            } else {
                println!("  skipped {key}: already bound to `{command}`");
            }
        }
    }

    if to_write.is_empty() {
        return Ok(());
    }

    println!();
    if !confirmed(args, "  Write them?")? {
        println!("  not written");
        return Ok(());
    }

    let mut out = text.clone();
    ensure_trailing_newline(&mut out);
    for (binding, command) in &to_write {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&render_block(binding.key, command));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, &out).with_context(|| format!("writing {}", path.display()))?;

    println!();
    println!(
        "  wrote {} binding{} to {}",
        to_write.len(),
        plural(to_write.len()),
        display_path(&path)
    );
    reload()
}

pub fn herdr_unbind(args: &HerdrKeysArgs) -> Result<()> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let blocks = parse_blocks(&text);
    let matched: Vec<&Block> = blocks
        .iter()
        .filter(|b| b.command.as_deref().is_some_and(written_by_bind))
        .collect();

    if matched.is_empty() {
        println!("  {} — no spoolway bindings to remove", display_path(&path));
        return Ok(());
    }

    println!(
        "  {} — {} binding{} to remove",
        display_path(&path),
        matched.len(),
        plural(matched.len())
    );
    println!();
    let keys: Vec<&str> = matched
        .iter()
        .map(|b| b.key.as_deref().unwrap_or("?"))
        .collect();
    println!("  {}", keys.join("   "));
    println!();

    if !confirmed(args, "  Remove them?")? {
        println!("  not removed");
        return Ok(());
    }

    let out = remove_bound_blocks(&text);
    std::fs::write(&path, &out).with_context(|| format!("writing {}", path.display()))?;

    println!();
    println!(
        "  removed {} binding{} from {}",
        matched.len(),
        plural(matched.len()),
        display_path(&path)
    );
    reload()
}

fn confirmed(args: &HerdrKeysArgs, question: &str) -> Result<bool> {
    if args.yes {
        return Ok(true);
    }
    ask::confirm(question, false)
}

/// Run `herdr server reload-config`, reported on its own line — separately
/// from the write above it — so a write that landed and a reload that
/// failed are never mistaken for one outcome.
fn reload() -> Result<()> {
    let output = Command::new("herdr")
        .args(["server", "reload-config"])
        .output()
        .context("running `herdr server reload-config`")?;
    if output.status.success() {
        println!("  reloaded the running herdr config");
        return Ok(());
    }
    bail!(
        "the change above is already on disk, but `herdr server reload-config` failed: {} — \
         herdr keeps running its old config until this succeeds. Start herdr if it is not \
         running, then run `herdr server reload-config` yourself.",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// `spoolway`, when that resolves on `PATH`; otherwise the absolute path to
/// the plugin-root binary `herdr plugin list` names — see the module doc for
/// why an installed plugin's copy is not on `PATH` at all.
fn resolve_program() -> Result<String> {
    if super::which("spoolway").is_some() {
        return Ok("spoolway".to_string());
    }
    let root = plugin_root()?;
    Ok(root.join("bin").join("spoolway").display().to_string())
}

/// The installed `spoolway` plugin's own root directory, read off `herdr
/// plugin list --json`'s `plugin_root` field.
fn plugin_root() -> Result<PathBuf> {
    let output = Command::new("herdr")
        .args(["plugin", "list", "--json"])
        .output()
        .context("running `herdr plugin list --json`")?;
    if !output.status.success() {
        bail!(
            "`herdr plugin list` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing `herdr plugin list --json`")?;
    let root = value["result"]["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|plugin| plugin["plugin_id"] == "spoolway")
        .and_then(|plugin| plugin["plugin_root"].as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`spoolway` is not on PATH and is not installed as a herdr plugin either \
                 (`herdr plugin list` names no `spoolway`) — install it with npm or cargo, or \
                 run `herdr plugin install marvingygas/spoolway`"
            )
        })?;
    Ok(PathBuf::from(root))
}

/// `~/.config/herdr/config.toml`, the file herdr itself reads and writes —
/// see the module doc for why this is not `~/.herdr/config.toml`.
fn config_path() -> Result<PathBuf> {
    let home =
        crate::platform::home_dir().context("no home directory to find herdr's config.toml in")?;
    Ok(home.join(".config").join("herdr").join("config.toml"))
}

/// `path`, with a leading run matching the home directory rewritten to `~`,
/// for the report lines — left absolute when `path` does not sit under home.
fn display_path(path: &Path) -> String {
    match crate::platform::home_dir() {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn ensure_trailing_newline(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

fn render_block(key: &str, command: &str) -> String {
    format!(
        "[[keys.command]]\nkey = \"{key}\"\ntype = \"popup\"\ncommand = \"{command}\"\n\
         width = \"{WIDTH}\"\nheight = \"{HEIGHT}\"\n"
    )
}

/// Whether `command` is one `bind` would write — a bare `spoolway`, or a
/// path ending `/bin/spoolway`, followed by one of [`BINDINGS`]'s own verbs.
/// Read off the shape alone, not off the currently resolved program: the
/// plugin directory a binding named may already be gone by the time
/// `unbind` runs (`herdr plugin uninstall` deletes it outright), so this
/// must recognise the block without being able to reproduce it.
fn written_by_bind(command: &str) -> bool {
    let Some((prog, verb)) = command.rsplit_once(' ') else {
        return false;
    };
    BINDINGS.iter().any(|b| b.verb == verb)
        && (prog == "spoolway" || prog.ends_with("/bin/spoolway"))
}

/// Remove every `[[keys.command]]` block [`written_by_bind`] recognises from
/// `text`, along with the blank line immediately before it — the one `bind`
/// itself put there, see the module doc — and leave everything else,
/// including a blank line that separates a removed block from content
/// spoolway never wrote, exactly as it was.
///
/// A pure text transform, factored out of [`herdr_unbind`] so the round trip
/// this exists for is a fact this module can check on a string, not only
/// against a real file — see `unbind_leaves_a_later_hand_written_blocks_own_\
/// leading_blank_alone` below.
fn remove_bound_blocks(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let blocks = parse_blocks(text);
    let matched: Vec<&Block> = blocks
        .iter()
        .filter(|b| b.command.as_deref().is_some_and(written_by_bind))
        .collect();
    if matched.is_empty() {
        return text.to_string();
    }

    let mut keep = vec![true; lines.len()];
    for block in &matched {
        for slot in &mut keep[block.start..block.end] {
            *slot = false;
        }
        if block.start > 0 && lines[block.start - 1].trim().is_empty() {
            keep[block.start - 1] = false;
        }
    }
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// One `[[keys.command]]` block found in the file: the line range it
/// occupies (`start` is the header line, `end` one past its last field
/// line) and the two fields these commands care about.
struct Block {
    start: usize,
    end: usize,
    key: Option<String>,
    command: Option<String>,
}

/// Every `[[keys.command]]` block in `text`, scanned line by line rather
/// than parsed as TOML — see the module doc.
fn parse_blocks(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "[[keys.command]]" {
            let start = i;
            let mut j = i + 1;
            let mut key = None;
            let mut command = None;
            while j < lines.len() {
                let trimmed = lines[j].trim();
                // A block's own fields are contiguous, the way [`render_block`]
                // writes them and every real `config.toml` one keeps them, so
                // the first blank line ends the block. Stopping here rather
                // than at the next `[` is what keeps `end` from also
                // swallowing whatever separates this block from whatever
                // comes after it — a blank line that belongs to *that*
                // content, not to this block, and which [`herdr_unbind`]'s
                // own leading-blank rule must find still standing.
                if trimmed.is_empty() || trimmed.starts_with('[') {
                    break;
                }
                if let Some(value) = field_value(trimmed, "key") {
                    key = Some(value);
                } else if let Some(value) = field_value(trimmed, "command") {
                    command = Some(value);
                }
                j += 1;
            }
            blocks.push(Block {
                start,
                end: j,
                key,
                command,
            });
            i = j;
        } else {
            i += 1;
        }
    }
    blocks
}

/// `name = "value"`'s `value`, if `line` is that field — `None` for any
/// other line, including one for a same-prefixed field name (`keys_extra`
/// does not match `"key"`).
fn field_value(line: &str, name: &str) -> Option<String> {
    let rest = line.strip_prefix(name)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    let rest = rest.strip_prefix('"')?;
    let rest = rest.strip_suffix('"')?;
    Some(rest.to_string())
}

/// Every `key =` a block in `text` already carries, paired with that
/// block's own `command =` (empty when the block has none) — what `bind`
/// checks a key it is about to write against.
fn existing_keys(text: &str) -> Vec<(String, String)> {
    parse_blocks(text)
        .into_iter()
        .filter_map(|block| {
            block
                .key
                .map(|key| (key, block.command.unwrap_or_default()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_written_by_bind_is_recognised_either_way() {
        assert!(written_by_bind("spoolway init"));
        assert!(written_by_bind(
            "/home/x/.config/herdr/plugins/github/spoolway-abc/bin/spoolway doctor"
        ));
        assert!(!written_by_bind("lazygit"));
        assert!(!written_by_bind("spoolway"));
        assert!(!written_by_bind("spoolway status"));
        // Any `/bin/spoolway` on the end is recognised, deliberately looser
        // than the one plugin root `resolve_program` would name today — see
        // the module doc: a plugin root may already be gone by the time
        // `unbind` runs, so this cannot insist on reproducing it exactly.
        assert!(written_by_bind("/usr/bin/spoolway init"));
        assert!(!written_by_bind("/usr/bin/spoolway-extra init"));
    }

    #[test]
    fn parse_blocks_reads_key_and_command_in_any_field_order() {
        let text = "\
[theme]
name = \"catppuccin\"

[[keys.command]]
key = \"prefix+alt+g\"
type = \"popup\"
command = \"lazygit\"
width = \"80%\"
height = \"80%\"
";
        let blocks = parse_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].key.as_deref(), Some("prefix+alt+g"));
        assert_eq!(blocks[0].command.as_deref(), Some("lazygit"));
    }

    #[test]
    fn render_and_parse_round_trip_a_block() {
        let block = render_block("prefix+alt+s", "spoolway init");
        let parsed = parse_blocks(&block);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].key.as_deref(), Some("prefix+alt+s"));
        assert_eq!(parsed[0].command.as_deref(), Some("spoolway init"));
    }

    #[test]
    fn field_value_does_not_match_a_longer_field_with_the_same_prefix() {
        assert_eq!(field_value("keys_extra = \"x\"", "key"), None);
        assert_eq!(
            field_value("key = \"prefix+alt+s\"", "key"),
            Some("prefix+alt+s".to_string())
        );
    }

    /// Review's own reproduction: a block `bind` wrote sits between one
    /// block from before `bind` ran and one a person typed in by hand
    /// afterward. Removing it must take only its own leading blank line —
    /// the one `bind` put there — never the blank line that separates it
    /// from the hand-written block after it, which belongs to that block,
    /// not to this one.
    #[test]
    fn unbind_leaves_a_later_hand_written_blocks_own_leading_blank_alone() {
        let seed = "[theme]\nname = \"catppuccin\"\n\n";
        let lazygit = render_block("prefix+alt+g", "lazygit");
        let spoolway_block = render_block("prefix+alt+s", "spoolway init");
        let htop = render_block("prefix+alt+h", "htop");

        let text = format!("{seed}{lazygit}\n{spoolway_block}\n{htop}");
        let out = remove_bound_blocks(&text);

        assert_eq!(out, format!("{seed}{lazygit}\n{htop}"));
    }

    #[test]
    fn remove_bound_blocks_is_a_no_op_with_nothing_to_remove() {
        let text = "[theme]\nname = \"catppuccin\"\n";
        assert_eq!(remove_bound_blocks(text), text);
    }
}

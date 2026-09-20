---
domain: herdr-plugin
covers: ["herdr-plugin.toml", "scripts/fetch-or-build.sh", "src/commands/herdr.rs"]
---

# The herdr plugin

How spoolway installs as a herdr plugin, what its manifest declares, what
`spoolway herdr bind`/`unbind` write, and the rehearsal a person runs before the
repository is listed in herdr's marketplace.

## The manifest

[`herdr-plugin.toml`](../herdr-plugin.toml) at the repository root declares spoolway's id,
version, minimum herdr version, description, `platforms = ["linux", "macos"]`, one `[[build]]`
step and four panes and actions, one pair per command:

| Action | Action title | Pane title | Placement |
|---|---|---|---|
| `init` | Set up this project | Set up this project | popup |
| `queue` | Show the queue | Queue | tab |
| `dispatch` | Run the dispatcher | Dispatch | tab |
| `doctor` | Check this project | Check this project | popup |

The `[[build]]` step runs `scripts/fetch-or-build.sh`, which downloads the release archive
matching the manifest's own `version`, verifies its `SHA256SUMS`, and unpacks the binary to
`./bin/spoolway` — relative to the plugin's own root directory, which `herdr` names
`<repo>-<hash>/` under `~/.config/herdr/plugins/github/` (`spoolway-abc/`, say — the exact path
is whatever `herdr plugin list --json`'s `plugin_root` field names). If no release matches, the
script falls back to `cargo build --release`. Nothing it writes lands outside that directory, so
`herdr plugin uninstall spoolway` removing the directory removes everything the build step put
there. See [Installation and setup](installation.md) for the fallback in full.

`herdr plugin link <PATH>` skips `[[build]]` entirely — it proves the manifest parses and the
panes and actions load without a download, but leaves no `./bin/spoolway` behind, so none of its
four commands run yet.

## What `bind` and `unbind` write

herdr has no keybinding section of its own manifest format, so `herdr-plugin.toml` declares no
keys. `spoolway herdr bind` writes four `[[keys.command]]` blocks straight into herdr's own
config file at `~/.config/herdr/config.toml`, one per pane, each running the plugin's binary
directly rather than `herdr plugin action invoke` — bare `spoolway` when that resolves on
`PATH`, or the absolute `.../bin/spoolway` inside the plugin's own root otherwise.
`spoolway herdr unbind` finds and removes exactly those blocks, by their `key =`/`command =`
lines, and leaves everything else in the file untouched. Both commands reload the running herdr
afterwards with `herdr server reload-config`.

## The rehearsal

None of this can be checked in CI, because none of it exists without a running herdr. Run this
by hand, at a terminal, before the topic goes on the repository — it is the only gate before
the listing appears.

1. `herdr plugin link ~/spoolway` — manifest, panes and actions, no build.
2. `herdr plugin action list` — four actions: `init`, `queue`, `dispatch`, `doctor`.
3. `herdr plugin unlink spoolway`
4. `herdr plugin install marvingygas/spoolway --ref herdr-plugin` — the real path, build script
   and all, off the branch under review rather than `main`.
5. `spoolway herdr bind`
6. `spoolway herdr unbind`
7. `herdr plugin uninstall spoolway`
8. `ls ~/.config/herdr/plugins/github/` — nothing spoolway's own left.
9. `grep spoolway ~/.config/herdr/config.toml` — nothing left.

## Listing spoolway

The marketplace is an automatic index of public GitHub repos carrying the topic
`herdr-plugin`, refreshed within thirty minutes — there is no submission and no review. Setting
that topic and fixing up the repository description are done by hand, by a person with push
rights, once the rehearsal above passes:

- Add the `herdr-plugin` topic to the repository.
- Fix the `piplines` typo in the current repository description at the same time.

The listing appears within thirty minutes of the topic being set.

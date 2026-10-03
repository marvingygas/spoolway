#!/usr/bin/env bash
# `spoolway herdr bind`/`unbind` against a real `~/.config/herdr/config.toml`.
# Both commands are a fact about this machine's herdr install, not about any
# checkout, so the suite stands in an unrelated git repository on purpose and
# proves they leave its identity alone. It needs neither `new_repo` nor a forge.
#
# `herdr server reload-config` is the one real dependency this cannot drive
# for real — there is no server here to reload, the same reason
# `herdr-stub.sh` stands in for a pane elsewhere — so a minimal double
# answers that one verb on `$PATH` and nothing else. `spoolway` itself is
# never stubbed: `run.sh` already puts the build under test first on `$PATH`
# for every suite, so `bind` resolves the bare name the same way a real
# install on `$PATH` would. The plugin-root fallback (`herdr plugin list
# --json`, reached only when `spoolway` is not on `$PATH`, which it always is
# here) is covered by `src/commands/herdr.rs`'s own unit tests instead.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"

LIVE=${WORK:-$(mktemp -d)}

# A scratch $HOME of its own, the same isolation `new_repo` gives every other
# suite — this one just never calls it, since there is no checkout to seed.
export HOME="$LIVE/home"
mkdir -p "$HOME/.config/herdr"

UNRELATED="$LIVE/unrelated"
mkdir -p "$UNRELATED"
git -C "$UNRELATED" init -q
cd "$UNRELATED" || exit 2

HERDRBIN="$LIVE/herdrbin"
mkdir -p "$HERDRBIN"
cat > "$HERDRBIN/herdr" <<'STUB'
#!/bin/sh
if [ "$1" = "server" ] && [ "$2" = "reload-config" ]; then
  echo '{"result":{}}'
  exit 0
fi
echo "herdr-stub: unsupported: $*" >&2
exit 1
STUB
chmod +x "$HERDRBIN/herdr"
PATH="$HERDRBIN:$PATH"; export PATH

CONFIG="$HOME/.config/herdr/config.toml"

# Content spoolway never wrote: a person's own comment and theme, a manual
# binding unrelated to any of spoolway's three, and — on the one key spoolway
# would otherwise claim for the dispatch tab — a binding of the person's own,
# there before `bind` ever runs. None of it may move, reformat, or lose a
# byte across the whole round trip below.
cat > "$CONFIG" <<'SEED'
# a person's own comment, above a person's own theme
[theme]
name = "catppuccin"

[[keys.command]]
key = "prefix+alt+g"
type = "popup"
command = "lazygit"
width = "80%"
height = "80%"

[[keys.command]]
key = "prefix+alt+d"
type = "popup"
command = "tmux-status"
width = "60%"
height = "40%"
SEED
cp "$CONFIG" "$LIVE/seed.toml"

BIND_OUT="$LIVE/bind.out"
if "$SPOOLWAY" herdr bind --yes >"$BIND_OUT" 2>&1; then
  ok "herdr bind writes once confirmed"
else
  bad "herdr bind writes once confirmed"
  sed 's/^/        /' "$BIND_OUT"
fi
has "bind skips the key already bound rather than overwriting it" \
  "skipped prefix+alt+d: already bound to \`tmux-status\`" "$BIND_OUT"
has "bind reports the reload separately from the write" \
  "reloaded the running herdr config" "$BIND_OUT"
has "the person's own theme survives" 'name = "catppuccin"' "$CONFIG"
has "the person's own lazygit binding survives" 'command = "lazygit"' "$CONFIG"
has "the person's own conflicting binding survives untouched" 'command = "tmux-status"' "$CONFIG"
has "init is bound to spoolway on PATH" 'command = "spoolway init"' "$CONFIG"
has "doctor is bound to spoolway on PATH" 'command = "spoolway doctor"' "$CONFIG"
lacks "the dispatch tab's bare spoolway is not bound — its key was already taken" \
  'command = "spoolway"' "$CONFIG"

UNBIND_OUT="$LIVE/unbind.out"
if "$SPOOLWAY" herdr unbind --yes >"$UNBIND_OUT" 2>&1; then
  ok "herdr unbind removes once confirmed"
else
  bad "herdr unbind removes once confirmed"
  sed 's/^/        /' "$UNBIND_OUT"
fi
has "unbind removes only the two blocks bind actually wrote" \
  "2 bindings to remove" "$UNBIND_OUT"
has "unbind reports the reload separately from the removal" \
  "reloaded the running herdr config" "$UNBIND_OUT"
works "the round trip leaves the file byte-identical" \
  diff -u "$LIVE/seed.toml" "$CONFIG"
says "unbinding an already-clean file says so" "no spoolway bindings to remove" \
  "$SPOOLWAY" herdr unbind --yes

# The dispatch tab's own key, free this time: `bind` writes the bare program
# on it, with no verb after it — the one binding of the three that runs
# `spoolway` on its own, and so opens the screen.
printf '# nothing bound yet\n' > "$CONFIG"
FREE_OUT="$LIVE/bind-free.out"
if "$SPOOLWAY" herdr bind --yes >"$FREE_OUT" 2>&1; then
  ok "herdr bind writes on a config with every key free"
else
  bad "herdr bind writes on a config with every key free"
  sed 's/^/        /' "$FREE_OUT"
fi
lacks "and skips nothing" "skipped" "$FREE_OUT"
# The block's own `key` line, then its `type`, then its `command` — read as
# one block, so a bare `spoolway` bound to some other key cannot pass for it.
if awk '/^key = "prefix\+alt\+d"$/ { k = NR } k && NR == k + 2 { print; exit }' "$CONFIG" \
     | grep -qxF 'command = "spoolway"'; then
  ok "prefix+alt+d runs bare spoolway"
else
  bad "prefix+alt+d runs bare spoolway"
  sed 's/^/        /' "$CONFIG"
fi
lacks "and no binding runs spoolway queue any more" 'command = "spoolway queue"' "$CONFIG"
says "unbind takes all three back" "3 bindings to remove" \
  "$SPOOLWAY" herdr unbind --yes
works "leaving the file as it was" \
  bash -c '[ "$(cat "$1")" = "# nothing bound yet" ]' _ "$CONFIG"

# Regression: a block a person adds *after* `bind` has already run sits
# right behind the last block `bind` wrote, with only a blank line between
# them — the one shape a fix here has to get right, since `unbind` must take
# its own blocks' leading blank lines without also taking the blank that
# leads into this one, which belongs to it, not to spoolway.
HANDWRITTEN="$LIVE/handwritten-block.toml"
cat > "$HANDWRITTEN" <<'BLOCK'

[[keys.command]]
key = "prefix+alt+h"
type = "popup"
command = "htop"
width = "50%"
height = "50%"
BLOCK

cp "$LIVE/seed.toml" "$CONFIG"
must "bind, ahead of the hand-written-block regression" "$SPOOLWAY" herdr bind --yes
cat "$HANDWRITTEN" >> "$CONFIG"

works "unbind runs with a hand-written block sitting right after bind's own" \
  "$SPOOLWAY" herdr unbind --yes
cat "$LIVE/seed.toml" "$HANDWRITTEN" > "$LIVE/expected-with-handwritten-block.toml"
works "the hand-written block, and the blank line before it, are untouched" \
  diff -u "$LIVE/expected-with-handwritten-block.toml" "$CONFIG"

# Upgrading from 0.6.0: its `bind` wrote four blocks, and two of their verbs
# are retired — `spoolway dispatch` on prefix+alt+d and `spoolway queue` on
# prefix+alt+q. `unbind` takes all four back, and `bind` replaces the retired
# two instead of skipping their keys as taken.
OLD="$LIVE/bound-by-0.6.0.toml"
{
  printf '# bound by 0.6.0\n'
  for kv in "s init" "d dispatch" "q queue" "k doctor"; do
    set -- $kv
    printf '\n[[keys.command]]\nkey = "prefix+alt+%s"\ntype = "popup"\ncommand = "spoolway %s"\nwidth = "80%%"\nheight = "80%%"\n' "$1" "$2"
  done
} > "$OLD"

cp "$OLD" "$CONFIG"
says "unbind takes back all four blocks 0.6.0 bound" "4 bindings to remove" \
  "$SPOOLWAY" herdr unbind --yes
works "leaving only what was there before them" \
  bash -c '[ "$(cat "$1")" = "# bound by 0.6.0" ]' _ "$CONFIG"

cp "$OLD" "$CONFIG"
OLD_OUT="$LIVE/bind-over-0.6.0.out"
if "$SPOOLWAY" herdr bind --yes >"$OLD_OUT" 2>&1; then
  ok "herdr bind writes over a config 0.6.0 bound"
else
  bad "herdr bind writes over a config 0.6.0 bound"
  sed 's/^/        /' "$OLD_OUT"
fi
has "bind names both retired bindings it removes" "2 retired to remove" "$OLD_OUT"
has "including the old dispatch key" \
  "removing prefix+alt+d: \`spoolway dispatch\` is retired" "$OLD_OUT"
lacks "and does not skip that key as taken" "skipped prefix+alt+d" "$OLD_OUT"
lacks "no binding runs spoolway dispatch any more" 'command = "spoolway dispatch"' "$CONFIG"
lacks "no binding runs spoolway queue any more" 'command = "spoolway queue"' "$CONFIG"
if awk '/^key = "prefix\+alt\+d"$/ { k = NR } k && NR == k + 2 { print; exit }' "$CONFIG" \
     | grep -qxF 'command = "spoolway"'; then
  ok "prefix+alt+d now runs bare spoolway"
else
  bad "prefix+alt+d now runs bare spoolway"
  sed 's/^/        /' "$CONFIG"
fi
says "and unbind then finds exactly the three current ones" "3 bindings to remove" \
  "$SPOOLWAY" herdr unbind --yes

if [ -e "$UNRELATED/.git/spoolway-id" ] || [ -e "$UNRELATED/.git/spoolway-label" ]; then
  bad "herdr commands do not claim the git checkout they are run from"
else
  ok "herdr commands do not claim the git checkout they are run from"
fi

finish

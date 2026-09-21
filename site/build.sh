#!/usr/bin/env bash
# Assemble the Jekyll source for the documentation site.
#
# docs/*.md stays plain markdown so GitHub renders it normally. Everything
# Jekyll needs -- front matter, a layout, .md links rewritten to .html -- is
# added here, into site/_src, which is never committed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/docs"
OUT="$ROOT/site/_src"

rm -rf "$OUT"
mkdir -p "$OUT"

cp "$ROOT/site/_config.yml" "$OUT/"
cp -r "$ROOT/site/_layouts" "$OUT/_layouts"

# Anything dropped in site/static lands at the site root. Search-engine
# verification files go here.
if [ -d "$ROOT/site/static" ]; then
  find "$ROOT/site/static" -maxdepth 1 -type f -exec cp {} "$OUT/" \;
fi

for f in "$SRC"/*.md; do
  base="$(basename "$f" .md)"
  if [ "$base" = "README" ]; then dest="$OUT/index.md"; else dest="$OUT/$base.md"; fi

  title="$(sed -n 's/^# \(.*\)/\1/p' "$f" | head -1)"
  [ -n "$title" ] || title="$base"

  {
    echo "---"
    echo "layout: default"
    printf 'title: %s\n' "$(printf '%s' "$title" | sed 's/"/\\"/g; s/^/"/; s/$/"/')"
    echo "---"
    # Domain metadata guides the archivist inside the repository. It is not
    # page content, so strip a source file's leading front matter before
    # handing the page to Jekyll (which already has the front matter above).
    awk '
      NR == 1 && $0 == "---" { source_front_matter = 1; next }
      source_front_matter && $0 == "---" { source_front_matter = 0; next }
      !source_front_matter { print }
    ' "$f" | sed -e 's:](README\.md):](index.html):g' \
        -e 's:](\([A-Za-z0-9_-]\+\)\.md\(#[^)]*\)\?):](\1.html\2):g'
  } > "$dest"
done

for d in screenshots logo; do
  if compgen -G "$SRC/$d/*.png" > /dev/null; then
    mkdir -p "$OUT/$d"
    cp "$SRC/$d"/*.png "$OUT/$d/"
  fi
done

echo "assembled $(find "$OUT" -maxdepth 1 -name '*.md' | wc -l) pages into $OUT"

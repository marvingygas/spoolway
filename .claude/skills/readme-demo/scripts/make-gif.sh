#!/usr/bin/env bash
# Turns the person's screen recording into the README's demo GIF. 10 frames a
# second, 1440 pixels wide, and one 128-colour palette with no dithering keep
# the terminal text sharp and a 70-second recording near 2.6 MB.
#
# Usage: make-gif.sh VIDEO [OUT]    OUT defaults to docs/assets/spoolway-demo.gif
#
# Uses ffmpeg from PATH, or else the static ffmpeg inside the imageio-ffmpeg
# Python package, fetched into uv's cache. Nothing is installed system-wide.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(git -C "$HERE" rev-parse --show-toplevel)"
VIDEO="${1:?usage: make-gif.sh VIDEO [OUT]}"
OUT="${2:-$REPO_ROOT/docs/assets/spoolway-demo.gif}"
[[ -f "$VIDEO" ]] || { echo "no video at $VIDEO" >&2; exit 1; }

if command -v ffmpeg >/dev/null; then
  FF=ffmpeg
elif command -v uv >/dev/null; then
  FF=$(uv run --quiet --no-project --with imageio-ffmpeg python -I -c \
    'import imageio_ffmpeg; print(imageio_ffmpeg.get_ffmpeg_exe())')
else
  echo "neither ffmpeg nor uv is installed" >&2
  exit 1
fi

"$FF" -v error -y -i "$VIDEO" -an -filter_complex \
  "fps=10,scale=1440:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=none:diff_mode=rectangle" \
  "$OUT"
chmod 644 "$OUT"
"$FF" -hide_banner -i "$OUT" 2>&1 | grep -E "Duration|Video:" | sed 's/^ *//' || true
ls -l "$OUT"

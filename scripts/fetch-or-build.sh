#!/bin/sh
# fetch-or-build.sh — herdr [[build]] step for the spoolway plugin.
#
# Fast path: map `uname` onto one of npm/targets.json's triples, download the
# matching release's spoolway-<triple>.tar.gz, verify it against that
# release's SHA256SUMS, and unpack it to ./bin/spoolway (relative to the
# plugin root — herdr runs this with the plugin directory as $PWD and
# HERDR_PLUGIN_ROOT pointing at it; see herdr-plugin.toml). `herdr plugin
# uninstall` deletes the plugin directory and nothing else, so nothing here
# may write outside it — no ~/.local/bin, no ~/.cargo/bin.
#
# The release tag is read from herdr-plugin.toml's own `version`, which
# verify.yml's `test` job keeps equal to Cargo.toml's.
#
# Fallback: on ANY miss — no triple for this host, no such tag, no asset for
# the triple, a missing SHA256SUMS line, or a checksum mismatch — build from
# source with `cargo build --release` and copy the result into place, so
# installing the plugin never gets harder than a plain build.
#
# Paths and the release base URL are overridable via env (SPOOLWAY_PLUGIN_ROOT
# / SPOOLWAY_BASE_URL) so this is exercisable from a hermetic test.
set -u

repo="marvingygas/spoolway"

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
plugin_root="${HERDR_PLUGIN_ROOT:-${SPOOLWAY_PLUGIN_ROOT:-$script_dir/..}}"
manifest="$plugin_root/herdr-plugin.toml"
out="$plugin_root/bin/spoolway"
targets_json="$plugin_root/npm/targets.json"
base_url="${SPOOLWAY_BASE_URL:-https://github.com/$repo/releases/download}"

have() { command -v "$1" >/dev/null 2>&1; }

# Build from source — the original, unconditional behavior. Sourcing
# ~/.cargo/env means cargo is found even when herdr was launched without
# ~/.cargo/bin on PATH; the `[ -f ]` guard means a missing env file can't
# abort the build.
build_from_source() {
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  if ! have cargo; then
    echo "spoolway needs a Rust toolchain to build, but cargo was not found. Install Rust from https://rustup.rs then re-run: herdr plugin install $repo" >&2
    exit 1
  fi
  ( cd "$plugin_root" && cargo build --release --locked ) || exit 1
  mkdir -p "$(dirname "$out")"
  cp "$plugin_root/target/release/spoolway" "$out"
  chmod +x "$out"
  echo "spoolway: built from source -> $out"
}

fallback() {
  echo "spoolway: $1 — building from source instead." >&2
  [ -n "${tmpdir:-}" ] && rm -rf "$tmpdir"
  build_from_source
  exit $?
}

download() { # download <url> <dest>
  if have curl; then
    curl -fsSL -o "$2" "$1"
  elif have wget; then
    wget -q -O "$2" "$1"
  else
    return 127
  fi
}

sha256_of() { # prints the hex digest of file $1
  if have sha256sum; then
    sha256sum "$1" | awk '{print $1}'
  elif have shasum; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    return 127
  fi
}

# --- resolve the target triple from npm/targets.json ----------------------
[ -f "$targets_json" ] || fallback "missing $targets_json"

os=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)

case "$os" in
  Linux) host_os=linux ;;
  Darwin) host_os=darwin ;;
  *) fallback "unsupported platform $os" ;;
esac

case "$arch" in
  x86_64|amd64) host_cpu=x64 ;;
  arm64|aarch64) host_cpu=arm64 ;;
  *) fallback "unsupported architecture $arch" ;;
esac

# Only linux distinguishes libc; getconf reporting a glibc version is the
# cheapest sign of glibc, and its absence (as on a musl-only system such as
# Alpine) is treated as musl.
host_libc=""
if [ "$host_os" = linux ]; then
  if getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
    host_libc=glibc
  else
    host_libc=musl
  fi
fi

# npm/targets.json is pretty-printed one field per line inside `{ ... }`
# blocks, so a small state machine collects one target per block rather than
# parsing full JSON — this script has to run with nothing beyond a POSIX
# shell, sh's own builtins and awk.
triple=$(awk '
  /"triple":/ { t = $0; sub(/.*: *"/, "", t); sub(/".*/, "", t) }
  /"os":/     { o = $0; sub(/.*: *"/, "", o); sub(/".*/, "", o) }
  /"cpu":/    { c = $0; sub(/.*: *"/, "", c); sub(/".*/, "", c) }
  /"libc":/   {
    if ($0 ~ /null/) { l = "" }
    else { l = $0; sub(/.*: *"/, "", l); sub(/".*/, "", l) }
  }
  /}/ {
    if (t != "") print t "|" o "|" c "|" l
    t = ""
  }
' "$targets_json" | while IFS='|' read -r t o c l; do
  if [ "$o" = "$host_os" ] && [ "$c" = "$host_cpu" ] && [ "$l" = "$host_libc" ]; then
    echo "$t"
    break
  fi
done)

[ -n "$triple" ] || fallback "no entry in $targets_json for $host_os/$host_cpu${host_libc:+/$host_libc}"

# --- read the version this manifest declares -------------------------------
version=$(awk -F'"' '/^version *= *"/{print $2; exit}' "$manifest" 2>/dev/null)
[ -n "$version" ] || fallback "could not read version from $manifest"

asset="spoolway-$triple.tar.gz"

tmpdir=$(mktemp -d 2>/dev/null) || fallback "could not create a temp dir"
trap 'rm -rf "$tmpdir"' EXIT

archive_url="$base_url/v$version/$asset"
sums_url="$base_url/v$version/SHA256SUMS"
tmparchive="$tmpdir/$asset"
tmpsums="$tmpdir/SHA256SUMS"

download "$archive_url" "$tmparchive" || fallback "release asset not available for v$version ($asset)"
download "$sums_url" "$tmpsums" || fallback "SHA256SUMS not available for v$version"

# coreutils' `sha256sum` emits `<hash>  <name>` (two spaces); accept a single
# space or `*` too so a binary-mode line still verifies. release.yml's own
# `assets` job runs `sha256sum ./*`, so every name in this repo's own
# SHA256SUMS carries a `./` prefix — matched as optional so a SHA256SUMS
# without it (e.g. from a plain `sha256sum spoolway-*.tar.gz`) still verifies.
expected=$(grep -E "^[0-9a-f]{64} [ *](\./)?$asset\$" "$tmpsums" 2>/dev/null | awk '{print $1}' | head -n 1)
[ -n "$expected" ] || fallback "no checksum listed for $asset"

actual=$(sha256_of "$tmparchive") || fallback "no sha-256 tool (sha256sum/shasum) available"
if [ "$actual" != "$expected" ]; then
  fallback "checksum mismatch for $asset (expected $expected, got $actual)"
fi

mkdir -p "$(dirname "$out")"
tar -xzf "$tmparchive" -C "$tmpdir" spoolway || fallback "could not unpack $asset"
mv -f "$tmpdir/spoolway" "$out" || fallback "could not install the verified binary to $out"
chmod +x "$out"
echo "spoolway: installed prebuilt v$version ($triple), verified SHA-256, -> $out"

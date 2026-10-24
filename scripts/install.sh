#!/bin/sh
# install.sh — install or update the standalone spoolway binary.
#
# Published on every release as `install.sh`, so one line installs and the
# same line updates:
#
#   curl -fsSL https://github.com/marvingygas/spoolway/releases/latest/download/install.sh | sh
#
# It maps `uname` onto the release's bare file, downloads that file and the
# release's SHA256SUMS, checks the file against its line there, and renames it
# into the install directory. It needs only sh, uname, curl or wget, and
# sha256sum or shasum. It never builds from source (a host with no file is
# refused), never writes outside the install directory, and never edits a shell
# profile: when the directory is not on PATH it prints the line to add.
#
# The release job replaces the version placeholder below with the release's own
# version, so the files come from `releases/download/v<version>/` and never
# from `latest`. A release landing between the two downloads therefore cannot
# pair one release's file with another's checksums.
#
# The file is checked in a temporary directory first, so a refused install
# leaves the install directory untouched. A good file is copied to a temporary
# name inside the install directory and renamed over the target. The rename is
# atomic on one filesystem: an interrupted run leaves the old binary whole, and
# a running dispatcher keeps its open copy of the replaced file.
#
# Every step lives in functions called from the last line. If the download of
# this script is cut short, `sh` then reads an incomplete function definition
# and fails to parse it, instead of running half of the install.
#
# Variables, for tests and unusual setups:
#   SPOOLWAY_INSTALL_DIR  install here instead of ~/.local/bin
#   SPOOLWAY_BASE_URL     release download root instead of the GitHub one
#                         (a file:// URL works with curl)
set -u

version="@SPOOLWAY_VERSION@"
repo="marvingygas/spoolway"

have() { command -v "$1" >/dev/null 2>&1; }

die() { # die <line>... — each argument is printed on its own line
  first=1
  for line in "$@"; do
    if [ "$first" = 1 ]; then
      echo "spoolway: $line" >&2
      first=0
    else
      echo "$line" >&2
    fi
  done
  exit 1
}

download() { # download <url> <dest>
  if have curl; then
    curl -fsSL -o "$2" "$1"
  else
    wget -q -O "$2" "$1"
  fi
}

sha256_of() { # prints the hex digest of file $1
  if have sha256sum; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# Sets $asset to the release file for this host. Libc is not consulted: both
# Linux files are static musl builds that start on glibc and musl alike.
pick_asset() {
  os=$(uname -s 2>/dev/null || echo unknown)
  arch=$(uname -m 2>/dev/null || echo unknown)
  case "$os" in
    Linux) host_os=linux ;;
    Darwin) host_os=macos ;;
    *) host_os= ;;
  esac
  case "$arch" in
    x86_64|amd64) host_cpu=x64 ;;
    arm64|aarch64) host_cpu=arm64 ;;
    *) host_cpu= ;;
  esac
  if [ -z "$host_os" ] || [ -z "$host_cpu" ]; then
    case "$os" in
      MINGW*|MSYS*|CYGWIN*)
        die "no standalone build for $os/$arch." \
          "On Windows, run spoolway inside WSL." ;;
      *)
        die "no standalone build for $os/$arch." \
          "Builds exist for Linux and macOS on x64 and arm64. Build from source instead: cargo install --path . in a clone of https://github.com/$repo." ;;
    esac
  fi
  asset="spoolway-$host_os-$host_cpu"
}

main() {
  case "$version" in
    ""|@*) die "this copy of install.sh has no release version stamped into it." \
      "Run the install line from the README, which fetches the stamped copy from a release." ;;
  esac
  have curl || have wget ||
    die "neither curl nor wget is installed, so nothing can be downloaded." \
      "Install one of them and run the install line again."
  have sha256sum || have shasum ||
    die "neither sha256sum nor shasum is installed, so the download cannot be checked." \
      "Install one of them and run the install line again."

  pick_asset

  default_dir="$HOME/.local/bin"
  dir="${SPOOLWAY_INSTALL_DIR:-$default_dir}"
  target="$dir/spoolway"
  base_url="${SPOOLWAY_BASE_URL:-https://github.com/$repo/releases/download}/v$version"

  work=$(mktemp -d 2>/dev/null) ||
    die "could not create a temporary directory." \
      "Check that TMPDIR is writable and run the install line again."
  tmp=
  trap 'rm -rf "$work"; [ -n "$tmp" ] && rm -f "$tmp"' EXIT
  trap 'exit 1' HUP INT TERM

  echo "spoolway: downloading $asset (v$version)"
  download "$base_url/$asset" "$work/$asset" ||
    die "could not download $asset (v$version) from $base_url." \
      "Check your network, and that release v$version has this file, then run the install line again."
  download "$base_url/SHA256SUMS" "$work/SHA256SUMS" ||
    die "could not download SHA256SUMS for v$version from $base_url." \
      "Check your network, then run the install line again."

  # The release's own SHA256SUMS comes from `sha256sum ./*`, so names carry a
  # `./` prefix. It is matched as optional, as is the `*` of binary mode.
  expected=$(grep -E "^[0-9a-f]{64} [ *](\./)?$asset\$" "$work/SHA256SUMS" 2>/dev/null |
    awk '{print $1}' | head -n 1)
  [ -n "$expected" ] ||
    die "SHA256SUMS for v$version has no line for $asset, so the download cannot be checked." \
      "Nothing was installed."
  actual=$(sha256_of "$work/$asset")
  if [ "$actual" != "$expected" ]; then
    die "checksum mismatch for $asset (v$version)." \
      "The download is damaged or was changed. Nothing was installed."
  fi
  echo "spoolway: checksum verified"

  # Read the old version before the rename replaces the file. A copy that
  # cannot report one still counts as replaced.
  replaced=
  old=
  if [ -e "$target" ]; then
    old=$("$target" --version 2>/dev/null | awk '{print $2; exit}')
    replaced="${old:+v$old}"
    replaced="${replaced:-an unknown version}"
  fi

  mkdir -p "$dir" ||
    die "could not create $dir." \
      "Set SPOOLWAY_INSTALL_DIR to a directory you can write to and run the install line again."
  tmp=$(mktemp "$dir/.spoolway.XXXXXX" 2>/dev/null) ||
    die "could not write to $dir." \
      "Set SPOOLWAY_INSTALL_DIR to a directory you can write to and run the install line again."
  cp "$work/$asset" "$tmp" && chmod 755 "$tmp" && mv -f "$tmp" "$target" ||
    die "could not install $asset to $target." \
      "Nothing was changed. Set SPOOLWAY_INSTALL_DIR to a directory you can write to and run the install line again."
  tmp=

  if [ -n "$replaced" ]; then
    echo "spoolway: replaced $replaced with v$version -> $target"
    # A rerun over the same version changed nothing for a project to apply.
    if [ "$old" != "$version" ]; then
      echo
      echo "Run spoolway sync in each project to apply the update."
    fi
  else
    echo "spoolway: installed v$version -> $target"
  fi

  case ":$PATH:" in
    *":$dir:"*) ;;
    *)
      line="$dir"
      [ "$dir" = "$default_dir" ] && line='$HOME/.local/bin'
      echo
      echo "spoolway: $dir is not on your PATH. Add this line to your"
      echo "shell profile, then open a new terminal:"
      echo
      echo "  export PATH=\"$line:\$PATH\""
      ;;
  esac
}

main "$@"

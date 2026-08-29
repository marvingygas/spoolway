#!/usr/bin/env node
"use strict";

// Locates the prebuilt spoolway binary that npm installed for this platform and
// hands the terminal straight to it.
//
// The binary lives in an optional dependency (@spoolway/<platform>) whose `os`
// and `cpu` fields mean npm downloads exactly one of them. Nothing is fetched
// or compiled at install time, so this works under --ignore-scripts and from a
// warm cache with no network.

const { spawn } = require("node:child_process");

// Anything that would ordinarily stop the dispatcher. It runs as a long-lived
// loop, often under a process manager, so a stop signal has to reach the Rust
// process rather than killing this wrapper out from under it.
const FORWARDED = ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT", "SIGBREAK"];

// Runtime half of npm/targets.json. build-npm.mjs fails the build if the scope
// or any platform here disagrees with that file, so this stays a lookup rather
// than a second source of truth.
const SCOPE = "@spoolway";

const PLATFORMS = {
  "linux-x64-glibc": "linux-x64",
  "linux-x64-musl": "linux-x64-musl",
  "linux-arm64-glibc": "linux-arm64",
  "darwin-arm64": "darwin-arm64",
  "darwin-x64": "darwin-x64",
  "win32-x64": "win32-x64",
};

/// glibcVersionRuntime is absent from the diagnostic report on musl systems.
/// This is how Alpine gets told apart from Debian.
function libc() {
  if (process.platform !== "linux") return null;
  try {
    const report = process.report.getReport();
    return report.header && report.header.glibcVersionRuntime ? "glibc" : "musl";
  } catch {
    // No report available: glibc is the safe guess, and a wrong one surfaces
    // as a clear "package not installed" error rather than a crash.
    return "glibc";
  }
}

function platformKey() {
  const parts = [process.platform, process.arch];
  const c = libc();
  if (c) parts.push(c);
  return parts.join("-");
}

function resolveBinary() {
  const key = platformKey();
  const pkg = PLATFORMS[key];

  if (!pkg) {
    fail(
      `spoolway has no prebuilt binary for ${key}.`,
      "",
      `Supported: ${Object.values(PLATFORMS).join(", ")}.`,
      "",
      "Build from source instead, from a checkout of the repo:",
      "  cargo install --path .",
    );
  }

  // Windows will not run an extension-less file, so the published binary keeps
  // its .exe and the subpath has to ask for it by that name.
  const exe = process.platform === "win32" ? "spoolway.exe" : "spoolway";

  const name = `${SCOPE}/${pkg}`;
  try {
    return require.resolve(`${name}/bin/${exe}`);
  } catch {
    fail(
      `spoolway's binary for ${key} is missing.`,
      "",
      `It ships in the optional dependency ${name}, which npm should have`,
      "installed alongside spoolway. It is usually absent because:",
      "",
      "  - the install ran with --no-optional or --omit=optional",
      "  - the package manager skipped it after a network failure",
      "  - the lockfile was written on a different platform",
      "",
      "Reinstalling normally fixes it:",
      "  npm install -g spoolway --force",
    );
  }
}

/// Prefixes only the first line, matching how the Rust binary reports errors
/// (`spoolway: {err:#}` in src/main.rs) and leaving the rest readable as prose.
function fail(headline, ...rest) {
  console.error(`spoolway: ${headline}`);
  for (const line of rest) console.error(line);
  process.exit(1);
}

function main() {
  const binary = resolveBinary();

  // 'inherit' hands over the real TTY, which the ratatui settings UI needs for
  // raw mode and `spoolway dispatch` needs to render in its own pane.
  const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

  // Registering these also stops Node from exiting on its own, so the Rust
  // process is never orphaned mid-teardown -- it gets to restore the terminal
  // and drop its lock (src/lock.rs) before this wrapper reports anything.
  //
  // An interactive Ctrl-C reaches the child twice, once from the terminal's
  // process group and once from here. Signalling an already-exited process is
  // the only cost, and that just throws ESRCH.
  for (const signal of FORWARDED) {
    // Windows has no POSIX signals and Node emulates only some of them, so
    // registering one it does not know throws ERR_UNKNOWN_SIGNAL. That would
    // kill this wrapper during startup -- before the binary it exists to
    // launch ever runs -- so an unsupported signal is simply one this platform
    // does not forward.
    try {
      process.on(signal, () => {
        try {
          child.kill(signal);
        } catch {
          // Already gone; the exit handler below has the last word.
        }
      });
    } catch {
      // Not a signal this platform can listen for.
    }
  }

  child.on("error", (err) => {
    fail(`could not run ${binary}: ${err.message}`);
  });

  // Report the child's fate as our own, so exit codes and signal deaths stay
  // meaningful to whatever is watching this process.
  child.on("exit", (code, signal) => {
    if (signal) {
      // Drop our handler first, or re-raising would just call it again and
      // leave the wrapper hanging with nothing left to wait for.
      process.removeAllListeners(signal);
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

main();

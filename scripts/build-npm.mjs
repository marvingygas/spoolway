#!/usr/bin/env node
// Assembles the publishable npm packages from binaries built by cargo.
//
// Input:  artifacts/<rust-triple>/spoolway      (one per target; CI downloads these)
// Output: npm/dist/<scope>/<platform>/        (one package per target)
//         npm/spoolway/                         (version + optionalDependencies stamped)
//
// Targets whose artifact is absent are skipped, so this runs locally against
// whatever the current machine can build without pretending the rest exist.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const artifactsDir = path.join(root, "artifacts");
const wrapperDir = path.join(root, "npm", "spoolway");

const { scope, targets } = JSON.parse(
  fs.readFileSync(path.join(root, "npm", "targets.json"), "utf8"),
);
const distDir = path.join(root, "npm", "dist", scope);

const version = cargoVersion();
const repository = gitRepository();

checkShimTableMatches();

fs.rmSync(distDir, { recursive: true, force: true });

const built = [];
const skipped = [];

for (const target of targets) {
  const binary = path.join(artifactsDir, target.triple, binaryName(target));
  if (!fs.existsSync(binary)) {
    skipped.push(target);
    continue;
  }
  buildPlatformPackage(target, binary);
  built.push(target);
}

stampWrapper();
report();

/// The version in Cargo.toml is the single source of truth; every package.json
/// is stamped from it so a release can never publish mismatched versions.
function cargoVersion() {
  const toml = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");
  const pkg = toml.split(/^\[/m)[1]; // the [package] block, which is first
  const match = pkg && pkg.match(/^version\s*=\s*"([^"]+)"/m);
  if (!match) {
    throw new Error("could not read version from Cargo.toml's [package] block");
  }
  return match[1];
}

/// Where the code lives, if that is knowable yet. Derived rather than written
/// down because there was no remote when this was set up -- and because
/// `npm publish --provenance` refuses to run unless this field matches the
/// repository actually building the package.
///
/// Returns null when there is no remote, which leaves the field out entirely
/// rather than publishing a wrong one.
function gitRepository() {
  // Set for every GitHub Actions run, and authoritative there.
  if (process.env.GITHUB_REPOSITORY) {
    return {
      type: "git",
      url: `git+https://github.com/${process.env.GITHUB_REPOSITORY}.git`,
    };
  }

  try {
    const origin = execFileSync("git", ["remote", "get-url", "origin"], {
      cwd: root,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();

    // Normalise the SSH form (git@github.com:owner/repo.git) to the URL npm
    // and provenance both expect.
    const normalised = origin
      .replace(/^git@([^:]+):/, "https://$1/")
      .replace(/\.git$/, "");

    return { type: "git", url: `git+${normalised}.git` };
  } catch {
    return null;
  }
}

/// The shim carries its own copy of the scope and platform table because it
/// must resolve a package before any of this has run. Catch them drifting apart
/// here, at build time, rather than on a user's machine.
function checkShimTableMatches() {
  const shim = fs.readFileSync(path.join(wrapperDir, "bin", "spoolway.js"), "utf8");

  if (!shim.includes(`const SCOPE = "${scope}"`)) {
    throw new Error(
      `npm/spoolway/bin/spoolway.js SCOPE does not match targets.json's "${scope}"`,
    );
  }

  const missing = targets
    .map((t) => t.pkg)
    .filter((pkg) => !shim.includes(`"${pkg}"`));

  if (missing.length) {
    throw new Error(
      `npm/spoolway/bin/spoolway.js has no PLATFORMS entry for: ${missing.join(", ")}`,
    );
  }
}

/// What cargo named the binary, and what the package has to publish it as.
///
/// Windows will not run an extension-less file, so the `.exe` is not cosmetic:
/// dropping it produces a package that installs cleanly and then cannot start.
function binaryName(target) {
  return target.os === "win32" ? "spoolway.exe" : "spoolway";
}

function buildPlatformPackage(target, binary) {
  const dir = path.join(distDir, target.pkg);
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });

  const manifest = {
    name: `${scope}/${target.pkg}`,
    version,
    description: `spoolway binary for ${target.os} ${target.cpu}${
      target.libc === "musl" ? " (musl)" : ""
    }`,
    license: "MIT",
    author: "Marvin Gygas",
    // npm installs only the package matching the host, which is what keeps the
    // download to one 3 MB binary instead of five.
    os: [target.os],
    cpu: [target.cpu],
    files: ["bin"],
    // Deliberately no "exports": the wrapper resolves this package by subpath
    // (@spoolway/<pkg>/bin/spoolway[.exe]), which an exports map would block.
  };
  if (target.libc) manifest.libc = [target.libc];
  if (repository) manifest.repository = repository;

  writeJson(path.join(dir, "package.json"), manifest);

  const dest = path.join(dir, "bin", binaryName(target));
  fs.copyFileSync(binary, dest);
  // Do not inherit the source mode. Under WSL the repo may sit on a DrvFs
  // mount where every file reports 777, and CI artifact downloads drop the
  // executable bit outright -- either way the published binary must be 0755.
  // Meaningless on Windows, where the extension decides, and harmless there.
  fs.chmodSync(dest, 0o755);

  copyDocs(dir);
}

function stampWrapper() {
  const file = path.join(wrapperDir, "package.json");
  const manifest = JSON.parse(fs.readFileSync(file, "utf8"));

  manifest.version = version;
  manifest.optionalDependencies = Object.fromEntries(
    targets.map((t) => [`${scope}/${t.pkg}`, version]),
  );
  // Unlike the platform packages this file is checked in and rewritten in
  // place, so a stale repository from an earlier build has to be cleared, not
  // merely left unset.
  if (repository) {
    manifest.repository = repository;
  } else {
    delete manifest.repository;
  }

  writeJson(file, manifest);
  copyDocs(wrapperDir);
}

function copyDocs(dir) {
  for (const name of ["README.md", "LICENSE"]) {
    const src = path.join(root, name);
    if (fs.existsSync(src)) fs.copyFileSync(src, path.join(dir, name));
  }
}

function writeJson(file, value) {
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function report() {
  console.log(`spoolway ${version}\n`);

  for (const target of built) {
    const binary = path.join(distDir, target.pkg, "bin", binaryName(target));
    const { size } = fs.statSync(binary);
    const sum = createHash("sha256")
      .update(fs.readFileSync(binary))
      .digest("hex")
      .slice(0, 12);
    console.log(
      `  built    ${scope}/${target.pkg.padEnd(15)} ${mb(size)}  ${sum}`,
    );
  }

  for (const target of skipped) {
    console.log(
      `  skipped  ${scope}/${target.pkg.padEnd(15)} no artifacts/${target.triple}/${binaryName(target)}`,
    );
  }

  console.log(`\n  stamped  npm/spoolway/package.json`);
  console.log(
    repository
      ? `  repo     ${repository.url}`
      : "  repo     none (no git remote) -- npm publish --provenance will refuse",
  );

  if (skipped.length) {
    console.log(
      `\n${built.length}/${targets.length} platforms assembled. Publishing an incomplete`,
    );
    console.log("set would leave those platforms unable to install spoolway.");
  } else {
    console.log(`\nAll ${targets.length} platforms assembled.`);
  }
}

function mb(bytes) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`.padStart(8);
}

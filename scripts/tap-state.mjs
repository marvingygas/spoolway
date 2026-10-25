#!/usr/bin/env node
// Says how the tap's formula compares with the release being pushed.
//
// Usage: tap-state.mjs <version> <Formula/spoolway.rb>
//
// Prints one word on stdout and exits 0: "behind" when the tap names an older
// version or has no formula yet (the first release creates it), "same" when it
// already names this version, and "newer" when it names a later one. Exit 2
// means the arguments or the formula could not be understood.
//
// The release job asks this before it pushes. A bare "does the tap name this
// tag" check would let a re-run of an older tag, or two tags finishing out of
// order, replace a newer formula with an older one, and every new install
// would then get the old version. The version is read from the formula's urls
// (".../releases/download/v<version>/..."), which carry the full version,
// prereleases included.
// Versions are ordered as semver orders them, so 0.15.0-rc.1 is older than
// 0.15.0.

import fs from "node:fs";

const [version, formulaPath] = process.argv.slice(2);
if (!version || !formulaPath) {
  fail("usage: tap-state.mjs <version> <Formula/spoolway.rb>");
}
if (!fs.existsSync(formulaPath)) done("behind");

const named = fs
  .readFileSync(formulaPath, "utf8")
  .match(/\/releases\/download\/v([0-9A-Za-z.+-]+)\//);
if (!named) {
  fail(`${formulaPath} has no "/releases/download/v<version>/" url, so its version cannot be read. Fix the formula in the tap by hand, then re-run the job.`);
}
const order = compare(named[1], version);
done(order < 0 ? "behind" : order === 0 ? "same" : "newer");

/// Semver order: the numeric core first, then a prerelease sorts before the
/// release it leads up to, then prerelease identifiers one by one.
function compare(a, b) {
  const [coreA, preA] = split(a);
  const [coreB, preB] = split(b);
  for (let i = 0; i < 3; i++) {
    if (coreA[i] !== coreB[i]) return coreA[i] < coreB[i] ? -1 : 1;
  }
  if (!preA.length || !preB.length) return preB.length - preA.length;
  for (let i = 0; i < Math.max(preA.length, preB.length); i++) {
    const x = preA[i];
    const y = preB[i];
    if (x === undefined) return -1;
    if (y === undefined) return 1;
    if (x === y) continue;
    const numX = /^\d+$/.test(x);
    const numY = /^\d+$/.test(y);
    if (numX && numY) return Number(x) < Number(y) ? -1 : 1;
    if (numX !== numY) return numX ? -1 : 1;
    return x < y ? -1 : 1;
  }
  return 0;
}

function split(v) {
  const [core, ...pre] = v.split("+")[0].split("-");
  return [core.split(".").map(Number), pre.length ? pre.join("-").split(".") : []];
}

function done(state) {
  process.stdout.write(`${state}\n`);
  process.exit(0);
}

function fail(message) {
  process.stderr.write(`Error: ${message}\n`);
  process.exit(2);
}

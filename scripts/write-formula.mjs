#!/usr/bin/env node
// Prints the Homebrew formula for one release on stdout.
//
// Usage: write-formula.mjs <version> <SHA256SUMS> [npm/targets.json]
//
// The release job pushes the output to marvingygas/homebrew-tap as
// Formula/spoolway.rb. Each platform's download file name comes from
// targets.json and its checksum from the SHA256SUMS the release uploaded, so a
// platform can never be paired with another platform's file. A target with no
// download file is not part of the formula. A download with no checksum line
// is an error, because a formula without its checksum cannot be installed.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const [version, sumsPath, targetsPath = path.join(root, "npm", "targets.json")] =
  process.argv.slice(2);

if (!version || !sumsPath) {
  fail("usage: write-formula.mjs <version> <SHA256SUMS> [npm/targets.json]");
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  fail(`the version "${version}" is not a plain release number like 0.15.0, so no formula was written. Pass the tag without its leading "v".`);
}

const sums = readSums(sumsPath);
const { targets } = JSON.parse(fs.readFileSync(targetsPath, "utf8"));
const downloads = new Map(
  targets.filter((t) => t.download).map((t) => [t.os + "/" + t.cpu, t.download]),
);

const base = `https://github.com/marvingygas/spoolway/releases/download/v${version}`;

/// One `on_arm`/`on_intel` block. Homebrew calls x64 "intel".
function block(os, cpu) {
  const download = downloads.get(`${os}/${cpu}`);
  if (!download) {
    fail(`${targetsPath} has no download file for ${os} ${cpu}, so the formula would miss a platform. Add a "download" name to that target.`);
  }
  const sha = sums.get(download);
  if (!sha) {
    fail(`${sumsPath} has no checksum for ${download}, so the formula would not install on ${os} ${cpu}. Re-run the assets job so SHA256SUMS lists every download.`);
  }
  const key = cpu === "arm64" ? "on_arm" : "on_intel";
  return [
    `    ${key} do`,
    `      url "${base}/${download}"`,
    `      sha256 "${sha}"`,
    `    end`,
  ].join("\n");
}

process.stdout.write(`class Spoolway < Formula
  desc "Descriptive agent workflow builder run by a minimal command line state machine"
  homepage "https://github.com/marvingygas/spoolway"
  version "${version}"
  license "MIT"

  on_macos do
${block("darwin", "arm64")}
${block("darwin", "x64")}
  end

  on_linux do
${block("linux", "arm64")}
${block("linux", "x64")}
  end

  def install
    bin.install Dir["spoolway-*"].first => "spoolway"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/spoolway --version")
  end
end
`);

/// Maps file name to sha256 from `sha256sum` output ("<hash>  ./<name>").
function readSums(file) {
  const map = new Map();
  for (const line of fs.readFileSync(file, "utf8").split("\n")) {
    const m = line.match(/^([0-9a-f]{64}) [ *](?:\.\/)?(\S+)$/);
    if (m) map.set(m[2], m[1]);
  }
  return map;
}

function fail(message) {
  process.stderr.write(`Error: ${message}\n`);
  process.exit(1);
}

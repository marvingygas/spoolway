#!/usr/bin/env node
// Regenerates assets/model-prices.json from litellm's own price map.
//
// litellm publishes one JSON file with every model every provider it wraps
// will take, keyed by the bare model name. spoolway needs six numbers per
// model -- a context window and five per-token rates -- so this fetches that
// file, keeps only the chat models that carry a price, and distills each row
// down to just those six, converted from per-token to per-million.
//
// Nothing in the binary calls this, and nothing in the binary fetches
// anything at runtime: assets/model-prices.json is vendored, and this script
// is how a person refreshes the copy, by hand, when they choose to.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const outFile = path.join(root, "assets", "model-prices.json");

const SOURCE_URL =
  "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const SOURCE_PAGE =
  "https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json";
const LICENSE = "MIT";

const raw = await fetchJson(SOURCE_URL);

const models = {};
let skipped = 0;
for (const [name, entry] of Object.entries(raw)) {
  if (!isPricedChatRow(entry)) {
    skipped++;
    continue;
  }
  models[name] = distill(entry);
}

const distilled = {
  source: SOURCE_PAGE,
  license: LICENSE,
  generated: new Date().toISOString().slice(0, 10),
  models,
};

fs.writeFileSync(outFile, `${JSON.stringify(distilled, null, 2)}\n`);

report(Object.keys(models).length, skipped);

/// litellm's file holds every mode it prices -- embeddings, image generation,
/// rerankers -- plus one `sample_spec` row documenting the shape rather than
/// naming a model. spoolway only ever launches a chat model, and a row with no
/// per-token cost is not a price, whatever else it says.
function isPricedChatRow(entry) {
  return (
    entry &&
    typeof entry === "object" &&
    !Array.isArray(entry) &&
    entry.mode === "chat" &&
    isFiniteNumber(entry.input_cost_per_token) &&
    isFiniteNumber(entry.output_cost_per_token)
  );
}

/// litellm prices per token; spoolway prices per million, because that is the
/// unit a person sets a price in and the one `spoolway eval --by` prints.
///
/// A window falls back from `max_input_tokens` to `max_tokens` -- litellm
/// itself defines the second as standing in for the first when a provider
/// only publishes one number -- and then to zero, spoolway's own "unset".
/// Missing cache fields become zero the same way: no field means the provider
/// charges nothing extra to read or write the cache.
function distill(entry) {
  const perMillion = (perToken) => round(numberOr(perToken, 0) * 1_000_000);
  return {
    context_window: numberOr(entry.max_input_tokens, numberOr(entry.max_tokens, 0)),
    input: perMillion(entry.input_cost_per_token),
    output: perMillion(entry.output_cost_per_token),
    cache_read: perMillion(entry.cache_read_input_token_cost),
    cache_write_5m: perMillion(entry.cache_creation_input_token_cost),
    cache_write_1h: perMillion(entry.cache_creation_input_token_cost_above_1hr),
  };
}

function numberOr(value, fallback) {
  return isFiniteNumber(value) ? value : fallback;
}

function isFiniteNumber(value) {
  return typeof value === "number" && Number.isFinite(value);
}

/// Six decimal places is more precision than a per-token rate has to begin
/// with, and rounds away the float noise `* 1_000_000` introduces (litellm's
/// own `5e-06` becomes `4.999999999999999` without it).
function round(n) {
  return Math.round(n * 1e6) / 1e6;
}

async function fetchJson(url) {
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`fetching ${url}: ${res.status} ${res.statusText}`);
  }
  return res.json();
}

function report(kept, dropped) {
  const kb = (fs.statSync(outFile).size / 1024).toFixed(0);
  console.log(`  wrote    ${path.relative(root, outFile)} (${kb} KB)`);
  console.log(`  models   ${kept} priced chat rows kept, ${dropped} other rows dropped`);
  console.log(`  source   ${SOURCE_PAGE}`);
}

#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// Generates the app's market registry from the committed deployment manifests.
//
// deployments/markets/<network>/<market-id>.toml  ->  app/lib/markets.generated.ts
//
// Why a generated TS module rather than reading TOML at runtime: Next.js only
// inlines *static* `process.env.NEXT_PUBLIC_X` property references into the
// browser bundle, so an env-var scheme cannot express N markets, and the browser
// cannot read the deployments directory at all. A checked-in generated module
// makes the manifests the single source of truth while still being statically
// analyzable.
//
// Usage:
//   node scripts/sync-market-registry.mjs          # regenerate
//   node scripts/sync-market-registry.mjs --check  # fail if out of date (CI)

import { readdirSync, readFileSync, writeFileSync, existsSync, statSync } from "node:fs";
import { join, dirname, basename } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = join(dirname(fileURLToPath(import.meta.url)), "..");
const MARKETS_ROOT = join(REPO, "deployments", "markets");
const OUT_FILE = join(REPO, "app", "lib", "markets.generated.ts");

/**
 * Minimal TOML reader for the manifest shape this repo emits: top-level
 * key/value pairs plus one level of `[section]` tables, with string, integer,
 * and boolean scalars. Deliberately not a general TOML parser — it rejects what
 * it does not understand instead of guessing, so a malformed manifest fails the
 * build rather than producing a market with a missing address.
 */
function parseManifest(text, sourcePath) {
  const root = {};
  let table = root;

  text.split("\n").forEach((rawLine, index) => {
    const line = rawLine.trim();
    if (line === "" || line.startsWith("#")) return;

    const section = line.match(/^\[([A-Za-z0-9_]+)\]$/);
    if (section) {
      table = root[section[1]] = root[section[1]] ?? {};
      return;
    }

    const pair = line.match(/^([A-Za-z0-9_]+)\s*=\s*(.+)$/);
    if (!pair) {
      throw new Error(`${sourcePath}:${index + 1}: cannot parse \`${line}\``);
    }
    const [, key, rawValue] = pair;
    const value = rawValue.replace(/\s+#.*$/, "").trim();

    if (value.startsWith('"')) {
      const closing = value.lastIndexOf('"');
      if (closing <= 0) {
        throw new Error(`${sourcePath}:${index + 1}: unterminated string for \`${key}\``);
      }
      table[key] = value.slice(1, closing);
    } else if (value === "true" || value === "false") {
      table[key] = value === "true";
    } else if (/^-?\d+$/.test(value)) {
      table[key] = Number(value);
    } else {
      throw new Error(`${sourcePath}:${index + 1}: unsupported value \`${value}\` for \`${key}\``);
    }
  });

  return root;
}

function require_(manifest, path, sourcePath) {
  const value = path.split(".").reduce((node, key) => (node ?? {})[key], manifest);
  if (value === undefined || value === null || value === "") {
    throw new Error(`${sourcePath}: missing required field \`${path}\``);
  }
  return value;
}

function toMarket(manifest, sourcePath) {
  const contracts = manifest.contracts ?? {};
  const strategy = manifest.strategy ?? {};
  const risk = manifest.risk ?? {};
  const curve = manifest.curve ?? {};
  const operations = manifest.operations ?? {};

  // An address that is empty means a half-finished deploy. Emitting it would
  // produce a market the app happily renders and then fails against at
  // signature time, which is strictly worse than refusing to generate.
  for (const key of ["underlying", "strategy", "sy", "pt", "yt", "tokenizer", "amm"]) {
    require_(manifest, `contracts.${key}`, sourcePath);
  }

  return {
    id: require_(manifest, "market_id", sourcePath),
    label: require_(manifest, "label", sourcePath),
    network: require_(manifest, "network", sourcePath),
    networkPassphrase: require_(manifest, "network_passphrase", sourcePath),
    maturity: require_(manifest, "maturity", sourcePath),
    decimals: manifest.token_decimals ?? 7,
    underlyingAsset: require_(manifest, "underlying_asset", sourcePath),
    sourceCommit: manifest.source_commit ?? "",
    deployedAt: manifest.deployed_at ?? "",
    strategy: {
      kind: require_(manifest, "strategy.kind", sourcePath),
      contract: strategy.contract ?? "",
      pool: strategy.pool ?? "",
      reserve: strategy.reserve ?? "",
      docsUrl: strategy.docs_url ?? "",
    },
    risk: {
      tier: risk.tier ?? "pilot",
      depositCap: risk.deposit_cap ?? 0,
      notes: risk.notes ?? "",
    },
    curve: {
      scalarRoot: curve.scalar_root ?? "",
      initialAnchor: curve.initial_anchor ?? "",
      feeBps: curve.fee_bps ?? 0,
      twapWindow: curve.twap_window ?? 0,
    },
    operations: {
      keeperConfigured: Boolean(operations.keeper_configured),
      ttlExtendDays: operations.ttl_extend_days ?? 120,
      redemptionWindowDays: operations.redemption_window_days ?? 30,
    },
    contracts: {
      underlying: contracts.underlying,
      strategy: contracts.strategy,
      sy: contracts.sy,
      pt: contracts.pt,
      yt: contracts.yt,
      tokenizer: contracts.tokenizer,
      market: contracts.amm,
      orderbook: contracts.orderbook ?? "",
    },
  };
}

function collectMarkets() {
  if (!existsSync(MARKETS_ROOT)) return [];

  const markets = [];
  for (const network of readdirSync(MARKETS_ROOT).sort()) {
    const networkDir = join(MARKETS_ROOT, network);
    if (!statSync(networkDir).isDirectory()) continue;

    for (const file of readdirSync(networkDir).sort()) {
      if (!file.endsWith(".toml")) continue;
      const path = join(networkDir, file);
      const manifest = parseManifest(readFileSync(path, "utf8"), path);
      const market = toMarket(manifest, path);

      const expectedId = basename(file, ".toml");
      if (market.id !== expectedId) {
        throw new Error(`${path}: market_id "${market.id}" does not match its filename "${expectedId}"`);
      }
      if (market.network !== network) {
        throw new Error(`${path}: network "${market.network}" does not match its directory "${network}"`);
      }
      markets.push(market);
    }
  }

  const seen = new Set();
  for (const market of markets) {
    const key = `${market.network}/${market.id}`;
    if (seen.has(key)) throw new Error(`duplicate market ${key}`);
    seen.add(key);
  }

  // Soonest maturity first, so the default selection is the market a user is
  // most likely to be acting on.
  markets.sort((a, b) => a.network.localeCompare(b.network) || a.maturity - b.maturity);
  return markets;
}

function render(markets) {
  return `// SPDX-License-Identifier: Apache-2.0
//
// GENERATED FILE — do not edit by hand.
// Source: deployments/markets/<network>/<market-id>.toml
// Regenerate: node scripts/sync-market-registry.mjs
//
// ${markets.length} market${markets.length === 1 ? "" : "s"} configured.

import type { MarketConfig } from "@sidereal/sdk";

export const GENERATED_MARKETS: MarketConfig[] = ${JSON.stringify(markets, null, 2)};
`;
}

const markets = collectMarkets();

// `--env <network>/<market-id>` prints shell exports for one market, so scripts
// read addresses from the manifest instead of re-scraping TOML with awk.
const envIndex = process.argv.indexOf("--env");
if (envIndex !== -1) {
  const target = process.argv[envIndex + 1];
  if (!target) {
    console.error("usage: sync-market-registry.mjs --env <network>/<market-id>");
    process.exit(2);
  }
  const [network, id] = target.includes("/") ? target.split("/") : ["testnet", target];
  const market = markets.find((m) => m.network === network && m.id === id);
  if (!market) {
    console.error(
      `no market ${network}/${id}; known: ${markets.map((m) => `${m.network}/${m.id}`).join(", ") || "(none)"}`,
    );
    process.exit(1);
  }
  const exports = {
    MARKET_ID: market.id,
    MARKET_LABEL: market.label,
    NETWORK: market.network,
    MATURITY: market.maturity,
    TOKEN_DECIMALS: market.decimals,
    STRATEGY_KIND: market.strategy.kind,
    UNDERLYING: market.contracts.underlying,
    STRATEGY: market.contracts.strategy ?? "",
    SY: market.contracts.sy,
    PT: market.contracts.pt,
    YT: market.contracts.yt,
    TK: market.contracts.tokenizer,
    AMM: market.contracts.market,
    ORDERBOOK: market.contracts.orderbook ?? "",
    UPSTREAM_POOL: market.strategy.pool,
    UPSTREAM_RESERVE: market.strategy.reserve,
  };
  for (const [key, value] of Object.entries(exports)) {
    console.log(`${key}=${JSON.stringify(String(value))}`);
  }
  process.exit(0);
}

const rendered = render(markets);

// This repo is the contracts-only split of the monorepo, so there is no app to
// generate a registry for. `--env` (above) is the mode contract tooling actually
// uses and it has already returned by this point; the remaining modes are a
// no-op here rather than a hard failure, so deploy and smoke scripts keep
// working in both repos from one copy of this file.
if (!existsSync(dirname(OUT_FILE))) {
  console.log(
    `no app/ in this checkout — skipping registry sync (${markets.length} market(s) discovered)`,
  );
  process.exit(0);
}

if (process.argv.includes("--check")) {
  const current = existsSync(OUT_FILE) ? readFileSync(OUT_FILE, "utf8") : "";
  if (current !== rendered) {
    console.error(
      "app/lib/markets.generated.ts is out of date with deployments/markets/.\n" +
        "Run: node scripts/sync-market-registry.mjs"
    );
    process.exit(1);
  }
  console.log(`market registry up to date (${markets.length} market(s))`);
} else {
  writeFileSync(OUT_FILE, rendered);
  console.log(`wrote ${OUT_FILE} (${markets.length} market(s))`);
  for (const market of markets) {
    console.log(`  ${market.network}/${market.id} -> sy=${market.contracts.sy} strategy=${market.strategy.kind}`);
  }
}

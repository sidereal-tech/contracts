#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// Sidereal market keeper: invariant monitoring, TTL renewal, and maturity-rate
// custody for every configured market.
//
// Read-only by default. It reports what it *would* do and exits non-zero if any
// invariant is violated; `--run` is what actually sends transactions. That
// ordering is deliberate — every duty here either moves protocol state or
// freezes a rate that settles YT, so "alert before acting" is the rule.
//
// Duties, per market:
//   1. Invariants   — max_withdraw <= total_assets, exchange_rate > 0,
//                     PT supply covered by the tokenizer's SY escrow.
//   2. Liquidity    — flag when realizable liquidity falls below a floor.
//   3. TTL          — vault.touch(), which forwards to strategy.touch() and so
//                     renews the *upstream* persistent entry. That entry belongs
//                     to Blend/DeFindex; nothing else can renew it for us.
//   4. Maturity     — observe_rate before maturity so a fresh observation always
//                     exists, freeze_maturity_rate immediately after.
//
// Usage:
//   node scripts/keeper.mjs                        # check every market, report only
//   node scripts/keeper.mjs --run                  # perform due upkeep
//   node scripts/keeper.mjs --market testnet/blend-usdc-testnet-q4 --run
//   node scripts/keeper.mjs --json                 # machine-readable report
//
// Networks: the stellar-cli `mainnet` alias ships with no RPC URL, so mainnet
// upkeep needs `MAINNET_RPC_URL=<url>` in the environment (`TESTNET_RPC_URL`
// works the same way). Without it a mainnet market fails loudly rather than
// being silently skipped.
//
// Signer: `DEPLOY_IDENTITY` defaults to `sidereal-deployer`, which is the
// *testnet* key. Every duty here is permissionless — freezing a rate and
// bumping a TTL need no privilege, only a funded account — but `--run` against
// mainnet still needs a signer that exists and holds XLM there, so set
// `DEPLOY_IDENTITY` explicitly for mainnet upkeep.

import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync, existsSync, statSync } from "node:fs";
import { join, dirname, basename } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = join(dirname(fileURLToPath(import.meta.url)), "..");
const MARKETS_ROOT = join(REPO, "deployments", "markets");

const argv = process.argv.slice(2);
const APPLY = argv.includes("--run");
const AS_JSON = argv.includes("--json");
// Guard the -1 case explicitly: `argv[indexOf(...) + 1]` would read argv[0] and
// silently treat the first flag as a market name.
const marketFlagIndex = argv.indexOf("--market");
const ONLY = marketFlagIndex === -1 ? null : (argv[marketFlagIndex + 1] ?? null);
const IDENTITY = process.env.DEPLOY_IDENTITY ?? "sidereal-deployer";

/** The keeper's own public key, resolved lazily — the V1 TTL fallback needs it. */
let identityAddress = null;
function keeperAddress() {
  if (identityAddress === null) {
    identityAddress = execFileSync("stellar", ["keys", "address", IDENTITY], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    }).trim();
  }
  return identityAddress;
}

/** Fraction of total_assets that must stay realizable before we warn. */
const LIQUIDITY_FLOOR_BPS = BigInt(process.env.LIQUIDITY_FLOOR_BPS ?? "2000"); // 20%
/** Freeze the maturity rate once we are this far past maturity. */
const FREEZE_GRACE_SECONDS = Number(process.env.FREEZE_GRACE_SECONDS ?? "0");

const findings = [];
const actions = [];

function note(level, market, message, extra = {}) {
  findings.push({ level, market, message, ...extra });
  if (!AS_JSON) {
    const tag = { ok: "\x1b[1;32m  ok\x1b[0m", warn: "\x1b[1;33mwarn\x1b[0m", fail: "\x1b[1;31mfail\x1b[0m" }[level];
    console.log(`${tag} ${message}`);
  }
}

function log(message) {
  if (!AS_JSON) console.log(`\x1b[1;34m==>\x1b[0m ${message}`);
}

// --- manifest loading -------------------------------------------------------

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
    if (!pair) throw new Error(`${sourcePath}:${index + 1}: cannot parse \`${line}\``);
    const [, key, raw] = pair;
    const value = raw.replace(/\s+#.*$/, "").trim();
    if (value.startsWith('"')) table[key] = value.slice(1, value.lastIndexOf('"'));
    else if (value === "true" || value === "false") table[key] = value === "true";
    else if (/^-?\d+$/.test(value)) table[key] = Number(value);
    else throw new Error(`${sourcePath}:${index + 1}: unsupported value \`${value}\``);
  });
  return root;
}

function loadMarkets() {
  if (!existsSync(MARKETS_ROOT)) return [];
  const markets = [];
  for (const network of readdirSync(MARKETS_ROOT).sort()) {
    const dir = join(MARKETS_ROOT, network);
    if (!statSync(dir).isDirectory()) continue;
    for (const file of readdirSync(dir).sort()) {
      if (!file.endsWith(".toml")) continue;
      const path = join(dir, file);
      const manifest = parseManifest(readFileSync(path, "utf8"), path);
      markets.push({
        key: `${network}/${basename(file, ".toml")}`,
        network,
        id: manifest.market_id,
        label: manifest.label,
        maturity: manifest.maturity,
        contracts: manifest.contracts ?? {},
        strategy: manifest.strategy ?? {},
        // V1 markets put custody in the SY wrapper itself; V2 splits it behind a
        // strategy contract. The wrapper has no `touch`, so the TTL duty differs.
        generation: manifest.contracts?.strategy ? "v2" : "v1",
      });
    }
  }
  markets.push(...loadLegacyMarkets());
  return ONLY ? markets.filter((m) => m.key === ONLY || m.id === ONLY) : markets;
}

// The V1 mainnet market predates the per-market layout and lives in a flat
// manifest. It is deliberately NOT moved under deployments/markets/: that tree
// also generates the app's market registry, and a matured V1 market has no
// business in the market switcher. But it still has real custody, a real
// maturity, and real TTLs, so the keeper has to see it — the whole reason its
// 2026-08-09 maturity passed unattended is that this loader did not exist.
function loadLegacyMarkets() {
  const legacy = [
    { file: "mainnet.toml", network: "mainnet", id: "blend-usdc-mainnet-30d", label: "Blend v2 USDC · V1 pilot" },
  ];
  const markets = [];
  for (const entry of legacy) {
    const path = join(REPO, "deployments", entry.file);
    if (!existsSync(path)) continue;
    const manifest = parseManifest(readFileSync(path, "utf8"), path);
    if (!manifest.contracts?.sy_wrapper) continue;
    // A retired market has no upkeep anyone intends to perform. Reporting it
    // every run would make the keeper cry wolf, which is worse than not
    // watching it at all -- the whole value of this script is that a warning
    // means something.
    if (manifest.retired) continue;
    markets.push({
      key: `${entry.network}/${entry.id}`,
      network: entry.network,
      id: entry.id,
      label: entry.label,
      maturity: manifest.maturity,
      contracts: {
        underlying: manifest.contracts.underlying,
        sy: manifest.contracts.sy_wrapper,
        pt: manifest.contracts.pt_token,
        yt: manifest.contracts.yt_token,
        tokenizer: manifest.contracts.tokenizer,
        amm: manifest.contracts.amm,
      },
      strategy: {},
      generation: "v1",
    });
  }
  return markets;
}

// --- chain access -----------------------------------------------------------

/**
 * Network passphrases, needed only when an RPC URL is supplied explicitly.
 * The stellar-cli `mainnet` alias ships with no RPC URL ("Bring Your Own"), so
 * without an override every mainnet invocation fails before it reaches a
 * contract — which is the second reason the mainnet market went unattended.
 */
const PASSPHRASES = {
  mainnet: "Public Global Stellar Network ; September 2015",
  testnet: "Test SDF Network ; September 2015",
};

function networkArgs(network) {
  const override = process.env[`${network.toUpperCase()}_RPC_URL`];
  if (override && PASSPHRASES[network]) {
    return ["--rpc-url", override, "--network-passphrase", PASSPHRASES[network]];
  }
  return ["--network", network];
}

function invoke(network, contractId, fn, { send = false, args = [] } = {}) {
  const argv = [
    "contract",
    "invoke",
    "--id",
    contractId,
    "--source",
    IDENTITY,
    ...networkArgs(network),
    `--send=${send ? "yes" : "no"}`,
    "--",
    fn,
    ...args,
  ];
  const out = execFileSync("stellar", argv, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  return out.trim().replace(/^"|"$/g, "");
}

function tryInvoke(network, contractId, fn, opts) {
  try {
    return { ok: true, value: invoke(network, contractId, fn, opts) };
  } catch (error) {
    const text = String(error.stderr ?? error.message ?? error);
    return { ok: false, error: text.split("\n").slice(-4).join(" ").trim() };
  }
}

const big = (value) => {
  // `BigInt("")` is 0n, so an unreadable call would otherwise be indistinguishable
  // from a genuine zero — and every invariant below that compares against 0 would
  // report "ok" for a check it never actually performed.
  if (typeof value !== "string" || value.trim() === "") return null;
  try {
    return BigInt(value.trim());
  } catch {
    return null;
  }
};

// --- duties -----------------------------------------------------------------

function checkMarket(market) {
  const { network, contracts } = market;
  log(`${market.key} — ${market.label}`);

  // 1. Valuation and liquidity.
  const rate = tryInvoke(network, contracts.sy, "exchange_rate");
  if (!rate.ok) {
    note("fail", market.key, `exchange_rate unreadable: ${rate.error}`);
    return; // Everything downstream prices off this; do not guess.
  }
  const rateValue = big(rate.value);
  if (rateValue === null || rateValue <= 0n) {
    note("fail", market.key, `exchange_rate is not positive: ${rate.value}`);
  } else {
    note("ok", market.key, `exchange_rate=${rate.value}`);
  }

  const assets = big(tryInvoke(network, contracts.sy, "total_assets").value ?? "");
  const liquid = big(tryInvoke(network, contracts.sy, "max_withdraw").value ?? "");

  if (assets !== null && liquid !== null) {
    if (liquid > assets) {
      note("fail", market.key, `max_withdraw ${liquid} exceeds total_assets ${assets}`, {
        assets: String(assets),
        liquid: String(liquid),
      });
    } else if (assets > 0n && (liquid * 10_000n) / assets < LIQUIDITY_FLOOR_BPS) {
      // Not a failure: a redemption inside realizable liquidity still settles at
      // the unchanged pro-rata rate. It is a reason to gate *new deposits* and
      // to tell holders, which is what an alert is for.
      note(
        "warn",
        market.key,
        `realizable liquidity ${liquid} is under ${LIQUIDITY_FLOOR_BPS}bps of assets ${assets}`,
        { assets: String(assets), liquid: String(liquid) },
      );
    } else {
      note("ok", market.key, `assets=${assets} max_withdraw=${liquid}`);
    }
  }

  // 2. PT seniority: the tokenizer's SY escrow must cover PT face.
  const escrow = big(tryInvoke(network, contracts.tokenizer, "escrowed_sy").value ?? "");
  const ptSupply = big(tryInvoke(network, contracts.pt, "total_supply").value ?? "");

  // Coverage must be measured at the rate redemption will actually settle at.
  // After maturity that is the frozen rate, which can sit well below the live
  // one — checking against the live rate understates what PT is owed and would
  // report a shortfall as healthy.
  const frozen = big(tryInvoke(network, contracts.tokenizer, "maturity_rate").value ?? "");
  const settlementRate = frozen !== null && frozen > 0n ? frozen : rateValue;
  if (frozen !== null && frozen > 0n && rateValue && frozen !== rateValue) {
    note("ok", market.key, `settling at the frozen rate ${frozen} (live is ${rateValue})`);
  }

  if (escrow !== null && ptSupply !== null && settlementRate && settlementRate > 0n) {
    const WAD = 1_000_000_000_000_000_000n;
    // PT face is denominated in asset units; escrow is in SY shares.
    const required = ptSupply === 0n ? 0n : (ptSupply * WAD + settlementRate - 1n) / settlementRate;
    if (escrow < required) {
      note("fail", market.key, `SY escrow ${escrow} is below PT coverage ${required}`, {
        escrow: String(escrow),
        required: String(required),
      });
    } else {
      note("ok", market.key, `PT coverage: escrow=${escrow} required=${required}`);
    }
  }

  // 3. Upstream binding still intact.
  if (contracts.strategy) {
    const boundVault = tryInvoke(network, contracts.strategy, "vault");
    if (!boundVault.ok || boundVault.value !== contracts.sy) {
      note("fail", market.key, `strategy no longer names the vault (${boundVault.value ?? boundVault.error})`);
    }
  }

  // 4. TTL renewal. Permissionless, cheap, and the only way the upstream
  //    persistent entry gets renewed.
  if (market.generation === "v2") {
    if (APPLY) {
      const touched = tryInvoke(network, contracts.sy, "touch", { send: true });
      if (touched.ok) {
        actions.push({ market: market.key, action: "touch", result: "sent" });
        note("ok", market.key, "renewed vault and upstream TTLs");
      } else {
        note("warn", market.key, `touch failed: ${touched.error}`);
      }
    } else {
      actions.push({ market: market.key, action: "touch", result: "would send" });
      note("ok", market.key, "would renew TTLs (vault.touch)");
    }
  } else {
    // V1 has no `touch`. A zero-value self-approve passes the wrapper's
    // validation and bumps the instance entry, which is free and needs only the
    // keeper's own signature. It does NOT renew Blend's `Positions(vault)`
    // entry: only a real deposit or redeem originated by the wrapper does that,
    // so flag it rather than reporting a renewal we did not perform.
    const self = keeperAddress();
    const approveArgs = [
      "--from", self,
      "--spender", self,
      "--amount", "0",
      "--expiration_ledger", "0",
    ];
    if (APPLY) {
      const bumped = tryInvoke(network, contracts.sy, "approve", { send: true, args: approveArgs });
      if (bumped.ok) {
        actions.push({ market: market.key, action: "approve_zero", result: "sent" });
        note("ok", market.key, "renewed the V1 vault instance TTL (zero-value self-approve)");
      } else {
        note("warn", market.key, `V1 instance TTL bump failed: ${bumped.error}`);
      }
    } else {
      actions.push({ market: market.key, action: "approve_zero", result: "would send" });
      note("ok", market.key, "would renew the V1 vault instance TTL (zero-value self-approve)");
    }
    note(
      "warn",
      market.key,
      "V1 market: Blend's upstream persistent entry is NOT renewable by the keeper. " +
        "It needs a value-moving deposit or redeem before it expires.",
    );
  }

  // 5. Maturity-rate custody.
  const nowSec = Math.floor(Date.now() / 1000);
  const matured = nowSec >= market.maturity + FREEZE_GRACE_SECONDS;
  if (!matured) {
    const days = ((market.maturity - nowSec) / 86400).toFixed(1);
    if (APPLY) {
      const observed = tryInvoke(network, contracts.tokenizer, "observe_rate", { send: true });
      if (observed.ok) {
        actions.push({ market: market.key, action: "observe_rate", result: "sent" });
        note("ok", market.key, `recorded a rate observation (${days}d to maturity)`);
      } else {
        note("warn", market.key, `observe_rate failed: ${observed.error}`);
      }
    } else {
      actions.push({ market: market.key, action: "observe_rate", result: "would send" });
      note("ok", market.key, `would record a rate observation (${days}d to maturity)`);
    }
  } else {
    const frozen = tryInvoke(network, contracts.tokenizer, "maturity_rate");
    const frozenValue = frozen.ok ? big(frozen.value) : null;
    if (frozenValue && frozenValue > 0n) {
      note("ok", market.key, `maturity rate already frozen at ${frozenValue}`);
    } else if (APPLY) {
      const froze = tryInvoke(network, contracts.tokenizer, "freeze_maturity_rate", { send: true });
      if (froze.ok) {
        actions.push({ market: market.key, action: "freeze_maturity_rate", result: froze.value });
        note("ok", market.key, `froze the maturity rate at ${froze.value}`);
      } else {
        note("fail", market.key, `freeze_maturity_rate failed: ${froze.error}`);
      }
    } else {
      actions.push({ market: market.key, action: "freeze_maturity_rate", result: "would send" });
      note("warn", market.key, "matured and NOT frozen — run with --run to freeze");
    }
  }
}

// --- main -------------------------------------------------------------------

const markets = loadMarkets();
if (markets.length === 0) {
  console.error(ONLY ? `no market matching "${ONLY}"` : "no market manifests found");
  process.exit(ONLY ? 1 : 0);
}

if (!AS_JSON) {
  log(`Keeper ${APPLY ? "run" : "check (read-only)"} over ${markets.length} market(s)`);
}

for (const market of markets) {
  try {
    checkMarket(market);
  } catch (error) {
    note("fail", market.key, `keeper aborted: ${String(error.message ?? error)}`);
  }
}

const failures = findings.filter((f) => f.level === "fail");
const warnings = findings.filter((f) => f.level === "warn");

if (AS_JSON) {
  console.log(
    JSON.stringify(
      {
        applied: APPLY,
        checkedAt: new Date().toISOString(),
        markets: markets.map((m) => m.key),
        findings,
        actions,
        summary: { failures: failures.length, warnings: warnings.length },
      },
      null,
      2,
    ),
  );
} else {
  log(`${failures.length} failure(s), ${warnings.length} warning(s)`);
  if (!APPLY && actions.length > 0) {
    log("Re-run with --run to perform the upkeep above");
  }
}

process.exit(failures.length > 0 ? 1 : 0);

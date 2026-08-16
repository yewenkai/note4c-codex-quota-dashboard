#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const endpoint = "https://chatgpt.com/backend-api/wham/usage";
const registryPath =
  process.env.CODEX_AUTH_REGISTRY_PATH ||
  path.join(process.env.HOME || "", ".codex", "accounts", "registry.json");
const paidPlans = new Set(["team", "business", "plus"]);
const retryDelaysMs = [0, 1500, 3000, 5000, 8000];

function decodeClaims(accessToken) {
  const segments = accessToken.split(".");
  if (segments.length < 2) throw new Error("access token 不是 JWT");
  return JSON.parse(Buffer.from(segments[1], "base64url").toString("utf8"));
}

function normalizePlan(plan) {
  return plan === "business" ? "team" : plan;
}

function plansMatch(left, right) {
  return normalizePlan(left) === normalizePlan(right);
}

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, "utf8"));
}

function loadPaidSnapshots(accountsDirectory, registry) {
  const paidAccounts = registry.accounts.filter((account) => paidPlans.has(account.plan));
  const snapshots = [];
  for (const fileName of fs.readdirSync(accountsDirectory)) {
    if (!fileName.endsWith(".json") || fileName === path.basename(registryPath)) continue;
    let auth;
    try {
      auth = readJson(path.join(accountsDirectory, fileName));
    } catch {
      continue;
    }
    const accessToken = auth.tokens?.access_token;
    if (typeof accessToken !== "string" || accessToken.length === 0) continue;
    let claims;
    try {
      claims = decodeClaims(accessToken);
    } catch {
      continue;
    }
    const authClaims = claims["https://api.openai.com/auth"] || {};
    const profile = claims["https://api.openai.com/profile"] || {};
    const plan = authClaims.chatgpt_plan_type;
    const email = profile.email;
    const accountId = auth.tokens?.account_id || authClaims.chatgpt_account_id;
    if (
      !paidPlans.has(plan) ||
      typeof email !== "string" ||
      typeof accountId !== "string" ||
      accountId.length === 0
    ) {
      continue;
    }
    const account = paidAccounts.find(
      (candidate) => candidate.email === email && plansMatch(candidate.plan, plan),
    );
    if (!account) continue;
    snapshots.push({ email, plan: account.plan, accountId, accessToken });
  }

  if (snapshots.length !== paidAccounts.length) {
    throw new Error(
      `付费账号认证快照为 ${snapshots.length} 个，注册表为 ${paidAccounts.length} 个`,
    );
  }
  const unique = new Set(snapshots.map((snapshot) => `${snapshot.email}\n${snapshot.plan}`));
  if (unique.size !== snapshots.length) throw new Error("付费账号认证快照存在重复映射");
  snapshots.sort((left, right) => left.email.localeCompare(right.email));
  return snapshots;
}

function delay(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function fetchUsage(snapshot, accountIndex) {
  let lastError = "未知错误";
  for (let attempt = 0; attempt < retryDelaysMs.length; attempt += 1) {
    if (retryDelaysMs[attempt] > 0) await delay(retryDelaysMs[attempt]);
    try {
      const response = await fetch(endpoint, {
        method: "GET",
        headers: {
          Authorization: `Bearer ${snapshot.accessToken}`,
          "ChatGPT-Account-Id": snapshot.accountId,
          "User-Agent": "note4c-codex-quota-dashboard/0.1",
        },
        signal: AbortSignal.timeout(15000),
      });
      if (response.status !== 200) {
        lastError =
          response.status === 401 || response.status === 403
            ? `HTTP ${response.status}（认证已失效，需要重新登录该账号）`
            : `HTTP ${response.status}`;
        if (response.status === 401 || response.status === 403) break;
        continue;
      }
      const usage = await response.json();
      if (usage.email !== snapshot.email || !usage.rate_limit?.primary_window) {
        lastError = "响应身份或额度窗口不匹配";
        continue;
      }
      return usage;
    } catch (error) {
      lastError = `${error?.name || "Error"}/${error?.cause?.code || error?.message || "request failed"}`;
    }
  }
  throw new Error(`付费账号 ${accountIndex}（${snapshot.plan}）刷新失败：${lastError}`);
}

function convertWindow(window) {
  if (!window) return null;
  const usedPercent = Math.round(Number(window.used_percent));
  const windowMinutes = Math.round(Number(window.limit_window_seconds) / 60);
  const resetsAt = Math.trunc(Number(window.reset_at));
  if (
    !Number.isInteger(usedPercent) ||
    usedPercent < 0 ||
    usedPercent > 100 ||
    !Number.isInteger(windowMinutes) ||
    windowMinutes <= 0 ||
    !Number.isInteger(resetsAt) ||
    resetsAt <= 0
  ) {
    throw new Error("额度窗口字段无效");
  }
  return {
    used_percent: usedPercent,
    window_minutes: windowMinutes,
    resets_at: resetsAt,
  };
}

function convertUsage(usage, registryPlan) {
  const credits = usage.credits || {};
  return {
    primary: convertWindow(usage.rate_limit.primary_window),
    secondary: convertWindow(usage.rate_limit.secondary_window),
    credits: {
      has_credits: Boolean(credits.has_credits),
      unlimited: Boolean(credits.unlimited),
      balance: credits.balance == null ? null : String(credits.balance),
    },
    plan_type: registryPlan,
  };
}

function atomicWriteJson(filePath, value) {
  const temporary = `${filePath}.note4c.${process.pid}.tmp`;
  const mode = fs.statSync(filePath).mode & 0o777;
  const descriptor = fs.openSync(temporary, "w", mode);
  try {
    fs.writeFileSync(descriptor, `${JSON.stringify(value, null, 2)}\n`, "utf8");
    fs.fsyncSync(descriptor);
  } finally {
    fs.closeSync(descriptor);
  }
  fs.renameSync(temporary, filePath);
}

async function main() {
  if (!process.env.HOME || !path.isAbsolute(registryPath)) {
    throw new Error("CODEX_AUTH_REGISTRY_PATH 必须为绝对路径");
  }
  const initialRegistry = readJson(registryPath);
  if (initialRegistry.schema_version !== 3 || !Array.isArray(initialRegistry.accounts)) {
    throw new Error("只支持 codex-auth registry schema 3");
  }
  const snapshots = loadPaidSnapshots(path.dirname(registryPath), initialRegistry);
  const refreshed = [];
  for (let index = 0; index < snapshots.length; index += 1) {
    refreshed.push(await fetchUsage(snapshots[index], index + 1));
  }

  const latestRegistry = readJson(registryPath);
  if (latestRegistry.schema_version !== 3 || !Array.isArray(latestRegistry.accounts)) {
    throw new Error("刷新期间注册表结构发生变化");
  }
  const observedAt = Math.floor(Date.now() / 1000);
  for (let index = 0; index < snapshots.length; index += 1) {
    const snapshot = snapshots[index];
    const account = latestRegistry.accounts.find(
      (candidate) =>
        candidate.email === snapshot.email && plansMatch(candidate.plan, snapshot.plan),
    );
    if (!account) throw new Error(`刷新期间付费账号 ${index + 1} 映射发生变化`);
    account.last_usage = convertUsage(refreshed[index], account.plan);
    account.last_usage_at = observedAt;
  }
  atomicWriteJson(registryPath, latestRegistry);

  for (const snapshot of snapshots) {
    process.stderr.write(
      `[debug] response usage: ${snapshot.email} status=200 result=usage-windows\n`,
    );
  }
}

main().catch((error) => {
  process.stderr.write(`顺序刷新失败：${error?.message || error}\n`);
  process.exit(1);
});

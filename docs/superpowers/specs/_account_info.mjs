// Raycast 账号额度查询 — 批量读 accounts/*.json，列出剩余额度 / 试用到期时间
//
// 数据源（逆向自 backend/index.mjs，均带 Bearer token 即可）:
//   GET /api/v1/me           -> 账号信息 + subscription（tier/trial_days_left/current_period_end）
//   GET /api/v1/ai/credits   -> AI 额度（remaining/total/next_credits_at）
//
// 用法:
//   node scripts/account_info.mjs              # 查 accounts/ 下所有账号
//   node scripts/account_info.mjs 20260914     # 只查指定标签（也接受完整路径或 .json 结尾）

import fs from 'fs';
import path from 'path';

const UA = 'Raycast/2.3.1.0 (x-Windows-x64 10.0.26100)';
const accountsDir = path.join(import.meta.dirname, '..', 'accounts');

// ---- 参数 -> 账号文件列表 ----
let files;
if (process.argv[2]) {
  const arg = process.argv[2];
  const cand = [
    arg,
    arg.endsWith('.json') ? arg : `${arg}.json`,
    path.join(accountsDir, arg.endsWith('.json') ? arg : `${arg}.json`),
  ].find(p => fs.existsSync(p));
  if (!cand) {
    console.error(`[!] 找不到账号文件: ${arg}（也不在 accounts/ 下）`);
    process.exit(1);
  }
  files = [cand];
} else {
  files = fs.readdirSync(accountsDir).filter(f => f.endsWith('.json')).map(f => path.join(accountsDir, f));
  if (files.length === 0) {
    console.error('[!] accounts/ 下没有账号文件，先跑 raycast_register.mjs。');
    process.exit(1);
  }
}

// ---- 拉一个账号的信息 ----
async function fetchJson(url, token) {
  const r = await fetch(url, {
    headers: { Authorization: `Bearer ${token}`, Accept: 'application/json', 'User-Agent': UA },
  });
  if (r.status === 401) throw new Error('token 失效(401)');
  if (!r.ok) throw new Error(`HTTP ${r.status}: ${(await r.text()).slice(0, 120)}`);
  return r.json();
}

async function queryAccount(file) {
  const acct = JSON.parse(fs.readFileSync(file, 'utf8'));
  const tag = acct.tag ?? path.basename(file, '.json');
  const email = acct.user?.email ?? acct.user?.handle ?? '(未知)';
  const out = { tag, file, email, error: null };

  try {
    const [me, credits] = await Promise.all([
      fetchJson('https://backend.raycast.com/api/v1/me', acct.access_token),
      fetchJson('https://backend.raycast.com/api/v1/ai/credits', acct.access_token),
    ]);
    out.email = me.email ?? me.handle ?? email;
    out.tier = me.subscription?.tier ?? '-';
    out.status = me.subscription?.status ?? (me.can_apply_for_free_trial === false ? '无试用资格' : '未订阅');
    out.trialDaysLeft = me.subscription?.trial_days_left;
    out.periodEnd = me.subscription?.current_period_end;
    out.remaining = parseFloat(credits.remaining_balance_credits);
    out.total = parseFloat(credits.total_balance_credits);
    out.remainingUsd = parseFloat(credits.remaining_balance_usd);
    out.nextCreditsAt = credits.next_credits_at;
    out.obtainedAt = acct.obtained_at;
  } catch (e) {
    out.error = e.message;
  }
  return out;
}

// ---- 输出 ----
const ts2date = ts => ts ? new Date(ts * 1000).toISOString().slice(0, 10) : '-';
const iso2date = iso => iso ? iso.slice(0, 10) : '-';

const rows = [];
for (const f of files) {
  process.stdout.write(`查询 ${path.basename(f)} ...`);
  const r = await queryAccount(f);
  rows.push(r);
  console.log(r.error ? `  ✗ ${r.error}` : '  ✓');
}

console.log(`
=========================================================
  账号额度一览 (${new Date().toISOString().slice(0, 10)})
=========================================================
| 账号 | 邮箱 | 档位 | 状态 | 剩余额度 | 额度上限 | 试用剩余 | 订阅到期 | 下期发放 |
|---|---|---|---|---|---|---|---|---|`);

for (const r of rows) {
  if (r.error) {
    console.log(`| ${r.tag} | ${r.email} | — | ✗ ${r.error} | | | | | |`);
    continue;
  }
  const trial = r.trialDaysLeft != null ? `${r.trialDaysLeft}天` : '—';
  console.log(
    `| ${r.tag} | ${r.email} | ${r.tier} | ${r.status} | ${r.remaining.toFixed(2)} ($${r.remainingUsd.toFixed(2)}) | ${r.total.toFixed(0)} | ${trial} | ${ts2date(r.periodEnd)} | ${iso2date(r.nextCreditsAt)} |`
  );
}

const ok = rows.filter(r => !r.error);
if (ok.length) {
  const sum = ok.reduce((a, r) => a + r.remaining, 0);
  console.log(`\n合计可用额度: ${sum.toFixed(2)} credits ($${ok.reduce((a, r) => a + r.remainingUsd, 0).toFixed(2)})，共 ${ok.length} 个有效账号`);
}
if (rows.some(r => r.error)) {
  console.log(`\n[!] ${rows.filter(r => r.error).length} 个账号查询失败（token 可能过期，重新授权: node scripts/raycast_register.mjs <标签>）`);
}

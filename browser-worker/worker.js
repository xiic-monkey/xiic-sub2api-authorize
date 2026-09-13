#!/usr/bin/env node
'use strict';

// 浏览器 worker（与 xiic-crm 同构：playwright-core）。
// 由 Rust 侧通过子进程调用，也由 Tauri 桌面端调用（后者用 --progress 流式读进度）。
//
//   node worker.js check                                          # 检测可用浏览器引擎
//   node worker.js open  <url> [--out png] [--wait ms] [--html]
//   node worker.js gate  <url> [--cdk 字符串] [--out-dir 目录] [--wait ms]
//   node worker.js fetch <url> --emails-file <文件> | --emails "a@x\nb@y" [--cdk 字符串] [--max ms] [--then-open <url>]
//
// 通用选项：
//   --browser <chrome|chromium|msedge|...>  浏览器引擎。
//       `chrome`  = 唤醒本机已装的 Google Chrome（channel:'chrome'），不下载 Chromium（桌面端默认）；
//       缺省/`chromium` = playwright 内置 Chromium（服务器/Docker 用）。
//   --progress                              以 NDJSON 逐行输出进度事件（供 Tauri 实时显示）：
//       {"event":"step","step":"open","msg":"..."}
//       {"event":"poll","t":25,"stat":"...","dlAll":false,"copyAll":false}
//       {"event":"done","result":{...}}      最后一行，含完整结果
//       非 progress 模式：仍只在结尾输出一次格式化 JSON（保持 CLI 行为不变）。
//
// `gate` 是「接码平台」门页流程：
//   1. 打开 URL，检测当前是「未进入(pre)」还是「已进入(post，页面记住了会话)」；
//   2. 若未进入且提供了 CDK：填 #gateCdk → 点 #gateEnter → 等待进入后页面；
//   3. 再检测一次状态，前后各截一张图 + 抽表单结构 + 存 HTML，供人工核对。
//
// `fetch` 是「获取令牌(重授权) → 转 sub2api 凭证」完整链路，**不落盘**：
//   1. 确保已进入（门页则填 CDK 进入；已记住会话则跳过）；
//   2. 填 #emails(换行) → 点 #go(获取令牌) → 每 5s 轮询；
//   3. 完成后点「复制全部」(#copyAll)，从剪切板读回 401 的 session 结果；
//   4. 打开 CPA/Sub2API 页，把该结果贴进左边 #session-input，
//      右边 #output 自动出 sub2api 凭证（含 refresh_token/rt），再点「复制输出」。
//   所有结果只走 stdout JSON（clipboard / cpaPage.output），不写文件。

const { chromium } = require('playwright-core');
const fs = require('fs');
const path = require('path');

// 老板记下的同站备用入口（CPA/Sub2API 渠道页），fetch 完成后打开。
const CPA_PAGE_URL = 'https://zh.kyon888.xyz/CPAandSub2API/';

// ---- 进度事件（NDJSON）-----------------------------------------------------
let PROGRESS = false;
function emit(ev) {
  if (!PROGRESS) return;
  try {
    process.stdout.write(JSON.stringify(ev) + '\n');
  } catch (_) {
    /* 忽略 EPIPE */
  }
}
function step(name, msg, extra) {
  emit(Object.assign({ event: 'step', step: name, msg }, extra || {}));
}

function parseArgs(argv) {
  const out = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--out') out.out = argv[++i];
    else if (a === '--out-dir') out.outDir = argv[++i];
    else if (a === '--wait') out.wait = parseInt(argv[++i], 10);
    else if (a === '--cdk') out.cdk = argv[++i];
    else if (a === '--emails-file') out.emailsFile = argv[++i];
    else if (a === '--emails') out.emails = argv[++i];
    else if (a === '--max') out.max = parseInt(argv[++i], 10);
    else if (a === '--then-open') out.thenOpen = argv[++i];
    else if (a === '--browser') out.browser = argv[++i];
    else if (a === '--html') out.html = true;
    else if (a === '--progress') out.progress = true;
    else out._.push(a);
  }
  return out;
}

// ---- 浏览器引擎选择 --------------------------------------------------------
// 缺省=playwright 内置 Chromium；给了 --browser 则用系统对应渠道（chrome/msedge/...）。
function launchOptions(args) {
  const opt = { headless: true };
  const b = (args.browser || '').toLowerCase().trim();
  if (b && b !== 'chromium' && b !== 'bundled') opt.channel = b;
  return opt;
}

// 启动浏览器；系统浏览器缺失时给出可操作报错（而不是抛一坨 playwright 堆栈）。
async function launchBrowser(args) {
  const b = (args.browser || '').toLowerCase().trim();
  try {
    return await chromium.launch(launchOptions(args));
  } catch (e) {
    const msg = String(e && e.message);
    if (b && /not found|not installed|is not supported|Executable doesn't exist|Can't find/i.test(msg)) {
      console.error(
        `[worker] 无法启动浏览器 "${b}"：本机未找到该引擎。\n` +
          `  请安装对应浏览器，或改用内置 Chromium（去掉 --browser，并执行 npx playwright-core install chromium）。`
      );
      process.exit(4);
    }
    // 内置 Chromium 未下载
    if (/Executable doesn't exist|please run|npx playwright/i.test(msg)) {
      console.error(
        `[worker] 内置 Chromium 未安装。请在 browser-worker/ 下执行：\n` +
          `  npx playwright-core install chromium\n` +
          `或改用系统浏览器：--browser chrome`
      );
      process.exit(4);
    }
    throw e;
  }
}

// 抽关键表单/链接结构（门页与进入后页都用它区分状态）。
async function extractFields(page) {
  return page.$$eval(
    'input,button,select,textarea,a[href]',
    (els) =>
      els
        .map((e) => ({
          tag: e.tagName,
          type: e.getAttribute('type'),
          name: e.getAttribute('name'),
          id: e.id,
          placeholder: e.getAttribute('placeholder'),
          text: (e.textContent || '').trim().slice(0, 48),
        }))
        .filter(
          (f) =>
            f.text || f.placeholder || f.type || f.tag === 'INPUT' || f.tag === 'BUTTON'
        )
        .slice(0, 80)
  );
}

// 探测一组候选「进入后才可见」标记的真实可见性，用于把 `if` 定在正向特征上。
const MARKER_IDS = [
  'gate', 'gateCdk', 'gateEnter', 'mergeBtn', 'mergeBanner',
  'stat', 'jobs', 'work', 'go', 'left', 'newCdk', 'newLeft',
];
async function probeMarkers(page) {
  const out = {};
  for (const id of MARKER_IDS) {
    out[id] = await page.isVisible('#' + id).catch(() => null);
  }
  return out;
}

const ENTER_MARKERS = ['stat', 'work', 'go', 'left'];
// 是否已进入（页面记住了会话）：功能区正向标记任一可见，或门控件已隐藏。
async function isEntered(page) {
  const m = await probeMarkers(page);
  const marked = ENTER_MARKERS.some((k) => m[k]);
  const gateVisible =
    (await page.isVisible('#gateCdk').catch(() => false)) &&
    (await page.isVisible('#gateEnter').catch(() => false));
  return marked || !gateVisible;
}

// 若还在门页且给了 CDK：填 #gateCdk + 点 #gateEnter 进入；返回进入前后状态。
async function ensureEntered(page, cdk, errors) {
  const gateVisible =
    (await page.isVisible('#gateCdk').catch(() => false)) &&
    (await page.isVisible('#gateEnter').catch(() => false));
  const preEntered = await isEntered(page);
  const atGate = gateVisible && !preEntered;
  if (atGate) {
    if (cdk) {
      step('gate', '检测到门页，填入 CDK 并点击进入');
      await page
        .fill('#gateCdk', cdk)
        .catch((e) => errors.push('fill-gate: ' + e.message));
      await page
        .click('#gateEnter')
        .catch((e) => errors.push('click-gate: ' + e.message));
      await page.waitForTimeout(3500);
    } else {
      errors.push('no-cdk: 在门页但未提供 CDK，无法进入');
    }
  } else if (preEntered) {
    step('gate', '页面记住了会话，已是进入后状态，跳过门页');
  }
  const postEntered = await isEntered(page);
  return { preEntry: atGate, postEntry: postEntered };
}

async function isEnabled(page, sel) {
  const el = await page.$(sel).catch(() => null);
  if (!el) return false;
  return el.isEnabled().catch(() => false);
}

// ---- check：检测可用浏览器引擎 --------------------------------------------
async function checkCmd(args) {
  const out = { chrome: false, chromium: false, chromeError: null, chromiumError: null };
  try {
    const b = await chromium.launch({ channel: 'chrome', headless: true });
    await b.close();
    out.chrome = true;
  } catch (e) {
    out.chromeError = String((e && e.message) || e).split('\n')[0].slice(0, 200);
  }
  try {
    const b = await chromium.launch({ headless: true });
    out.chromeVersion = b.version();
    await b.close();
    out.chromium = true;
  } catch (e) {
    out.chromiumError = String((e && e.message) || e).split('\n')[0].slice(0, 200);
  }
  console.log(JSON.stringify(out, null, 2));
}

async function openCmd(args) {
  const url = args._[0];
  if (!url) {
    console.error('缺少 URL');
    process.exit(2);
  }
  const waitMs = Number.isFinite(args.wait) ? args.wait : 2500;

  step('open', '启动浏览器并打开：' + url);
  const browser = await launchBrowser(args);
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('requestfailed', (r) =>
    errors.push('reqfail: ' + r.url() + ' ' + (r.failure() && r.failure().errorText))
  );

  await page
    .goto(url, { waitUntil: 'domcontentloaded', timeout: 30000 })
    .catch((e) => errors.push('goto: ' + e.message));
  await page.waitForTimeout(waitMs);

  const out = args.out || '/tmp/sub2op-shot.png';
  await page.screenshot({ path: out, fullPage: true });

  const content = await page.content();
  const formFields = await extractFields(page);
  const result = {
    ok: true,
    url: page.url(),
    title: await page.title(),
    screenshot: out,
    htmlBytes: content.length,
    formFields,
    errors,
  };

  if (PROGRESS) emit({ event: 'done', result });
  else {
    console.log(JSON.stringify(result, null, 2));
    if (args.html) {
      console.log('---HTML---');
      console.log(content);
    }
  }

  await browser.close();
}

// 门页流程：检测状态 → （可选）填 CDK 进入 → 再检测 → 双状态落盘。
async function gateCmd(args) {
  const url = args._[0];
  if (!url) {
    console.error('缺少 URL');
    process.exit(2);
  }
  const cdk = args.cdk || '';
  const waitMs = Number.isFinite(args.wait) ? args.wait : 3500;
  const outDir = args.outDir || '/tmp';
  if (!fs.existsSync(outDir)) fs.mkdirSync(outDir, { recursive: true });

  step('open', '打开门页：' + url);
  const browser = await launchBrowser(args);
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('requestfailed', (r) =>
    errors.push('reqfail: ' + r.url() + ' ' + (r.failure() && r.failure().errorText))
  );

  await page
    .goto(url, { waitUntil: 'domcontentloaded', timeout: 30000 })
    .catch((e) => errors.push('goto: ' + e.message));
  await page.waitForTimeout(Math.min(waitMs, 2500));

  // 门页特征：#gateCdk（CDK 输入）+ #gateEnter（进入按钮）。
  // 重要：未进入的门页里，下方功能控件（#emails/#go/#stat…）已存在于 DOM，
  // 只是「进入后」才显示出来。所以判定要看「可见性」+「进入后正向标记」。
  const gateVisible =
    (await page.isVisible('#gateCdk').catch(() => false)) &&
    (await page.isVisible('#gateEnter').catch(() => false));
  const preScreenshot = path.join(outDir, 'gate-pre.png');
  await page.screenshot({ path: preScreenshot, fullPage: true });
  const preFields = await extractFields(page);
  const preHtml = await page.content();
  fs.writeFileSync(path.join(outDir, 'gate-pre.html'), preHtml);
  const preTitle = await page.title();
  const preMarkers = await probeMarkers(page);
  const preEntered =
    preMarkers.stat || preMarkers.work || preMarkers.go || preMarkers.left;
  const hasGate = gateVisible && !preEntered;

  step('detect', hasGate ? '当前未进入（在门页）' : '当前已进入（页面记住了会话）');

  // ---- 若未进入且给了 CDK：填 CDK → 点进入 ----
  let entered = false;
  if (hasGate && cdk) {
    step('gate', '填入 CDK 并点击进入');
    await page.fill('#gateCdk', cdk).catch((e) => errors.push('fill: ' + e.message));
    await page.click('#gateEnter').catch((e) => errors.push('click: ' + e.message));
    entered = true;
    await page.waitForTimeout(waitMs);
  } else if (hasGate) {
    errors.push('no-cdk: 未进入且未提供 CDK，跳过填表/点击');
  }

  const postGateVisible =
    (await page.isVisible('#gateCdk').catch(() => false)) &&
    (await page.isVisible('#gateEnter').catch(() => false));
  const postMarkers = await probeMarkers(page);
  const postEntered =
    postMarkers.stat || postMarkers.work || postMarkers.go || postMarkers.left;
  const alreadyEntered = postEntered || !postGateVisible;

  // 进入后若页面上仍有可见的 CDK 输入框（如重新校验用），仅填不点进入。
  if (alreadyEntered && cdk) {
    const cdkSel = await page.$(
      '#gateCdk:visible, input[name*="cdk" i]:visible, input[placeholder*="cdk" i]:visible'
    );
    if (cdkSel) {
      await cdkSel.fill(cdk).catch((e) => errors.push('post-fill: ' + e.message));
    }
  }

  const postScreenshot = path.join(outDir, 'gate-post.png');
  await page.screenshot({ path: postScreenshot, fullPage: true });
  const postFields = await extractFields(page);
  const postHtml = await page.content();
  fs.writeFileSync(path.join(outDir, 'gate-post.html'), postHtml);
  const postTitle = await page.title();

  const result = {
    ok: true,
    url: page.url(),
    detection: {
      preEntry: hasGate,
      postEntry: alreadyEntered,
      enteredWithCdk: entered,
    },
    pre: {
      title: preTitle,
      screenshot: preScreenshot,
      htmlFile: path.join(outDir, 'gate-pre.html'),
      htmlBytes: preHtml.length,
      fields: preFields,
      markers: preMarkers,
    },
    post: {
      title: postTitle,
      screenshot: postScreenshot,
      htmlFile: path.join(outDir, 'gate-post.html'),
      htmlBytes: postHtml.length,
      fields: postFields,
      markers: postMarkers,
    },
    errors,
  };

  if (PROGRESS) emit({ event: 'done', result });
  else console.log(JSON.stringify(result, null, 2));

  await browser.close();
}

// 获取令牌（重授权）流程：
//   确保已进入 → 填 #emails(换行) → 点 #go(获取令牌) → 每 5s 轮询，
//   靠 #stat 文案 / #dlAll·#copyAll 可用态 / 状态稳定 判定完成 →
//   点「复制全部」读剪切板 → 打开 CPA 页贴入 → 读 #output 的 sub2api 凭证。
async function fetchCmd(args) {
  const url = args._[0];
  if (!url) {
    console.error('缺少 URL');
    process.exit(2);
  }
  const cdk = args.cdk || '';
  let emails = [];
  if (args.emailsFile) {
    if (!fs.existsSync(args.emailsFile)) {
      console.error('邮箱文件不存在: ' + args.emailsFile);
      process.exit(2);
    }
    emails = fs
      .readFileSync(args.emailsFile, 'utf8')
      .split(/[\r\n]+/)
      .map((s) => s.trim())
      .filter(Boolean);
  } else if (typeof args.emails === 'string' && args.emails.trim()) {
    // 直接传换行/逗号分隔的邮箱串（Tauri 用这条，免写临时文件）
    emails = args.emails
      .split(/[\r\n,]+/)
      .map((s) => s.trim())
      .filter(Boolean);
  } else if (args._[1]) {
    emails = args._[1]
      .split(/[\r\n,]+/)
      .map((s) => s.trim())
      .filter(Boolean);
  }
  if (emails.length === 0) {
    console.error('未提供邮箱（用 --emails "<换行分隔>" 或 --emails-file <文件>）');
    process.exit(2);
  }
  const thenOpen = args.thenOpen || CPA_PAGE_URL;
  const pollMs = 5000;
  const maxMs = Number.isFinite(args.max) ? args.max : 150000; // 默认最多等 2.5 分钟

  step('open', '打开门页：' + url);
  const browser = await launchBrowser(args);
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('requestfailed', (r) =>
    errors.push('reqfail: ' + r.url() + ' ' + (r.failure() && r.failure().errorText))
  );

  await page
    .goto(url, { waitUntil: 'domcontentloaded', timeout: 30000 })
    .catch((e) => errors.push('goto: ' + e.message));
  await page.waitForTimeout(2500);

  // 1) 确保已进入（门页则填 CDK 进入；已记住会话则跳过）
  const entered = await ensureEntered(page, cdk, errors);
  if (!entered.postEntry) {
    const msg = '未能进入页面（CDK 无效或页面结构变化）';
    if (PROGRESS) emit({ event: 'error', msg, errors });
    console.error(msg + '。errors=' + JSON.stringify(errors));
    await browser.close();
    process.exit(3);
  }

  // 2) 填邮箱（换行分隔）
  step('emails', `填入 ${emails.length} 个邮箱`);
  await page
    .fill('#emails', emails.join('\n'))
    .catch((e) => errors.push('fill-emails: ' + e.message));

  // 3) 点「获取令牌」
  step('go', '已点击「获取令牌」，开始轮询（每 5 秒）');
  await page.click('#go').catch((e) => errors.push('click-go: ' + e.message));
  const startedAt = Date.now();

  // 4) 每 5s 轮询，看页面变化
  const pollLog = [];
  let lastStat = '';
  let stableCount = 0;
  let done = false;
  let finalState = null;
  // 完成信号：#stat 出现完成/成功/失败字样，或 #dlAll/#copyAll 变为可用
  const isComplete = (stat, dl, copy) =>
    /完成|成功|结束|已获取|done|finish|失败\s*\d+|成功\s*\d+/i.test(stat) ||
    (dl && copy);

  while (Date.now() - startedAt < maxMs) {
    await page.waitForTimeout(pollMs);
    const stat = ((await page.textContent('#stat').catch(() => '')) || '').trim();
    const errTx = ((await page.textContent('#err').catch(() => '')) || '').trim();
    const emailsVal = (await page.inputValue('#emails').catch(() => '')) || '';
    const dlEnabled = await isEnabled(page, '#dlAll');
    const copyEnabled = await isEnabled(page, '#copyAll');
    const elapsed = Math.round((Date.now() - startedAt) / 1000);
    const snap = {
      t: elapsed + 's',
      stat: stat.slice(0, 200),
      err: errTx.slice(0, 200),
      emailsLen: emailsVal.length,
      dlAll: dlEnabled,
      copyAll: copyEnabled,
    };
    pollLog.push(snap);
    emit(Object.assign({ event: 'poll' }, snap));
    console.error(
      `[poll ${elapsed}s] stat=${snap.stat} dlAll=${dlEnabled} copyAll=${copyEnabled}`
    );

    if (isComplete(stat, dlEnabled, copyEnabled)) {
      done = true;
      finalState = snap;
      break;
    }
    if (stat !== '' && stat === lastStat) stableCount++;
    else stableCount = 0;
    lastStat = stat;
    if (stableCount >= 3) {
      done = true;
      finalState = snap;
      console.error('[poll] #stat 连续 3 次不变，视为完成/停滞');
      break;
    }
  }
  if (!done) {
    finalState = {
      note: '超时未检测到完成信号',
      elapsed: Math.round((Date.now() - startedAt) / 1000) + 's',
    };
  }
  step('poll-done', done ? '检测到完成信号' : '超时结束', { finalState });

  // 5) 点「复制全部」→ 回读剪切板（不落盘）
  step('copy', '点击「复制全部」并读取剪切板');
  try {
    await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
  } catch (e) {
    errors.push('perm: ' + e.message);
  }
  let clipboard = '';
  let copyClicked = false;
  try {
    await page.click('#copyAll', { timeout: 5000 });
    copyClicked = true;
    await page.waitForTimeout(800);
    clipboard = await page.evaluate(() => navigator.clipboard.readText());
  } catch (e) {
    errors.push('copy: ' + e.message);
  }
  if (!clipboard) {
    clipboard = (await page.inputValue('#emails').catch(() => '')) || '';
    if (clipboard) errors.push('clipboard-empty: 回退 #emails 值');
  }

  // 6) 打开 CPA/Sub2API 页：贴左边 #session-input → 右边 #output 出 sub2api 凭证（含 rt）
  step('cpa', '打开 CPA 页并转换（贴入 → 读取输出）');
  let cpaPage = null;
  try {
    const p2 = await browser.newPage();
    await p2.context().grantPermissions(['clipboard-read', 'clipboard-write']).catch(() => {});
    await p2
      .goto(thenOpen, { waitUntil: 'domcontentloaded', timeout: 30000 })
      .catch((e) => errors.push('cpa-goto: ' + e.message));
    await p2.waitForTimeout(2500);
    // 先点 sub2api 渠道，确保输出为 sub2api 凭证格式
    await p2
      .click('button:has-text("sub2api")')
      .catch((e) => errors.push('cpa-channel: ' + e.message));
    if (clipboard) {
      await p2
        .fill('#session-input', clipboard)
        .catch((e) => errors.push('cpa-fill: ' + e.message));
    } else {
      errors.push('cpa: 剪切板为空，无法贴入 #session-input');
    }
    let cpaOut = '';
    const t0 = Date.now();
    while (Date.now() - t0 < 15000) {
      await p2.waitForTimeout(1500);
      cpaOut = (await p2.inputValue('#output').catch(() => '')) || '';
      if (/refresh_token|"rt"|rt-/i.test(cpaOut)) break;
    }
    await p2.click('#copy-output').catch((e) => errors.push('cpa-copy: ' + e.message));
    cpaPage = { url: p2.url(), title: await p2.title(), output: cpaOut };
    await p2.close();
  } catch (e) {
    errors.push('cpa: ' + e.message);
  }

  const result = {
    ok: true,
    url: page.url(),
    entered,
    emailsCount: emails.length,
    done,
    finalState,
    copyClicked,
    clipboard, // 重授权结果（来自剪切板，不落盘）
    cpaPage, // CPA 转换出的 sub2api 凭证（含 refresh_token）
    pollLog,
    errors,
  };

  if (PROGRESS) emit({ event: 'done', result });
  else console.log(JSON.stringify(result, null, 2));

  await browser.close();
}

(async () => {
  const args = parseArgs(process.argv.slice(2));
  PROGRESS = !!args.progress;
  const cmd = args._.shift();
  if (cmd === 'check') {
    await checkCmd(args);
  } else if (cmd === 'open') {
    await openCmd(args);
  } else if (cmd === 'gate') {
    await gateCmd(args);
  } else if (cmd === 'fetch') {
    await fetchCmd(args);
  } else {
    console.error('未知命令: ' + cmd);
    process.exit(2);
  }
})().catch((e) => {
  const msg = String((e && e.stack) || e);
  if (PROGRESS) emit({ event: 'error', msg });
  console.error('worker 失败:', msg);
  process.exit(1);
});

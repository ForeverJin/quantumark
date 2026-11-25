// 备份管理器前端：原生 JS，无构建步骤。
// 约定：所有请求都带 X-QM-CSRF；401 统一回到登录页。

const S = {
  csrf: null,
  setupRequired: false,
  view: 'dashboard',
  jobs: [],
  dests: [],
  overview: null,
  policy: null,
  jobForm: null,
  destForm: null,
  probe: null,
  selftest: null,
  createdToken: null,
  ageKey: null,
  runFilter: '',
  log: { runId: null, offset: 0, timer: null, text: '' },
  remotePick: null,
  localPick: null,
  error: null,
};

const app = document.getElementById('app');

function brandMark() {
  return `<span class="brand__mark" aria-hidden="true">
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
      <path d="M12 3.2 5 7.2v9.6l7 4 7-4V7.2l-7-4Z"/>
      <path d="m8.8 12 2.1 2.1 4.4-4.4"/>
    </svg>
  </span>`;
}

function navIcon(name) {
  return `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${NAV_ICONS[name] || ''}</svg>`;
}

// ---------- 工具 ----------

function esc(v) {
  return String(v == null ? '' : v).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  })[c]);
}

function toast(msg, bad) {
  const t = document.getElementById('toast');
  t.textContent = msg;
  t.className = 'toast show' + (bad ? ' bad' : '');
  clearTimeout(S._toastTimer);
  S._toastTimer = setTimeout(() => { t.className = 'toast'; }, 3800);
}

function fmtBytes(n) {
  n = Number(n || 0);
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0, v = n;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return i === 0 ? n + ' B' : v.toFixed(1) + ' ' + units[i];
}

function fmtTime(ts) {
  if (!ts) return '—';
  const d = new Date(ts * 1000);
  const p = (x) => String(x).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function statusBadge(status, running) {
  if (running) return '<span class="badge run">运行中</span>';
  const map = {
    success: ['ok', '成功'],
    partial: ['warn', '部分失败'],
    failed: ['bad', '失败'],
    cancelled: ['', '已停止'],
    running: ['run', '运行中'],
  };
  const [cls, text] = map[status] || ['', '未运行'];
  return `<span class="badge ${cls}">${text}</span>`;
}

async function api(path, method = 'GET', body = null) {
  const headers = { 'Content-Type': 'application/json' };
  if (S.csrf) headers['X-QM-CSRF'] = S.csrf;
  const res = await fetch('/api' + path, {
    method,
    headers,
    body: body == null ? undefined : JSON.stringify(body),
  });
  if (res.status === 401) {
    S.csrf = null;
    S.error = '会话已过期，请重新登录';
    renderAuth();
    throw new Error('未登录');
  }
  let data = {};
  try { data = await res.json(); } catch (e) { /* 空响应 */ }
  if (!res.ok) throw new Error(data.error || ('请求失败：HTTP ' + res.status));
  return data;
}

// ---------- 启动与登录 ----------

async function boot() {
  try {
    const me = await (await fetch('/api/me')).json();
    if (me.logged_in) {
      S.csrf = me.csrf;
      await loadAll();
      render();
    } else {
      S.setupRequired = !!me.needs_setup;
      renderAuth();
    }
  } catch (e) {
    app.className = '';
    app.innerHTML = `<div class="panel"><div class="errbox">加载失败：${esc(e.message)}</div></div>`;
  }
}

async function loadAll() {
  const [jobs, dests, overview, policy] = await Promise.all([
    api('/jobs'), api('/destinations'), api('/overview'), api('/policy'),
  ]);
  S.jobs = jobs.items || [];
  S.dests = dests.items || [];
  S.overview = overview;
  S.policy = policy;
}

function renderAuth() {
  app.className = '';
  const setup = S.setupRequired;
  document.title = setup ? '初始设置 · QuantumArk' : '登录 · QuantumArk';
  app.innerHTML = `
    <div class="auth-screen">
      <div class="auth-card">
        <div class="auth-brand">
          ${brandMark()}
          <div>
            <h1>QuantumArk</h1>
            <p>自托管备份控制台</p>
          </div>
        </div>
        <p>${setup ? '第一次启动，请先设置管理员密码。' : '本地数据经 CD2 安全上传至 115。'}</p>
        ${S.error ? `<div class="errbox">${esc(S.error)}</div>` : ''}
        ${setup ? `
          <label for="setup-code">设置码</label>
          <input id="setup-code" autocomplete="off" placeholder="从容器日志获取设置码">
          <div class="hint">设置码可在容器日志中查看：<span class="mono">docker compose logs quantumark</span></div>
          <label for="setup-password">管理员密码（至少 8 位）</label>
          <input id="setup-password" type="password" autocomplete="new-password" placeholder="设置管理员密码">
          <button class="primary auth-submit" data-act="setup">完成初始设置</button>
        ` : `
          <label for="login-password">管理员密码</label>
          <input id="login-password" type="password" autocomplete="current-password" placeholder="输入管理员密码">
          <button class="primary auth-submit" data-act="login">登录控制台</button>
        `}
        <div class="auth-footnote">QuantumArk · 数据始终通过 CD2 gRPC 接口访问 115</div>
      </div>
    </div>`;
}

async function doLogin() {
  const password = document.getElementById('login-password').value;
  S.error = null;
  try {
    const res = await fetch('/api/login', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password }),
    }).then(async (r) => ({ ok: r.ok, data: await r.json().catch(() => ({})) }));
    if (!res.ok) throw new Error(res.data.error || '登录失败');
    S.csrf = res.data.csrf;
    await loadAll();
    S.view = 'dashboard';
    render();
  } catch (e) {
    S.error = e.message;
    renderAuth();
  }
}

async function doSetup() {
  const code = document.getElementById('setup-code').value;
  const password = document.getElementById('setup-password').value;
  S.error = null;
  try {
    const res = await fetch('/api/setup', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ code, password }),
    }).then(async (r) => ({ ok: r.ok, data: await r.json().catch(() => ({})) }));
    if (!res.ok) throw new Error(res.data.error || '设置失败');
    S.csrf = res.data.csrf;
    S.setupRequired = false;
    await loadAll();
    render();
  } catch (e) {
    S.error = e.message;
    renderAuth();
  }
}

// ---------- 外壳 ----------

const TABS = [
  ['dashboard', '仪表盘', '运行概览', '查看备份运行状态与最近任务'],
  ['jobs', '备份任务', '备份任务', '配置目录、归档与 MySQL 备份策略'],
  ['runs', '运行记录', '运行记录', '追踪每次备份的结果与详细日志'],
  ['dests', '目的地', '目的地', '管理 CD2 连接与云端备份根路径'],
  ['settings', '设置', '系统设置', '调整限速策略、管理员密码与运行参数'],
];

const NAV_ICONS = {
  dashboard: '<rect width="7" height="9" x="3" y="3" rx="1"/><rect width="7" height="5" x="14" y="3" rx="1"/><rect width="7" height="9" x="14" y="12" rx="1"/><rect width="7" height="5" x="3" y="16" rx="1"/>',
  jobs: '<path d="m7.5 4.27 9 5.15"/><path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>',
  runs: '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/>',
  dests: '<rect width="18" height="8" x="3" y="3" rx="2"/><rect width="18" height="8" x="3" y="13" rx="2"/><path d="M7 7h.01"/><path d="M7 17h.01"/>',
  settings: '<path d="M4 21v-7"/><path d="M4 10V3"/><path d="M12 21v-9"/><path d="M12 8V3"/><path d="M20 21v-5"/><path d="M20 12V3"/><path d="M1 14h6"/><path d="M9 8h6"/><path d="M17 16h6"/>',
};

function render() {
  app.className = '';
  const current = TABS.find(([id]) => id === S.view) || TABS[0];
  const [viewId, label, title, description] = current;
  const tabs = TABS.map(([id, navLabel]) =>
    `<button class="${S.view === id ? 'active' : ''}" data-act="tab" data-view="${id}" aria-current="${S.view === id ? 'page' : 'false'}">
      ${navIcon(id)}<span>${navLabel}</span>
    </button>`
  ).join('');
  const cooldown = S.overview && S.overview.cooldown_until;
  const jobCount = S.overview && S.overview.jobs ? S.overview.jobs.length : S.jobs.length;
  let inner = '';
  if (S.view === 'dashboard') inner = viewDashboard();
  else if (S.view === 'jobs') inner = viewJobs();
  else if (S.view === 'runs') inner = viewRuns();
  else if (S.view === 'dests') inner = viewDests();
  else inner = viewSettings();

  app.innerHTML = `
    <div class="app-shell">
      <aside class="sidebar">
        <div class="brand">
          ${brandMark()}
          <div>
            <div class="brand__name">QuantumArk</div>
            <div class="brand__tagline">备份管理器</div>
          </div>
        </div>
        <nav class="sidebar-nav" aria-label="主导航">
          <div class="nav-label">工作台</div>
          ${tabs}
        </nav>
        <div class="sidebar-footer">
          <div class="sidebar-footer__row"><span><span class="status-dot"></span>服务在线</span><span class="sidebar-footer__value">v${esc(S.overview ? S.overview.version : '')}</span></div>
          <div class="sidebar-footer__row"><span>传输通道</span><span class="sidebar-footer__value">CD2 → 115</span></div>
        </div>
      </aside>
      <div class="main-shell">
        <header class="main-header">
          <div class="header-meta">
            <span class="badge run">服务在线</span>
            <span class="version">v${esc(S.overview ? S.overview.version : '')}</span>
            ${cooldown ? `<span class="badge bad">风控冷却至 ${esc(cooldown)}</span>` : ''}
          </div>
          <span class="spacer"></span>
          <button class="header-button icon-only" data-act="refresh" aria-label="刷新数据" title="刷新数据">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 11a8.1 8.1 0 0 0-15.5-2M4 4v5h5"/><path d="M4 13a8.1 8.1 0 0 0 15.5 2M20 20v-5h-5"/></svg>
          </button>
          <button class="header-button" data-act="logout" aria-label="退出登录" title="退出登录">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 17l5-5-5-5"/><path d="M15 12H3"/><path d="M15 3h4a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-4"/></svg>
            <span>退出登录</span>
          </button>
        </header>
        <main class="wrap">
          <div class="page-head">
            <div>
              <div class="page-head__eyebrow">QuantumArk / Workspace</div>
              <h2>${title}</h2>
              <p>${description}</p>
            </div>
            <div class="page-head__meta">
              <span class="badge">${jobCount} 个任务</span>
              ${S.overview && S.overview.destinations != null ? `<span class="badge">${S.overview.destinations} 个目的地</span>` : ''}
            </div>
          </div>
          ${inner}
        </main>
      </div>
    </div>`;
  document.title = `${title} · QuantumArk`;
}

// ---------- 仪表盘 ----------

function viewDashboard() {
  const jobs = S.overview ? S.overview.jobs : [];
  const running = jobs.filter((j) => j.running).length;
  const dataDir = S.overview ? S.overview.data_dir : '';
  const cards = `
    <div class="cards">
      <div class="card accent">
        <div class="metric-top"><div class="label">备份任务</div><div class="metric-icon">${navIcon('jobs')}</div></div>
        <div class="value">${jobs.length}</div>
        <div class="metric-hint">已配置的备份任务总数</div>
      </div>
      <div class="card">
        <div class="metric-top"><div class="label">正在运行</div><div class="metric-icon">${navIcon('runs')}</div></div>
        <div class="value">${running}</div>
        <div class="metric-hint">${running ? '有任务正在传输数据' : '当前没有运行中的任务'}</div>
      </div>
      <div class="card">
        <div class="metric-top"><div class="label">目的地</div><div class="metric-icon">${navIcon('dests')}</div></div>
        <div class="value">${S.overview ? S.overview.destinations : 0}</div>
        <div class="metric-hint">通过 CD2 接入的云端目标</div>
      </div>
      <div class="card">
        <div class="metric-top"><div class="label">数据目录</div><div class="metric-icon">${navIcon('settings')}</div></div>
        <div class="value text">${esc(dataDir || '—')}</div>
        <div class="metric-hint">任务与密钥的持久化位置</div>
      </div>
    </div>`;

  if (!jobs.length) {
    return cards + `<div class="panel"><div class="empty-state">还没有备份任务。请先到「目的地」配置 CD2，再创建第一个任务。</div></div>`;
  }
  const rows = jobs.map((j) => {
    const last = j.last_run;
    return `<tr>
      <td>${esc(j.name)}<div class="sortcell">${esc(j.kind)} · ${esc(j.remote_dir)}</div></td>
      <td>${statusBadge(last && last.status, j.running)}</td>
      <td>${last ? esc(last.started_at_text) : '—'}</td>
      <td>${last && last.stats ? `${esc(last.stats.uploaded || 0)} 个 / ${fmtBytes(last.stats.uploaded_bytes)}` : '—'}</td>
      <td>${esc(j.next_run || (j.cron ? j.cron : '仅手动'))}</td>
      <td><div class="row-actions">
        ${j.running
          ? `<button class="small danger" data-act="stop-job" data-id="${j.id}">停止</button>`
          : `<button class="small primary" data-act="run-job" data-id="${j.id}">立即运行</button>`}
        ${last ? `<button class="small" data-act="open-log" data-run="${last.id}">日志</button>` : ''}
      </div></td>
    </tr>`;
  }).join('');
  return cards + `<div class="panel">
    <div class="inline" style="justify-content:space-between;margin-bottom:12px">
      <div><h2 style="margin:0">最近任务</h2><div class="hint">最近一次运行状态与下次调度</div></div>
      <span class="badge">${jobs.length} 个</span>
    </div>
    <table><thead><tr><th>任务</th><th>状态</th><th>上次运行</th><th>上次上传</th><th>下次</th><th></th></tr></thead>
    <tbody>${rows}</tbody></table></div>`;
}

// ---------- 任务 ----------

function optionSummary(job) {
  const o = job.options || {};
  if (job.kind === 'mirror') {
    const parts = [`源 ${esc(o.source || '')}`, o.sync_delete ? '同步删除云端' : '只增不删'];
    if (o.delete_local) parts.push(o.local_trash ? '上传后移到回收目录' : '上传后删本地');
    return parts.join(' · ');
  }
  if (job.kind === 'archive') {
    return [`源 ${esc(o.source || '')}`, o.age_recipient ? '加密' : '不加密', `保留 ${esc(job.keep || '不限')}`].join(' · ');
  }
  if (job.kind === 'mysql') {
    const mode = o.mode || 'dump';
    if (mode === 'dump') {
      const dbs = (o.databases || []).length ? o.databases.join(',') : '全部库';
      return [`连库导出 ${esc(o.host || '')}:${esc(o.port || 3306)}`, esc(dbs),
        o.age_recipient ? '加密' : '不加密', `保留 ${esc(job.keep || '不限')}`].join(' · ');
    }
    const parts = [`${mode === 'mirror' ? '同步数据目录' : '打包数据目录'} ${esc(o.source || '')}`];
    if (mode === 'archive') {
      parts.push(o.age_recipient ? '加密' : '不加密', `保留 ${esc(job.keep || '不限')}`);
    }
    return parts.join(' · ');
  }
  return '';
}

function viewJobs() {
  const rows = S.jobs.map((j) => `<tr>
      <td>${esc(j.name)}${j.enabled ? '' : ' <span class="badge">已停用</span>'}
        <div class="sortcell">${optionSummary(j)}</div></td>
      <td>${esc(j.destination_name)}<div class="sortcell mono">${esc(j.remote_dir)}</div></td>
      <td title="${esc(j.cron || '')}">${esc(describeCronExpr(j.cron))}</td>
      <td>${statusBadge(j.last_run && j.last_run.status, j.running)}</td>
      <td><div class="row-actions">
        ${j.running
          ? `<button class="small danger" data-act="stop-job" data-id="${j.id}">停止</button>`
          : `<button class="small primary" data-act="run-job" data-id="${j.id}">运行</button>`}
        ${j.kind === 'mirror' ? `<button class="small" data-act="dry-run" data-id="${j.id}">预览</button>` : ''}
        <button class="small" data-act="edit-job" data-id="${j.id}">编辑</button>
        <button class="small danger" data-act="del-job" data-id="${j.id}">删除</button>
      </div></td>
    </tr>`).join('');

  return `
    <div class="panel">
      <div class="inline" style="justify-content:space-between">
        <h2 style="margin:0">备份任务</h2>
        <button class="primary" data-act="new-job">新建任务</button>
      </div>
      ${S.jobs.length ? `<table><thead><tr><th>任务</th><th>目的地</th><th>定时</th><th>状态</th><th></th></tr></thead><tbody>${rows}</tbody></table>`
        : '<p class="hint">还没有任务。</p>'}
    </div>
    ${S.jobForm ? viewJobForm() : ''}
    ${S.localPick ? viewLocalPicker() : ''}`;
}

// ---------- 定时设置 ----------

const DOW_LABELS = [
  ['1', '周一'], ['2', '周二'], ['3', '周三'], ['4', '周四'],
  ['5', '周五'], ['6', '周六'], ['0', '周日'],
];

function pad2(n) {
  return String(n).padStart(2, '0');
}

function clampInt(v, min, max) {
  const n = Number(v);
  if (!Number.isFinite(n)) return min;
  return Math.min(max, Math.max(min, Math.trunc(n)));
}

// 把 cron 解析成表单状态
function parseCronToState(cron) {
  const raw = (cron || '').trim();
  if (!raw) return { kind: 'manual' };
  let m = raw.match(/^(\d+)\s+(\d+)\s+\*\s+\*\s+\*$/);
  if (m) return { kind: 'daily', minute: +m[1], hour: +m[2] };
  m = raw.match(/^(\d+)\s+(\d+)\s+\*\s+\*\s+([\d,]+)$/);
  if (m) {
    const days = m[3].split(',').map(Number).filter((d) => d >= 0 && d <= 7).map((d) => (d === 7 ? 0 : d));
    return { kind: 'weekly', minute: +m[1], hour: +m[2], days: days.length ? days : [1] };
  }
  m = raw.match(/^(\d+)\s+(\d+)\s+(\d+)\s+\*\s+\*$/);
  if (m) return { kind: 'monthly', minute: +m[1], hour: +m[2], dom: +m[3] };
  m = raw.match(/^(\d+)\s+\*\s+\*\s+\*\s+\*$/);
  if (m) return { kind: 'hourly', minute: +m[1] };
  return { kind: 'custom', raw };
}

// cron → 中文说明
function describeCronExpr(cron) {
  const st = parseCronToState(cron);
  const hhmm = `${pad2(st.hour || 0)}:${pad2(st.minute || 0)}`;
  switch (st.kind) {
    case 'manual':
      return '不定时（只手动运行）';
    case 'hourly':
      return `每小时第 ${st.minute} 分`;
    case 'daily':
      return `每天 ${hhmm}`;
    case 'weekly':
      return `每周${st.days.map((d) => (DOW_LABELS.find((x) => +x[0] === d) || [, '?'])[1]).join('、')} ${hhmm}`;
    case 'monthly':
      return `每月 ${st.dom} 号 ${hhmm}`;
    default:
      return `自定义：${st.raw}`;
  }
}

// 从表单读回 cron
function readSchedule() {
  const kind = (document.getElementById('s-kind') || {}).value || 'manual';
  const timeVal = (document.getElementById('s-time') || {}).value || '03:00';
  const [hh, mm] = timeVal.split(':').map(Number);
  const hour = clampInt(hh, 0, 23);
  const minute = clampInt(mm, 0, 59);
  if (kind === 'manual') return '';
  if (kind === 'hourly') {
    const m = clampInt((document.getElementById('s-min') || {}).value ?? 0, 0, 59);
    return `${m} * * * *`;
  }
  if (kind === 'daily') return `${minute} ${hour} * * *`;
  if (kind === 'weekly') {
    const days = [...document.querySelectorAll('#s-dows input[data-dow]')]
      .filter((x) => x.checked)
      .map((x) => x.dataset.dow);
    return `${minute} ${hour} * * ${days.length ? days.join(',') : '1'}`;
  }
  if (kind === 'monthly') {
    const dom = clampInt((document.getElementById('s-dom') || {}).value ?? 1, 1, 31);
    return `${minute} ${hour} ${dom} * *`;
  }
  return ((document.getElementById('s-cron') || {}).value || '').trim();
}

function renderSchedule(cron) {
  const st = parseCronToState(cron);
  const kind = st.kind;
  const time = `${pad2(st.hour ?? 3)}:${pad2(st.minute ?? 0)}`;
  let extra = '';
  if (kind === 'daily' || kind === 'weekly' || kind === 'monthly') {
    extra += `<div><label>时间</label><input id="s-time" type="time" value="${time}"></div>`;
  }
  if (kind === 'weekly') {
    const days = new Set(st.days || [1]);
    extra += `<div><label>星期</label><div class="inline" style="flex-wrap:wrap">` +
      DOW_LABELS.map(([v, label]) =>
        `<label class="check" style="margin:0"><input type="checkbox" data-dow="${v}" ${days.has(+v) ? 'checked' : ''}> ${label}</label>`
      ).join('') + `</div></div>`;
  }
  if (kind === 'monthly') {
    extra += `<div><label>每月几号</label><input id="s-dom" type="number" min="1" max="31" value="${st.dom ?? 1}"></div>`;
  }
  if (kind === 'hourly') {
    extra += `<div><label>每小时的几分</label><input id="s-min" type="number" min="0" max="59" value="${st.minute ?? 0}"></div>`;
  }
  if (kind === 'custom') {
    extra += `<div><label>cron 表达式（分 时 日 月 周）</label>
      <input id="s-cron" class="mono" value="${esc(st.raw || '')}" placeholder="17 4 * * *"></div>`;
  }
  return `
    <div class="grid2">
      <div><label>什么时候运行</label>
        <select id="s-kind">
          <option value="manual" ${kind === 'manual' ? 'selected' : ''}>不定时（只手动运行）</option>
          <option value="hourly" ${kind === 'hourly' ? 'selected' : ''}>每小时</option>
          <option value="daily" ${kind === 'daily' ? 'selected' : ''}>每天</option>
          <option value="weekly" ${kind === 'weekly' ? 'selected' : ''}>每周</option>
          <option value="monthly" ${kind === 'monthly' ? 'selected' : ''}>每月</option>
          <option value="custom" ${kind === 'custom' ? 'selected' : ''}>自定义 cron</option>
        </select></div>
      ${extra}
    </div>
    <p class="hint" id="s-preview">${esc(describeCronExpr(cron))}</p>`;
}

function updateSchedulePreview() {
  const el = document.getElementById('s-preview');
  if (el) el.textContent = describeCronExpr(readSchedule());
}

function field(label, id, value, placeholder, type) {
  return `<div><label>${label}</label><input id="${id}" type="${type || 'text'}" value="${esc(value ?? '')}" placeholder="${esc(placeholder || '')}"></div>`;
}

function viewJobForm() {
  const f = S.jobForm;
  const editing = !!f.id;
  const destOptions = S.dests.map((d) =>
    `<option value="${d.id}" ${String(d.id) === String(f.destination_id) ? 'selected' : ''}>${esc(d.name)} (${esc(d.address)})</option>`
  ).join('');
  let kindFields = '';

  if (f.kind === 'mirror') {
    kindFields = `
      <div class="grid2">
        <div><label>源目录（容器内路径）</label>
          <div class="inline"><input id="f-source" value="${esc(f.options.source)}" placeholder="点浏览，从 /host 下选">
          <button class="small" data-act="pick-local">浏览</button></div></div>
        <div><label>包含规则（glob，逗号分隔，留空=全部）</label>
          <input id="f-include" value="${esc((f.options.include || []).join(','))}" placeholder="*.mkv,*.jpg"></div>
      </div>
      <label>排除规则（glob，逗号分隔）</label>
      <input id="f-exclude" value="${esc((f.options.exclude || []).join(','))}">
      <label class="check"><input type="checkbox" id="f-sync-del" ${f.options.sync_delete ? 'checked' : ''}>
        源端删除后，云端也删除（进 115 回收站）</label>
      <label class="check"><input type="checkbox" id="f-del-local" ${f.options.delete_local ? 'checked' : ''}>
        上传校验通过后删除本地文件（本地不留存；源目录必须以可写方式挂载，例如 /upload）</label>
      <label>删本地时先移到（可选，留空=直接删除；必须在源目录之外）</label>
      <input id="f-trash" value="${esc(f.options.local_trash || '')}" placeholder="/upload/.trash">`;
  } else if (f.kind === 'archive') {
    kindFields = `
      <div class="grid2">
        <div><label>源目录（容器内路径）</label>
          <div class="inline"><input id="f-source" value="${esc(f.options.source)}" placeholder="点浏览，从 /host 下选">
          <button class="small" data-act="pick-local">浏览</button></div></div>
        <div><label>zstd 压缩级别（1-19）</label>
          <input id="f-zstd" type="number" min="1" max="19" value="${esc(f.options.zstd_level || 3)}"></div>
      </div>
      <label>排除规则（glob，逗号分隔）</label>
      <input id="f-exclude" value="${esc((f.options.exclude || []).join(','))}">
      ${ageFields(f.options)}`;
  } else {
    const mode = f.options.mode || 'dump';
    const dumpFields = `
      <div class="grid2">
        ${field('数据库地址', 'f-host', f.options.host, '192.168.8.139')}
        ${field('端口', 'f-port', f.options.port || 3306, '3306', 'number')}
        ${field('用户名', 'f-user', f.options.user, 'admin')}
        <div><label>密码（留空=不修改）</label>
          <input id="f-dbpass" type="password" placeholder="${f.has_secret ? '已设置，留空不改' : '数据库密码'}"></div>
      </div>
      <label>库名（逗号分隔，留空=所有非系统库）</label>
      <input id="f-dbs" value="${esc((f.options.databases || []).join(','))}" placeholder="inventory_db,ominscope">
      <div class="grid2">
        <div><label>zstd 压缩级别（1-19）</label>
          <input id="f-zstd" type="number" min="1" max="19" value="${esc(f.options.zstd_level || 3)}"></div>
        <div><label>额外参数（可选，空格分隔）</label>
          <input id="f-extra" value="${esc((f.options.extra_args || []).join(' '))}" placeholder="--skip-triggers"></div>
      </div>
      ${ageFields(f.options)}
      <p class="hint">用 mariadb-dump 导出（兼容 MySQL 8.x），带 --single-transaction：不锁表、不用停库。</p>`;
    const filesFields = `
      <div class="grid2">
        <div><label>数据目录（容器内路径）</label>
          <div class="inline"><input id="f-source" value="${esc(f.options.source)}" placeholder="点浏览，从 /host 下选 datadir">
          <button class="small" data-act="pick-local">浏览</button></div></div>
        <div></div>
      </div>
      <label>排除规则（glob，逗号分隔；默认已含 ibtmp1、#innodb_temp、*.pid、*.sock、*.err）</label>
      <input id="f-exclude" value="${esc((f.options.exclude || []).join(','))}">
      ${mode === 'archive' ? `<div class="grid2"><div><label>zstd 压缩级别（1-19）</label>
          <input id="f-zstd" type="number" min="1" max="19" value="${esc(f.options.zstd_level || 3)}"></div></div>
        ${ageFields(f.options)}` : ''}
      <div class="warnbox">文件级备份：数据库运行中拷贝可能不一致，建议停库或用文件系统快照。一般优先用「连数据库导出」。</div>`;
    kindFields = `
      <div class="grid2">
        <div><label>备份方式</label>
          <select id="f-mode">
            <option value="dump" ${mode === 'dump' ? 'selected' : ''}>连数据库导出（推荐）</option>
            <option value="archive" ${mode === 'archive' ? 'selected' : ''}>打包数据目录</option>
            <option value="mirror" ${mode === 'mirror' ? 'selected' : ''}>按文件同步数据目录</option>
          </select></div>
        <div></div>
      </div>
      ${mode === 'dump' ? dumpFields : filesFields}`;
  }

  return `<div class="panel">
    <h2>${editing ? '编辑任务' : '新建任务'}</h2>
    ${S.error ? `<div class="errbox">${esc(S.error)}</div>` : ''}
    <div class="grid2">
      ${field('任务名称', 'f-name', f.name, '例如：媒体库同步')}
      <div><label>类型</label>
        <select id="f-kind" ${editing ? 'disabled' : ''}>
          <option value="mirror" ${f.kind === 'mirror' ? 'selected' : ''}>目录镜像（增量）</option>
          <option value="archive" ${f.kind === 'archive' ? 'selected' : ''}>目录归档（打包）</option>
          <option value="mysql" ${f.kind === 'mysql' ? 'selected' : ''}>MySQL 数据目录</option>
        </select></div>
      <div><label>目的地</label><select id="f-dest">${destOptions || '<option value="">（先配置目的地）</option>'}</select></div>
      ${field('远端目录', 'f-remote', f.remote_dir, '/115open/backup/media')}
      ${f.kind === 'archive' || (f.kind === 'mysql' && (f.options.mode || 'dump') !== 'mirror')
        ? field('保留份数（留空=不限）', 'f-keep', f.keep, '7', 'number')
        : ''}
    </div>
    ${renderSchedule(f.cron || '')}
    <div class="sep"></div>
    ${kindFields}
    <label class="check"><input type="checkbox" id="f-enabled" ${f.enabled ? 'checked' : ''}> 启用（定时任务只跑启用的）</label>
    <div style="margin-top:16px" class="row-actions">
      <button class="primary" data-act="save-job">保存</button>
      <button data-act="cancel-job">取消</button>
    </div>
    <p class="hint">定时时区跟随容器的 TZ；需要更复杂的规则时选"自定义 cron"直接写 5 段表达式。远端目录必须在目的地根路径之下。</p>
  </div>`;
}

function ageFields(options) {
  return `
    <label>age 公钥（留空=不加密）</label>
    <div class="inline">
      <input id="f-age" value="${esc(options.age_recipient || '')}" placeholder="age1...">
      <button class="small" data-act="keygen">生成密钥对</button>
    </div>
    ${S.ageKey ? `<div class="warnbox" style="margin-top:8px">
      <div>已生成密钥对。<b>私钥只显示这一次</b>，请立刻保存到安全位置（容器不保存私钥）：</div>
      <div class="copybox"><input id="age-identity" value="${esc(S.ageKey.identity)}" readonly>
        <button class="small" data-act="copy" data-target="age-identity">复制</button></div>
      <div class="hint">公钥已填入上面的输入框。</div>
    </div>` : ''}`;
}

function readJobForm() {
  const f = S.jobForm;
  const val = (id) => (document.getElementById(id) || {}).value || '';
  const checked = (id) => !!(document.getElementById(id) || {}).checked;
  const list = (id) => val(id).split(',').map((s) => s.trim()).filter(Boolean);
  const kind = val('f-kind') || f.kind;
  let options = {};
  if (kind === 'mirror') {
    options = {
      source: val('f-source'),
      include: list('f-include'),
      exclude: list('f-exclude'),
      sync_delete: checked('f-sync-del'),
      skip_adopt: false,
      delete_local: checked('f-del-local'),
      local_trash: val('f-trash').trim() || null,
    };
  } else if (kind === 'archive') {
    options = {
      source: val('f-source'),
      exclude: list('f-exclude'),
      zstd_level: Number(val('f-zstd') || 3),
      age_recipient: val('f-age').trim() || null,
    };
  } else {
    // 切换备份方式时，另一组的输入框可能不在页面上：用之前的值兜底
    const prev = f.options || {};
    const val2 = (id, key) => {
      const el = document.getElementById(id);
      return el ? el.value : (prev[key] == null ? '' : String(prev[key]));
    };
    const list2 = (id, key) => {
      const el = document.getElementById(id);
      if (el) return el.value.split(',').map((x) => x.trim()).filter(Boolean);
      return Array.isArray(prev[key]) ? prev[key] : [];
    };
    options = {
      mode: val2('f-mode', 'mode') || 'dump',
      host: val2('f-host', 'host'),
      port: Number(val2('f-port', 'port') || 3306),
      user: val2('f-user', 'user'),
      databases: list2('f-dbs', 'databases'),
      extra_args: (() => {
        const el = document.getElementById('f-extra');
        if (el) return el.value.split(/\s+/).filter(Boolean);
        return Array.isArray(prev.extra_args) ? prev.extra_args : [];
      })(),
      source: val2('f-source', 'source'),
      exclude: list2('f-exclude', 'exclude'),
      zstd_level: Number(val2('f-zstd', 'zstd_level') || 3),
      age_recipient: val2('f-age', 'age_recipient') || null,
    };
  }
  const body = {
    name: val('f-name'),
    enabled: checked('f-enabled'),
    kind,
    destination_id: Number(val('f-dest') || 0),
    remote_dir: val('f-remote'),
    cron: readSchedule() || null,
    keep: val('f-keep') ? Number(val('f-keep')) : null,
    options,
  };
  if (kind === 'mysql') {
    const pw = document.getElementById('f-dbpass');
    if (pw && pw.value) body.mysql_password = pw.value;
  }
  return body;
}

// ---------- 运行记录 ----------

function viewRuns() {
  const rows = S.runsCache || [];
  const jobOptions = ['<option value="">全部任务</option>']
    .concat(S.jobs.map((j) => `<option value="${j.id}" ${String(S.runFilter) === String(j.id) ? 'selected' : ''}>${esc(j.name)}</option>`))
    .join('');
  const body = rows.map((r) => `<tr>
      <td class="mono">#${r.id}</td>
      <td>${esc(r.job_name || ('任务 ' + r.job_id))}</td>
      <td>${statusBadge(r.status, r.status === 'running')}</td>
      <td>${esc(r.started_at_text)}</td>
      <td>${r.stats ? `${esc(r.stats.uploaded || 0)} 个 / ${fmtBytes(r.stats.uploaded_bytes)}` : '—'}</td>
      <td>${esc(r.trigger === 'manual' ? '手动' : '定时')}</td>
      <td>${esc(r.error || '')}</td>
      <td><button class="small" data-act="open-log" data-run="${r.id}">日志</button></td>
    </tr>`).join('');
  return `
    <div class="panel">
      <div class="inline" style="justify-content:space-between">
        <h2 style="margin:0">运行记录</h2>
        <div class="inline">
          <select id="run-filter" style="width:200px">${jobOptions}</select>
          <button class="small" data-act="reload-runs">刷新</button>
        </div>
      </div>
      ${rows.length ? `<table><thead><tr><th>#</th><th>任务</th><th>状态</th><th>开始</th><th>上传</th><th>触发</th><th>说明</th><th></th></tr></thead><tbody>${body}</tbody></table>`
        : '<p class="hint">还没有运行记录。</p>'}
    </div>
    ${S.log.runId ? viewLogPanel() : ''}`;
}

function viewLogPanel() {
  return `<div class="panel">
    <div class="inline" style="justify-content:space-between">
      <h2 style="margin:0">运行 #${S.log.runId} 日志</h2>
      <div class="row-actions">
        <button class="small" data-act="reload-log">刷新</button>
        <button class="small" data-act="close-log">关闭</button>
      </div>
    </div>
    <div class="log" id="log-box">${esc(S.log.text || '')}</div>
  </div>`;
}

async function openLog(runId) {
  S.log = { runId, offset: 0, timer: null, text: '' };
  S.view = 'runs';
  await loadRuns();
  await pollLog();
  render();
  clearInterval(S.log.timer);
  S.log.timer = setInterval(pollLog, 3000);
}

async function pollLog() {
  if (!S.log.runId) return;
  try {
    const res = await api(`/runs/${S.log.runId}/log?offset=${S.log.offset}`);
    S.log.offset = res.offset;
    if (res.text) S.log.text += res.text;
    const box = document.getElementById('log-box');
    if (box) {
      box.textContent = S.log.text;
      box.scrollTop = box.scrollHeight;
    }
  } catch (e) { /* 忽略轮询错误 */ }
}

async function loadRuns() {
  const q = S.runFilter ? `?job_id=${S.runFilter}&limit=100` : '?limit=100';
  const res = await api('/runs' + q);
  S.runsCache = res.items || [];
}

// ---------- 目的地 ----------

function viewDests() {
  const rows = S.dests.map((d) => `<tr>
      <td>${esc(d.name)}</td>
      <td class="mono">${esc(d.address)}</td>
      <td class="mono">${esc(d.root)}</td>
      <td>${esc(d.secret_masked)}</td>
      <td><div class="row-actions">
        <button class="small" data-act="probe" data-id="${d.id}">测试连接</button>
        <button class="small" data-act="selftest" data-id="${d.id}">自检</button>
        <button class="small" data-act="edit-dest" data-id="${d.id}">编辑</button>
        <button class="small danger" data-act="del-dest" data-id="${d.id}">删除</button>
      </div></td>
    </tr>`).join('');

  return `
    <div class="panel">
      <div class="inline" style="justify-content:space-between">
        <h2 style="margin:0">目的地</h2>
        <button class="primary" data-act="new-dest">新增目的地</button>
      </div>
      ${S.dests.length ? `<table><thead><tr><th>名称</th><th>CD2 地址</th><th>根路径</th><th>凭据</th><th></th></tr></thead><tbody>${rows}</tbody></table>`
        : '<p class="hint">还没有目的地。首版只支持 CD2，填好地址和 API Token 即可。</p>'}
      <p class="hint">说明：备份管理器只认识 CD2 的地址和令牌，115 的读写都由 CD2 完成。</p>
    </div>
    ${S.destForm ? viewDestForm() : ''}
    ${S.probe ? viewProbe() : ''}
    ${S.selftest ? viewSelftest() : ''}
    ${S.createdToken ? viewCreatedToken() : ''}`;
}

function viewDestForm() {
  const f = S.destForm;
  return `<div class="panel">
    <h2>${f.id ? '编辑目的地' : '新增目的地'}</h2>
    ${S.error ? `<div class="errbox">${esc(S.error)}</div>` : ''}
    <div class="grid2">
      ${field('名称', 'd-name', f.name, 'nas-cd2')}
      ${field('CD2 地址', 'd-address', f.address, 'http://192.168.8.114:19798')}
      ${field('根路径', 'd-root', f.root, '/115open')}
    </div>
    <div class="sep"></div>
    <label>API Token（推荐）</label>
    <input id="d-token" type="password" placeholder="${f.id && f.has_secret ? '已设置，留空不修改' : '在 CD2 网页 → 令牌管理里创建'}">
    <p class="hint">需要的权限：列目录、建文件夹、建文件、写入、改名、删除、查看传输任务（空间信息可选）；建议把令牌根目录限制到 <span class="mono">/115open</span>。</p>
    <div class="sep"></div>
    <h3>或者用 CD2 账号密码（可选）</h3>
    <div class="grid2">
      ${field('CD2 账号', 'd-user', f.user, '登录 CD2 网页的账号')}
      <div><label>CD2 密码</label><input id="d-pass" type="password" placeholder="仅在没填 Token 时使用"></div>
    </div>
    <div class="row-actions" style="margin-top:10px">
      <button class="small" data-act="make-token">用账号密码生成最小权限 Token</button>
    </div>
    <div style="margin-top:16px" class="row-actions">
      <button class="primary" data-act="save-dest">保存</button>
      <button data-act="probe-form">测试连接</button>
      <button data-act="cancel-dest">取消</button>
    </div>
  </div>`;
}

function readDestForm() {
  const val = (id) => (document.getElementById(id) || {}).value || '';
  const f = S.destForm;
  return {
    id: f.id,
    name: val('d-name'),
    address: val('d-address'),
    root: val('d-root'),
    token: val('d-token') || undefined,
    user: val('d-user') || undefined,
    password: val('d-pass') || undefined,
  };
}

function viewProbe() {
  const p = S.probe;
  if (p.error) return `<div class="panel"><h2>连接测试</h2><div class="errbox">${esc(p.error)}</div></div>`;
  const r = p.report || {};
  const missing = (p.missing_permissions || []);
  const perms = r.permissions ? `
    <div class="kv">
      <div>列目录</div><div>${r.permissions.list ? '✅' : '❌'}</div>
      <div>建文件夹</div><div>${r.permissions.create_folder ? '✅' : '❌'}</div>
      <div>建文件</div><div>${r.permissions.create_file ? '✅' : '❌'}</div>
      <div>改名</div><div>${r.permissions.rename ? '✅' : '❌'}</div>
      <div>删除</div><div>${r.permissions.delete ? '✅' : '❌'}</div>
      <div>查看传输任务</div><div>${r.permissions.transfer_tasks ? '✅' : '❌'}</div>
    </div>` : '<p class="hint">用 CD2 账号密码鉴权时无法查询令牌权限。</p>';
  return `<div class="panel">
    <h2>连接测试</h2>
    <div class="okbox">CD2 已连接：${esc(r.product || 'CloudDrive')} ${esc(r.version || '')}（CloudAPI ${esc(r.cloud_api_version || '?')}）</div>
    ${r.root_exists ? `<div class="okbox" style="margin-top:8px">根路径 ${esc(r.root_path)} 存在</div>`
      : `<div class="warnbox" style="margin-top:8px">根路径 ${esc(r.root_path)} 不存在或不是目录，先检查 CD2 里 115 的路径</div>`}
    ${missing.length ? `<div class="warnbox" style="margin-top:8px">缺少权限：${esc(missing.join('、'))}</div>` : ''}
    ${r.token_root ? `<p class="hint">令牌被限制在：<span class="mono">${esc(r.token_root)}</span></p>` : ''}
    ${r.token_info_error ? `<p class="hint">查不到令牌权限清单（不影响备份）：${esc(r.token_info_error)}</p>` : ''}
    ${r.space ? `<p class="hint">115 空间：已用 ${fmtBytes(r.space.used)} / 共 ${fmtBytes(r.space.total)}，剩余 ${fmtBytes(r.space.free)}</p>` : ''}
    <div class="sep"></div>
    ${perms}
  </div>`;
}

function viewSelftest() {
  const r = S.selftest;
  if (r.error) return `<div class="panel"><h2>自检</h2><div class="errbox">${esc(r.error)}</div></div>`;
  const rows = (r.steps || []).map((st) => `<div class="kv" style="margin:6px 0">
      <div>${st.ok ? '✅' : '❌'} ${esc(st.name)}</div>
      <div class="mono">${esc(st.detail)}</div>
    </div>`).join('');
  return `<div class="panel">
    <div class="inline" style="justify-content:space-between">
      <h2 style="margin:0">自检</h2>
      <button class="small" data-act="close-selftest">关闭</button>
    </div>
    ${r.ok ? '<div class="okbox">全部通过：CD2 API → CD2 → 115 整条链路正常</div>'
           : '<div class="errbox">有步骤没通过，见下面明细</div>'}
    ${rows}
    <p class="hint">自检会在根路径下建一个 _selftest 目录，传一个 64 KiB 随机文件，读回比对后删掉。</p>
  </div>`;
}

function viewCreatedToken() {
  return `<div class="panel">
    <h2>已生成令牌</h2>
    <div class="warnbox">令牌只显示这一次，请立刻复制并填入上面的 Token 输入框。</div>
    <div class="copybox"><input id="new-token" class="mono" value="${esc(S.createdToken)}" readonly>
      <button class="small" data-act="copy" data-target="new-token">复制</button></div>
    <div style="margin-top:10px"><button class="small" data-act="use-new-token">填入表单</button></div>
  </div>`;
}

// ---------- 设置 ----------

function viewSettings() {
  const p = S.policy || {};
  return `
    <div class="panel">
      <h2>限速与风控</h2>
      <p class="hint">115 对列目录和上传有配额（文档记录的 770004）。这些值调小风险更高，默认值是按现有项目经验定的。</p>
      <div class="grid2">
        ${field('文件之间最小间隔（毫秒）', 'p-file-gap', p.file_gap_ms, '300', 'number')}
        ${field('列目录最小间隔（毫秒）', 'p-list-gap', p.list_gap_ms, '1000', 'number')}
        ${field('删除最小间隔（毫秒）', 'p-del-gap', p.delete_gap_ms, '1000', 'number')}
        ${field('任意 CD2 调用最小间隔（毫秒）', 'p-rpc-gap', p.rpc_gap_ms, '200', 'number')}
        ${field('上传状态轮询（毫秒）', 'p-poll', p.poll_ms, '10000', 'number')}
        ${field('连续失败几次中止', 'p-maxfail', p.max_failures, '3', 'number')}
        ${field('风控冷却（分钟）', 'p-cooldown', p.cooldown_minutes, '60', 'number')}
        ${field('停机补跑窗口（秒）', 'p-misfire', p.misfire_window_s, '86400', 'number')}
      </div>
      <label class="check"><input type="checkbox" id="p-imm" ${p.upload_immediately ? 'checked' : ''}>
        上传时要求 CD2 立刻上传（不排队等待）</label>
      <div style="margin-top:14px"><button class="primary" data-act="save-policy">保存</button></div>
    </div>

    <div class="panel">
      <h2>修改管理员密码</h2>
      <div class="grid2">
        <div><label>原密码</label><input id="pw-old" type="password"></div>
        <div><label>新密码（至少 8 位）</label><input id="pw-new" type="password"></div>
      </div>
      <div style="margin-top:14px"><button data-act="save-password">修改密码</button></div>
    </div>

    <div class="panel">
      <h2>关于</h2>
      <div class="kv">
        <div>版本</div><div>${esc(S.overview ? S.overview.version : '')}</div>
        <div>数据目录</div><div class="mono">${esc(S.overview ? S.overview.data_dir : '')}</div>
        <div>对 115 的访问</div><div>一律经 CD2 的 gRPC 接口，由 CD2 去读写 115</div>
      </div>
    </div>`;
}

// ---------- 动作 ----------

const actions = {
  async tab(elm) {
    S.view = elm.dataset.view;
    S.error = null;
    if (S.view === 'runs') await loadRuns();
    if (S.view !== 'runs' && S.log.timer) {
      clearInterval(S.log.timer);
      S.log = { runId: null, offset: 0, timer: null, text: '' };
    }
    render();
    window.scrollTo({ top: 0, behavior: 'smooth' });
  },

  async refresh() {
    try {
      await loadAll();
      if (S.view === 'runs') await loadRuns();
      render();
      toast('数据已刷新');
    } catch (e) { toast(e.message, true); }
  },

  async logout() {
    await fetch('/api/logout', { method: 'POST' });
    S.csrf = null;
    S.error = null;
    renderAuth();
  },

  async 'run-job'(elm) {
    try {
      await api(`/jobs/${elm.dataset.id}/run`, 'POST');
      toast('已加入队列');
      await loadAll();
      render();
    } catch (e) { toast(e.message, true); }
  },

  async 'stop-job'(elm) {
    try {
      await api(`/jobs/${elm.dataset.id}/stop`, 'POST');
      toast('已请求停止');
      await loadAll();
      render();
    } catch (e) { toast(e.message, true); }
  },

  async 'dry-run'(elm) {
    try {
      const res = await api(`/jobs/${elm.dataset.id}/dry-run`, 'POST');
      const sample = (res.sample || []).map((r) => `<div class="mono">${esc(r)}</div>`).join('');
      S.probe = null;
      render();
      const panel = document.createElement('div');
      panel.className = 'panel';
      panel.innerHTML = `<h2>预览结果</h2>
        <div class="kv">
          <div>扫描文件</div><div>${res.scanned}</div>
          <div>需要上传</div><div>${res.candidates}</div>
          <div>未变化</div><div>${res.unchanged}</div>
          <div>按规则排除</div><div>${res.excluded}</div>
          <div>跳过符号链接</div><div>${res.symlinks}</div>
          <div>首次运行</div><div>${res.first_run ? '是（运行时会先读一次云端目录做认领）' : '否'}</div>
        </div>
        ${sample ? `<h3>样例</h3>${sample}` : ''}`;
      document.querySelector('.wrap').appendChild(panel);
    } catch (e) { toast(e.message, true); }
  },

  'new-job'() {
    S.jobForm = {
      id: null, name: '', kind: 'mirror', enabled: true,
      destination_id: S.dests.length ? S.dests[0].id : '',
      remote_dir: S.dests.length ? S.dests[0].root + '/backup' : '',
      cron: '', keep: null, has_secret: false,
      options: { source: '', include: [], exclude: ['@eaDir', '.DS_Store', 'Thumbs.db', '*.tmp'], sync_delete: false, delete_local: false, local_trash: null },
    };
    S.error = null;
    S.ageKey = null;
    render();
    window.scrollTo(0, document.body.scrollHeight);
  },

  'edit-job'(elm) {
    const job = S.jobs.find((j) => String(j.id) === elm.dataset.id);
    if (!job) return;
    S.jobForm = {
      id: job.id, name: job.name, kind: job.kind, enabled: job.enabled,
      destination_id: job.destination_id, remote_dir: job.remote_dir,
      cron: job.cron || '', keep: job.keep, has_secret: job.has_secret,
      options: Object.assign({}, job.options),
    };
    S.error = null;
    S.ageKey = null;
    render();
  },

  'cancel-job'() { S.jobForm = null; S.error = null; S.ageKey = null; render(); },

  async 'save-job'() {
    try {
      const body = readJobForm();
      if (S.jobForm.id) await api(`/jobs/${S.jobForm.id}`, 'PUT', body);
      else await api('/jobs', 'POST', body);
      S.jobForm = null;
      S.error = null;
      await loadAll();
      render();
      toast('已保存');
    } catch (e) { S.error = e.message; render(); }
  },

  async 'del-job'(elm) {
    if (!confirm('删除任务？索引和运行记录会一起删掉（115 上的文件不动）')) return;
    try {
      await api(`/jobs/${elm.dataset.id}`, 'DELETE');
      await loadAll();
      render();
      toast('已删除');
    } catch (e) { toast(e.message, true); }
  },

  'pick-local'() {
    S.localPick = { path: '/host', dirs: [] };
    loadLocalPicker('/host');
  },

  async 'new-dest'() {
    S.destForm = { id: null, name: '', address: 'http://', root: '/115open', has_secret: false };
    S.probe = null;
    S.createdToken = null;
    S.error = null;
    render();
  },

  'edit-dest'(elm) {
    const d = S.dests.find((x) => String(x.id) === elm.dataset.id);
    if (!d) return;
    S.destForm = { id: d.id, name: d.name, address: d.address, root: d.root, has_secret: d.has_secret };
    S.probe = null;
    S.createdToken = null;
    S.error = null;
    render();
  },

  'cancel-dest'() { S.destForm = null; S.probe = null; S.createdToken = null; S.error = null; render(); },

  async 'save-dest'() {
    try {
      const body = readDestForm();
      if (S.destForm.id) await api(`/destinations/${S.destForm.id}`, 'PUT', body);
      else await api('/destinations', 'POST', body);
      S.destForm = null;
      S.error = null;
      await loadAll();
      render();
      toast('已保存');
    } catch (e) { S.error = e.message; render(); }
  },

  async 'del-dest'(elm) {
    if (!confirm('删除这个目的地？')) return;
    try {
      await api(`/destinations/${elm.dataset.id}`, 'DELETE');
      await loadAll();
      render();
      toast('已删除');
    } catch (e) { toast(e.message, true); }
  },

  async probe(elm) {
    await runProbe({ id: Number(elm.dataset.id) });
  },

  async 'probe-form'() {
    await runProbe(readDestForm());
  },

  'make-token'() {
    const f = readDestForm();
    if (!f.user || !f.password) { toast('先填 CD2 账号和密码', true); return; }
    makeToken({ address: f.address, root: f.root, user: f.user, password: f.password });
  },

  async 'save-policy'() {
    const num = (id) => Number((document.getElementById(id) || {}).value || 0);
    const body = {
      file_gap_ms: num('p-file-gap'), list_gap_ms: num('p-list-gap'), delete_gap_ms: num('p-del-gap'),
      rpc_gap_ms: num('p-rpc-gap'), poll_ms: num('p-poll'), max_failures: num('p-maxfail'),
      cooldown_minutes: num('p-cooldown'), misfire_window_s: num('p-misfire'),
      upload_immediately: !!(document.getElementById('p-imm') || {}).checked,
    };
    try {
      await api('/policy', 'PUT', body);
      S.policy = await api('/policy');
      toast('已保存');
      render();
    } catch (e) { toast(e.message, true); }
  },

  async 'save-password'() {
    const oldPw = (document.getElementById('pw-old') || {}).value || '';
    const newPw = (document.getElementById('pw-new') || {}).value || '';
    try {
      await api('/password', 'POST', { old: oldPw, new: newPw });
      toast('密码已修改');
      render();
    } catch (e) { toast(e.message, true); }
  },

  async keygen() {
    try {
      // 先把当前表单内容收进状态，重渲染时才不会丢
      if (S.jobForm) {
        const current = readJobForm();
        S.jobForm.options = current.options;
        S.jobForm.keep = current.keep;
        S.jobForm.name = current.name;
        S.jobForm.remote_dir = current.remote_dir;
        S.jobForm.cron = current.cron || '';
        S.jobForm.enabled = current.enabled;
        S.jobForm.destination_id = current.destination_id;
      }
      S.ageKey = await api('/age/keygen', 'POST');
      if (S.jobForm && S.ageKey.recipient) S.jobForm.options.age_recipient = S.ageKey.recipient;
      render();
    } catch (e) { toast(e.message, true); }
  },

  'open-log'(elm) { openLog(Number(elm.dataset.run)); },
  'close-log'() {
    clearInterval(S.log.timer);
    S.log = { runId: null, offset: 0, timer: null, text: '' };
    render();
  },
  'reload-log'() { pollLog(); },
  async 'reload-runs'() { await loadRuns(); render(); },
  async 'filter-runs'() { S.runFilter = (document.getElementById('run-filter') || {}).value || ''; await loadRuns(); render(); },

  'use-new-token'() {
    const input = document.getElementById('d-token');
    if (input && S.createdToken) input.value = S.createdToken;
    document.querySelectorAll('[data-act="new-dest"],[data-act="edit-dest"]').forEach(() => {});
    toast('已填入，别忘了保存');
  },

  copy(elm) {
    const input = document.getElementById(elm.dataset.target);
    if (!input) return;
    input.select();
    navigator.clipboard.writeText(input.value).then(() => toast('已复制'), () => toast('复制失败，请手动选择', true));
  },

  async setup() { await doSetup(); },
  async login() { await doLogin(); },
};

async function runProbe(input) {
  if (S.destForm) S.destForm = Object.assign({}, S.destForm, input);
  try {
    const res = await api('/destinations/probe', 'POST', input);
    S.probe = { report: res.report, missing_permissions: res.missing_permissions };
    S.view = 'dests';
    render();
  } catch (e) {
    S.probe = { error: e.message };
    S.view = 'dests';
    render();
  }
}

async function makeToken(input) {
  if (S.destForm) S.destForm = Object.assign({}, S.destForm, input);
  try {
    const res = await api('/token/create', 'POST', input);
    S.createdToken = res.token;
    render();
    toast('令牌已生成');
  } catch (e) { toast(e.message, true); }
}

async function loadLocalPicker(path) {
  try {
    const res = await api('/browse?path=' + encodeURIComponent(path));
    S.localPick = { path: res.path, parent: res.parent, dirs: res.dirs || [] };
    render();
  } catch (e) { toast(e.message, true); }
}

function viewLocalPicker() {
  const p = S.localPick;
  const dirs = (p.dirs || []).map((d) =>
    `<div style="margin:4px 0"><a href="#" data-act="local-enter" data-path="${esc(d.path)}"><svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" style="vertical-align:-2px;margin-right:4px"><path d="M3 7a2 2 0 0 1 2-2h5l2 2h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z"/></svg>${esc(d.name)}</a></div>`
  ).join('') || '<p class="hint">没有子目录</p>';
  return `<div class="panel">
    <div class="inline" style="justify-content:space-between">
      <h2 style="margin:0">选择源目录</h2>
      <button class="small" data-act="local-close">关闭</button>
    </div>
    <p class="hint mono">${esc(p.path)}</p>
    <div class="row-actions" style="margin-bottom:8px">
      ${p.parent ? `<button class="small" data-act="local-enter" data-path="${esc(p.parent)}">上一级</button>` : ''}
      <button class="small primary" data-act="local-use" data-path="${esc(p.path)}">用这个目录</button>
    </div>
    ${dirs}
  </div>`;
}

// 事件委托
document.addEventListener('click', async (ev) => {
  const elm = ev.target.closest('[data-act]');
  if (!elm) return;
  const act = elm.dataset.act;
  if (act === 'local-enter') {
    ev.preventDefault();
    await loadLocalPicker(elm.dataset.path);
    return;
  }
  if (act === 'local-use') {
    ev.preventDefault();
    if (S.jobForm) S.jobForm.options.source = elm.dataset.path;
    const input = document.getElementById('f-source');
    if (input) input.value = elm.dataset.path;
    S.localPick = null;
    render();
    return;
  }
  if (act === 'local-close') {
    S.localPick = null;
    render();
    return;
  }
  // 类型切换时重渲染表单
  if (act === 'noop') return;
  const fn = actions[act];
  if (fn) {
    ev.preventDefault();
    await fn(elm);
  }
});

document.addEventListener('change', async (ev) => {
  // 切换"什么时候运行"：保留其它字段，按旧时间给新方式一个合理默认值
  if (ev.target && ev.target.id === 's-kind' && S.jobForm) {
    const body = readJobForm();
    const prev = parseCronToState(body.cron);
    const hour = prev.hour ?? 3;
    const minute = prev.minute ?? 0;
    const kind = ev.target.value;
    let seed = '';
    if (kind === 'hourly') seed = `${minute} * * * *`;
    else if (kind === 'daily') seed = `${minute} ${hour} * * *`;
    else if (kind === 'weekly') seed = `${minute} ${hour} * * ${(prev.days || [1]).join(',')}`;
    else if (kind === 'monthly') seed = `${minute} ${hour} ${prev.dom || 1} * *`;
    else if (kind === 'custom') seed = body.cron && body.cron.trim() ? body.cron.trim() : '0 4 * * *';
    S.jobForm = Object.assign({}, S.jobForm, {
      name: body.name, enabled: body.enabled, destination_id: body.destination_id,
      remote_dir: body.remote_dir, keep: body.keep, options: body.options, cron: seed,
    });
    render();
    return;
  }
  if (ev.target && ev.target.id === 'f-mode' && S.jobForm) {
    const current = readJobForm();
    S.jobForm.options = current.options;
    S.jobForm.name = current.name;
    S.jobForm.destination_id = current.destination_id;
    S.jobForm.remote_dir = current.remote_dir;
    S.jobForm.cron = current.cron || '';
    S.jobForm.keep = current.keep;
    S.jobForm.enabled = current.enabled;
    render();
    return;
  }
  if (ev.target && ev.target.id === 'f-kind' && S.jobForm) {
    S.jobForm.kind = ev.target.value;
    // 切换类型时保留基本信息
    const keep = {
      id: S.jobForm.id, name: (document.getElementById('f-name') || {}).value || '',
      destination_id: (document.getElementById('f-dest') || {}).value || S.jobForm.destination_id,
      remote_dir: (document.getElementById('f-remote') || {}).value || S.jobForm.remote_dir,
      cron: readSchedule(),
      keep: (document.getElementById('f-keep') || {}).value || null,
      enabled: !!(document.getElementById('f-enabled') || {}).checked,
      has_secret: S.jobForm.has_secret,
    };
    S.jobForm = Object.assign({}, S.jobForm, keep, { kind: ev.target.value, options: ev.target.value === 'mysql'
      ? { mode: 'dump', host: '', port: 3306, user: '', databases: [], extra_args: [], source: '', exclude: [], zstd_level: 3, age_recipient: null }
      : { source: S.jobForm.options.source || '', include: [], exclude: ['@eaDir', '*.tmp'], sync_delete: false, delete_local: false, local_trash: null, zstd_level: 3, age_recipient: null } });
    render();
  }
  if (ev.target && ev.target.dataset && ev.target.dataset.dow) {
    updateSchedulePreview();
  }
  if (ev.target && ev.target.id === 'run-filter') {
    S.runFilter = ev.target.value;
    await loadRuns();
    render();
  }
});

document.addEventListener('input', (ev) => {
  if (ev.target && ['s-time', 's-min', 's-dom', 's-cron'].includes(ev.target.id)) {
    updateSchedulePreview();
  }
});

document.addEventListener('keydown', (ev) => {
  if (ev.key === 'Enter') {
    const id = ev.target && ev.target.id;
    if (id === 'login-password') { ev.preventDefault(); doLogin(); }
    if (id === 'setup-password') { ev.preventDefault(); doSetup(); }
  }
});

boot();

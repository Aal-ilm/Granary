// Granary web UI. Plain JavaScript, no build step, no third-party code.
'use strict';

// ------------------------------------------------------------------ helpers

const $ = (sel, el = document) => el.querySelector(sel);
const $$ = (sel, el = document) => Array.from(el.querySelectorAll(sel));

const esc = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

/** Only allow http(s) links (and inline PNGs for the 2FA QR code) in
 *  href/src attributes built from API data. */
const safeUrl = (u) => (/^(https?:\/\/|data:image\/png;base64,)/i.test(String(u ?? '')) ? String(u) : '#');

const REPOS = {
  cargo: { label: 'Cargo', upstream: 'crates.io' },
  npm: { label: 'npm', upstream: 'registry.npmjs.org' },
  pypi: { label: 'PyPI', upstream: 'pypi.org' },
  maven: { label: 'Maven', upstream: 'Maven Central' },
};
const REPO_IDS = Object.keys(REPOS);

/** Maven packages are named `group:artifact`. */
const mavenName = (name) => {
  const i = String(name).indexOf(':');
  return i < 0 ? { group: '', artifact: name } : { group: name.slice(0, i), artifact: name.slice(i + 1) };
};

/** A repository's client setup as a list of snippets: the main one, then
 *  any alternatives (Gradle next to Maven). */
const setupSnippets = (setup) => [{ label: '', file: setup.file, snippet: setup.snippet, after: setup.after }, ...(setup.more || [])];

/** Shown on the Overview and Repositories pages while GRANARY_OFFLINE is on. */
const offlineBanner = (health) =>
  (health || []).some((h) => h.offline)
    ? '<div class="banner"><span class="dot warn"></span><span>Offline mode is on. Granary serves only what is cached and never contacts the public registries. Turn off <code>GRANARY_OFFLINE</code> to fetch new packages.</span></div>'
    : '';

/** Display names for everything with a gauge: the package formats and Git. */
const FORMAT_LABELS = { ...Object.fromEntries(REPO_IDS.map((id) => [id, REPOS[id].label])), git: 'Git' };

const chip = (repo) => `<span class="fmt ${esc(repo)}">${esc(FORMAT_LABELS[repo] ?? repo)}</span>`;

function bytes(n) {
  if (n == null) return '-';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

const count = (n) => Number(n || 0).toLocaleString();
const plural = (n, one, many = one + 's') => `${count(n)} ${n === 1 ? one : many}`;

function ago(ts) {
  if (!ts) return 'never';
  const s = Math.round(Date.now() / 1000 - ts);
  if (s < -5) return 'in ' + duration(-s);
  if (s < 45) return 'just now';
  return duration(s) + ' ago';
}

function duration(s) {
  if (s < 90) return `${Math.max(1, Math.round(s))} sec`;
  if (s < 5400) return `${Math.round(s / 60)} min`;
  if (s < 129600) return `${Math.round(s / 3600)} hr`;
  const d = Math.round(s / 86400);
  return d === 1 ? '1 day' : `${d} days`;
}

const fullTime = (ts) => (ts ? new Date(ts * 1000).toLocaleString() : '');
const timeTag = (ts) => (ts ? `<time datetime="${new Date(ts * 1000).toISOString()}" title="${esc(fullTime(ts))}">${esc(ago(ts))}</time>` : '<span class="muted">never</span>');

function debounce(fn, ms) {
  let t;
  return (...args) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
}

const icons = {
  chevron: '<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M3 1.5 6.5 5 3 8.5" fill="none" stroke="currentColor" stroke-width="1.6"/></svg>',
};

// ---------------------------------------------------------- session & api

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

/** The signed-in user and server settings, from /api/auth/me. */
let me = null;
const isAdmin = () => me?.user?.role === 'admin';

async function api(path, { method = 'GET', body } = {}) {
  const headers = { Accept: 'application/json' };
  if (method !== 'GET') headers['X-Granary-Request'] = '1';
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  let res;
  try {
    res = await fetch(path, {
      method,
      headers,
      credentials: 'same-origin',
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError(0, 'Granary is not responding. Check that the server is running.');
  }
  if (!res.ok) {
    const text = (await res.text()).trim();
    const message = text ? text[0].toUpperCase() + text.slice(1) + '.' : `Request failed (HTTP ${res.status}).`;
    if (res.status === 401 && !path.startsWith('/api/auth/') && me) showSignIn('Your session ended. Sign in again.');
    if (res.status === 403 && /change your password/i.test(text)) showPasswordChange();
    throw new ApiError(res.status, message);
  }
  return res.status === 204 ? null : res.json();
}

/** Kept as a seam for actions that change data. */
const act = (fn) => fn();

// ----------------------------------------------------------- sign-in screens

function stopPage() {
  cleanups.forEach((fn) => fn());
  cleanups = [];
  navToken++;
}

function hideShell() {
  stopPage();
  $('.shell').classList.add('hidden');
  $('#auth').classList.remove('hidden');
}

function showShell() {
  $('#auth').classList.add('hidden');
  $('#auth').innerHTML = '';
  $('.shell').classList.remove('hidden');
  document.body.dataset.role = me.user.role;
  $('#userbox').innerHTML = `
    <div><strong>${esc(me.user.username)}</strong> <span class="muted">${me.user.role === 'admin' ? 'Admin' : 'Viewer'}</span></div>
    <div class="userlinks"><a href="#/account">Account</a><button class="link" data-signout>Sign out</button></div>`;
}

const authCard = (inner) => `
  <div class="auth-card panel">
    <div class="auth-brand"><img src="/ui/granary-logo.svg" alt="" width="136" height="136"><span>Granary</span></div>
    ${inner}
  </div>`;

function showError(el, message) {
  el.textContent = message;
  el.classList.toggle('hidden', !message);
}

function showSignIn(notice = '') {
  me = null;
  hideShell();
  $('#auth').innerHTML = authCard(`
    <h1>Sign in</h1>
    ${notice ? `<p class="auth-note">${esc(notice)}</p>` : ''}
    <form id="signin" class="stack" novalidate>
      <div class="field"><label for="si-user">Username</label>
        <input id="si-user" type="text" autocomplete="username" autocapitalize="off" spellcheck="false"></div>
      <div class="field"><label for="si-pass">Password</label>
        <input id="si-pass" type="password" autocomplete="current-password"></div>
      <div class="field hidden" id="si-code-row"><label for="si-code">Two-factor code</label>
        <input id="si-code" type="text" inputmode="numeric" autocomplete="one-time-code" maxlength="9">
        <p class="help">Enter the 6-digit code from your authenticator app.</p></div>
      <p class="err hidden" id="si-err" role="alert"></p>
      <button class="primary wide" type="submit">Sign in</button>
    </form>`);
  const form = $('#signin');
  const err = $('#si-err');
  $('#si-user').focus();
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const username = $('#si-user').value.trim();
    const password = $('#si-pass').value;
    const codeRow = $('#si-code-row');
    const code = codeRow.classList.contains('hidden') ? undefined : $('#si-code').value.trim();
    if (!username || !password) return showError(err, 'Enter your username and password.');
    if (code !== undefined && !code) return showError(err, 'Enter the code from your authenticator app.');
    const button = form.querySelector('button[type="submit"]');
    button.disabled = true;
    try {
      const r = await api('/api/auth/login', { method: 'POST', body: { username, password, code } });
      if (r.status === 'totp_required') {
        showError(err, '');
        codeRow.classList.remove('hidden');
        $('#si-code').focus();
        return;
      }
      me = await api('/api/auth/me');
      enter();
    } catch (ex) {
      showError(err, ex.message);
      if (code !== undefined) $('#si-code').select();
    } finally {
      button.disabled = false;
    }
  });
}

const passwordFields = (prefix, currentLabel) => `
  <div class="field"><label for="${prefix}-current">${esc(currentLabel)}</label>
    <input id="${prefix}-current" type="password" autocomplete="current-password"></div>
  <div class="field"><label for="${prefix}-new">New password</label>
    <input id="${prefix}-new" type="password" autocomplete="new-password">
    <p class="help">At least ${esc(me?.min_password_length ?? 12)} characters. A few unrelated words make a strong, memorable password.</p></div>
  <div class="field"><label for="${prefix}-confirm">Repeat new password</label>
    <input id="${prefix}-confirm" type="password" autocomplete="new-password"></div>`;

function bindPasswordForm(form, prefix, onDone) {
  const err = $(`#${prefix}-err`, form);
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const current = $(`#${prefix}-current`, form).value;
    const next = $(`#${prefix}-new`, form).value;
    if (!current) return showError(err, 'Enter your current password.');
    if (next.length < (me?.min_password_length ?? 12)) return showError(err, `Use at least ${me?.min_password_length ?? 12} characters.`);
    if (next !== $(`#${prefix}-confirm`, form).value) return showError(err, 'The new passwords don’t match.');
    try {
      const r = await api('/api/account/password', { method: 'POST', body: { current, new: next } });
      me.user = r.user;
      form.reset();
      showError(err, '');
      onDone();
    } catch (ex) {
      showError(err, ex.message);
    }
  });
}

function showPasswordChange() {
  hideShell();
  $('#auth').innerHTML = authCard(`
    <h1>Choose a new password</h1>
    <p class="auth-note">You signed in with a temporary password. Choose your own to continue.</p>
    <form id="pw-form" class="stack" novalidate>
      ${passwordFields('fp', 'Temporary password')}
      <p class="err hidden" id="fp-err" role="alert"></p>
      <button class="primary wide" type="submit">Save password</button>
    </form>
    <button class="link signout-link" data-signout>Sign out</button>`);
  $('#fp-current').focus();
  bindPasswordForm($('#pw-form'), 'fp', () => {
    toast('Password saved');
    enter();
  });
}

function enter() {
  if (me.user.must_change_password) return showPasswordChange();
  showShell();
  route();
}

async function signOut() {
  try {
    await api('/api/auth/logout', { method: 'POST' });
  } catch {
    /* signed out either way */
  }
  showSignIn('You signed out.');
}

/** Show a one-time secret (password or token) with a copy button. */
const reveal = (value) => `
  <div class="reveal"><code>${esc(value)}</code><button class="small" type="button" data-copy="${esc(value)}">Copy</button></div>`;

function confirmAction({ title, body, ok }) {
  const dlg = $('#confirm-dialog');
  $('#confirm-title').textContent = title;
  $('#confirm-body').textContent = body;
  $('#confirm-ok').textContent = ok;
  dlg.returnValue = '';
  dlg.showModal();
  $('#confirm-ok').focus();
  return new Promise((resolve) => {
    dlg.addEventListener('close', () => resolve(dlg.returnValue === 'ok'), { once: true });
  });
}

function toast(message, kind = '') {
  const el = document.createElement('div');
  el.className = 'toast ' + kind;
  el.textContent = message;
  $('#toasts').append(el);
  setTimeout(() => el.remove(), kind === 'error' ? 7000 : 3500);
}

async function copyText(text, button) {
  let ok = false;
  try {
    await navigator.clipboard.writeText(text);
    ok = true;
  } catch {
    // Clipboard API needs HTTPS; fall back for plain-HTTP LAN setups.
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.className = 'sr-only';
    document.body.append(ta);
    ta.select();
    try {
      ok = document.execCommand('copy');
    } catch {
      ok = false;
    }
    ta.remove();
  }
  if (button) {
    const prev = button.textContent;
    button.textContent = ok ? 'Copied' : 'Copy failed';
    setTimeout(() => (button.textContent = prev), 1500);
  }
}

// ------------------------------------------------------------------ router

const view = () => $('#view');
let navToken = 0;
let cleanups = [];

function onLeave(fn) {
  cleanups.push(fn);
}

function parseHash() {
  const raw = location.hash.replace(/^#/, '') || '/';
  const [path, qs = ''] = raw.split('?');
  return { path: path || '/', query: new URLSearchParams(qs) };
}

function go(path, query, { replace = false } = {}) {
  const qs = query && [...query].length ? '?' + query.toString() : '';
  const target = '#' + path + qs;
  if (replace) history.replaceState(null, '', target);
  else location.hash = target;
  if (replace) route();
}

const routes = [
  [/^\/$/, 'overview', renderOverview],
  [/^\/repos$/, 'repos', renderRepos],
  [/^\/browse\/git$/, 'browse', renderGitRepos],
  [/^\/browse(?:\/(cargo|npm|pypi|maven))?$/, 'browse', renderBrowse],
  [/^\/c\/(cargo|npm|pypi|maven)\/(.+)$/, 'browse', renderComponent],
  [/^\/get(?:\/(npm|cargo|pypi|maven))?$/, 'get', renderGet, { admin: true }],
  [/^\/templates$/, 'templates', renderTemplates],
  [/^\/templates\/new$/, 'templates', renderTemplateEditor, { admin: true }],
  [/^\/templates\/(\d+)$/, 'templates', renderTemplate],
  [/^\/templates\/(\d+)\/edit$/, 'templates', renderTemplateEditor, { admin: true }],
  [/^\/jobs$/, 'jobs', renderJobs],
  [/^\/jobs\/(\d+)$/, 'jobs', renderJob],
  [/^\/cleanup$/, 'cleanup', renderCleanup],
  [/^\/tasks$/, 'tasks', renderTasks],
  [/^\/system$/, 'system', renderSystem],
  [/^\/logs$/, 'logs', renderLogs, { admin: true }],
  [/^\/audit$/, 'audit', renderAudit, { admin: true }],
  [/^\/account$/, 'account', renderAccount],
  [/^\/tokens$/, 'tokens', renderTokens],
  [/^\/users$/, 'users', renderUsers, { admin: true }],
];

const decodeParam = (p) => {
  try {
    return p ? decodeURIComponent(p) : p;
  } catch {
    return null;
  }
};

function renderAdminOnly() {
  setView(`<div class="page-head"><div><h1>Admins only</h1><p>This page is only available to admin accounts. Ask an admin if you need access.</p></div></div>
    <a class="btn" href="#/">Go to overview</a>`);
}

async function route() {
  if (!me) return;
  cleanups.forEach((fn) => fn());
  cleanups = [];
  const token = ++navToken;
  const { path, query } = parseHash();
  const match = routes.find(([re]) => re.test(path));
  let [re, nav, render, opts] = match || [null, '', renderNotFound];
  const params = re ? path.match(re).slice(1).map(decodeParam) : [];
  if (params.includes(null)) render = renderNotFound;
  else if (opts?.admin && !isAdmin()) render = renderAdminOnly;
  $$('.nav a').forEach((a) => (a.dataset.nav === nav ? a.setAttribute('aria-current', 'page') : a.removeAttribute('aria-current')));
  const search = $('#global-search');
  if (document.activeElement !== search) search.value = path.startsWith('/browse') ? query.get('q') || '' : '';
  const isCurrent = () => token === navToken;
  try {
    await render({ params, query, isCurrent });
  } catch (e) {
    if (!isCurrent()) return;
    setView(`
      <div class="page-head"><div><h1>Something went wrong</h1><p>${esc(e.message)}</p></div></div>
      <button class="btn" data-retry>Try again</button>`);
    $('[data-retry]', view()).addEventListener('click', route);
  }
}

/** Replace the page. The container is swapped for a fresh node so event
 *  listeners from the previous page never linger. */
function setView(html) {
  const old = view();
  const fresh = old.cloneNode(false);
  fresh.innerHTML = html;
  old.replaceWith(fresh);
}

function renderNotFound() {
  setView(`<div class="page-head"><div><h1>Page not found</h1><p>There is nothing at this address.</p></div></div>
    <a class="btn" href="#/">Go to overview</a>`);
}

// ---------------------------------------------------------------- overview

async function renderOverview({ isCurrent }) {
  const d = await api('/api/overview');
  if (!isCurrent()) return;
  const total = d.repos.reduce((s, r) => s + r.size, 0);
  const packages = d.repos.filter((r) => r.repo !== 'git').reduce((s, r) => s + r.packages, 0);
  const gitRepos = d.repos.find((r) => r.repo === 'git')?.packages || 0;
  const hits = d.stats.file_hits;
  const served = hits + d.stats.file_downloads;
  const hitRate = served ? Math.round((hits / served) * 100) : null;
  const disk = d.disk;
  const used = disk ? disk.total - disk.free : 0;
  const usedPct = disk && disk.total ? (used / disk.total) * 100 : 0;
  const health = Object.fromEntries((d.health || []).map((h) => [h.repo, h]));

  // Gauges fill against the cache budget, or the whole disk without one.
  const limit = d.cache_limit || null;
  const capacity = limit || (disk ? disk.total : 0);
  const against = limit ? 'cache budget' : 'disk';
  const budgetPct = limit ? (d.cache_used / limit) * 100 : 0;
  const silos = d.repos
    .map((r) => {
      const share = total ? (r.size / total) * 100 : 0;
      const level = capacity ? (r.size / capacity) * 100 : share;
      // Anything cached shows a visible level, clear of the empty mark.
      const fill = r.size ? Math.min(100, Math.max(4, level)) : 0;
      const levelText = capacity ? `, ${level < 1 && r.size ? 'under 1' : Math.round(level)}% of the ${against}` : '';
      return `
      <a class="silo-col ${esc(r.repo)}" href="#/browse/${esc(r.repo)}" aria-label="${esc(FORMAT_LABELS[r.repo] ?? r.repo)}: ${esc(bytes(r.size))}, ${esc(plural(r.packages, r.repo === 'git' ? 'repo' : 'package'))}${esc(levelText)}" title="${esc(bytes(r.size))}${esc(levelText)}">
        <div class="silo"><div class="grain" data-fill="${fill.toFixed(1)}"></div></div>
        <div class="silo-label">
          ${chip(r.repo)}
          <span class="silo-size num">${esc(bytes(r.size))}</span>
          <span class="silo-meta">${esc(plural(r.packages, r.repo === 'git' ? 'repo' : 'package'))}</span>
          ${total ? `<span class="silo-meta">${Math.round(share)}% of cache</span>` : ''}
        </div>
      </a>`;
    })
    .join('');

  const upstreams = REPO_IDS.map((id) => {
    const h = health[id];
    const state = !h ? ['', 'not checked yet'] : h.offline ? ['', 'offline mode'] : h.ok ? ['ok', `reachable, ${h.latency_ms} ms`] : ['bad', `unreachable (${h.error || 'HTTP ' + h.status})`];
    return `<span class="status"><span class="dot ${state[0]}"></span>${esc(REPOS[id].upstream)} <span class="muted">${esc(state[1])}</span></span>`;
  }).join('');

  const recent = d.recent.length
    ? `<div class="panel table-wrap"><table>
        <thead><tr><th>Package</th><th>Version</th><th>Repository</th><th class="r">Size</th><th>Cached</th></tr></thead>
        <tbody>${d.recent
          .map(
            (a) => `<tr>
              <td><a class="pkg mono" href="#/c/${esc(a.repo)}/${esc(encodeURIComponent(a.package))}">${esc(a.package)}</a></td>
              <td class="mono">${esc(a.version)}</td>
              <td>${chip(a.repo)}</td>
              <td class="r num">${esc(bytes(a.size))}</td>
              <td>${timeTag(a.cached_at)}</td></tr>`
          )
          .join('')}</tbody></table></div>`
    : `<div class="panel empty"><strong>Nothing cached yet</strong>
        Point Cargo, npm, pip, Maven or Gradle at Granary and packages will appear here as they are downloaded.
        <p><a class="btn" href="#/repos">Show client setup</a></p></div>`;

  setView(`
    <div class="page-head">
      <div><h1>Overview</h1><p>${esc(plural(packages, 'package'))}${gitRepos ? ` and ${esc(plural(gitRepos, 'git repo'))}` : ''} cached, using ${esc(bytes(total))}.</p></div>
    </div>
    ${offlineBanner(d.health)}
    <section class="granary">
      <div class="panel">
        <div class="silos">${silos}</div>
        ${capacity ? `<p class="silo-scale">Gauges are full at the ${esc(bytes(capacity))} ${esc(against)}${limit ? '' : ' (no <code>GRANARY_CACHE_LIMIT</code> set)'}.</p>` : ''}
        <div class="upstreams">${upstreams}</div>
      </div>
      <dl class="panel facts">
        <div class="fact"><dt>Served from cache</dt><dd class="num">${hitRate == null ? '-' : hitRate + '%'}<small>${esc(count(hits))} of ${esc(count(served))} file requests since start</small></dd></div>
        <div class="fact"><dt>Fetched from upstream</dt><dd class="num">${esc(count(d.stats.file_downloads))}<small>new files since start</small></dd></div>
        <div class="fact"><dt>Metadata refreshes</dt><dd class="num">${esc(count(d.stats.meta_refreshes))}<small>${d.stats.meta_stale_served ? esc(count(d.stats.meta_stale_served)) + ' served stale while upstream was down' : 'none served stale'}</small></dd></div>
        ${limit ? `<div class="fact"><dt>Cache budget</dt><dd class="num">${esc(bytes(d.cache_used))}<small>of ${esc(bytes(limit))} used, files and metadata</small>
          <div class="meter ${budgetPct > 95 ? 'full' : budgetPct > 85 ? 'high' : ''}" role="img" aria-label="${Math.round(budgetPct)}% of the cache budget used"><span data-pct="${Math.min(100, budgetPct).toFixed(1)}"></span></div></dd></div>` : ''}
        <div class="fact"><dt>Disk</dt><dd class="num">${disk ? esc(bytes(disk.free)) + ' free' : '-'}<small>${disk ? 'of ' + esc(bytes(disk.total)) : 'unknown'}</small>
          ${disk ? `<div class="meter ${usedPct > 95 ? 'full' : usedPct > 85 ? 'high' : ''}" role="img" aria-label="${Math.round(usedPct)}% of disk used"><span data-pct="${usedPct.toFixed(1)}"></span></div>` : ''}</dd></div>
        <div class="fact"><dt>Running for</dt><dd>${esc(duration(d.uptime_secs))}<small>version ${esc(d.version)}</small></dd></div>
      </dl>
    </section>
    <section>
      <h2>Recently cached</h2>
      ${recent}
    </section>`);

  $$('.meter span', view()).forEach((el) => el.style.setProperty('--pct', el.dataset.pct + '%'));
  // The one animation in the UI: the silos fill when the overview opens.
  requestAnimationFrame(() =>
    requestAnimationFrame(() => $$('.grain', view()).forEach((el) => el.style.setProperty('--fill', el.dataset.fill + '%')))
  );
}

// ------------------------------------------------------------ repositories

async function renderRepos({ isCurrent }) {
  const d = await api('/api/repos');
  if (!isCurrent()) return;
  const blocks = d.repos
    .map((r) => {
      const h = r.health;
      const status = !h ? '<span class="status"><span class="dot"></span>Not checked yet</span>'
        : h.offline ? '<span class="status"><span class="dot"></span>Offline mode</span>'
        : h.ok ? `<span class="status"><span class="dot ok"></span>Online</span>`
        : `<span class="status"><span class="dot bad"></span>Upstream unreachable</span>`;
      return `
      <article class="repo">
        <div class="repo-head">
          <div>
            <h2>${chip(r.id)} <span>${esc(r.name)}</span> <span class="kind">Proxy</span></h2>
            <div class="repo-url"><code>${esc(r.url)}</code><button class="small" data-copy="${esc(r.url)}">Copy URL</button></div>
          </div>
          <div class="page-actions">
            <a class="btn" href="#/browse/${r.id}">Browse</a>
            <button class="admin-only" data-invalidate="${r.id}" data-name="${esc(r.name)}">Invalidate cache</button>
          </div>
        </div>
        <dl class="repo-grid">
          <div><dt>Packages</dt><dd class="num">${esc(count(r.packages))}</dd></div>
          <div><dt>Size</dt><dd class="num">${esc(bytes(r.size))}</dd></div>
          <div><dt>Metadata</dt><dd class="num">${esc(bytes(r.meta_size))}</dd></div>
          <div><dt>Status</dt><dd>${status}</dd></div>
        </dl>
        <div class="repo-upstream">${r.upstream.map((u) => `${esc(u.label)} from <code>${esc(u.url)}</code>`).join('<br>')}</div>
        <details class="setup">
          <summary>Client setup</summary>
          ${setupSnippets(r.setup)
            .map(
              (s) => `${s.label ? `<h3 class="setup-label">${esc(s.label)}</h3>` : ''}
          <div class="snippet"><pre>${esc(s.snippet)}</pre><button class="small" data-copy="${esc(s.snippet)}">Copy</button></div>
          <div class="snippet-file">Add to ${esc(s.file)}${d.client_auth ? '. Replace YOUR_TOKEN with a token from <a href="#/tokens">Access tokens</a>.' : ''}</div>
          ${s.after ? `<div class="snippet-file">${esc(s.after)}</div>` : ''}`
            )
            .join('')}
        </details>
      </article>`;
    })
    .join('');

  setView(`
    <div class="page-head">
      <div><h1>Repositories</h1><p>Each repository caches one public registry. Point your package manager at its URL.</p></div>
    </div>
    ${offlineBanner(d.repos.map((r) => r.health))}
    <div class="panel">${blocks}</div>`);

  view().addEventListener('click', async (e) => {
    const inv = e.target.closest('[data-invalidate]');
    if (!inv) return;
    const ok = await confirmAction({
      title: `Invalidate the ${inv.dataset.name} cache?`,
      body: 'Granary forgets its cached package listings for this repository and fetches fresh ones on the next request. Cached package files are kept.',
      ok: 'Invalidate cache',
    });
    if (!ok) return;
    try {
      const r = await act(() => api(`/api/repos/${inv.dataset.invalidate}/invalidate`, { method: 'POST' }));
      toast(`Invalidated ${inv.dataset.name} metadata, cleared ${bytes(r.freed)}`);
      route();
    } catch (err) {
      toast(err.message, 'error');
    }
  });
}

// ------------------------------------------------------------------ browse

const PAGE_SIZE = 50;
// Set when typing in the filter box causes the re-render, so focus and any
// text typed meanwhile carry over. Other navigation leaves it unset.
let filterTyping = false;

async function renderBrowse({ params, query, isCurrent }) {
  const repo = params[0] || '';
  const q = query.get('q') || '';
  const sort = query.get('sort') || 'name';
  const order = query.get('order') || (sort === 'name' ? 'asc' : 'desc');
  const page = Math.max(1, parseInt(query.get('page') || '1', 10) || 1);

  const apiQuery = new URLSearchParams({ sort, order, limit: PAGE_SIZE, offset: (page - 1) * PAGE_SIZE });
  if (repo) apiQuery.set('repo', repo);
  if (q) apiQuery.set('q', q);
  const d = await api('/api/components?' + apiQuery);
  if (!isCurrent()) return;

  const tabs = browseTabs(repo, q);

  const col = (key, label, cls = '') => {
    const active = sort === key;
    const nextOrder = active ? (order === 'asc' ? 'desc' : 'asc') : key === 'name' ? 'asc' : 'desc';
    const ariaSort = active ? ` aria-sort="${order === 'asc' ? 'ascending' : 'descending'}"` : '';
    const arrow = active ? (order === 'asc' ? ' ↑' : ' ↓') : '';
    return `<th class="${cls}"${ariaSort}><button class="sort" data-sort="${key}" data-order="${nextOrder}">${esc(label)}${arrow}</button></th>`;
  };

  // Maven, sorted by name: artifacts sit under a row for their group.
  const byGroup = repo === 'maven' && sort === 'name';
  let lastGroup = null;
  const rows = d.items
    .map((c) => {
      let head = '';
      let label = c.name;
      if (byGroup) {
        const { group, artifact } = mavenName(c.name);
        label = artifact;
        if (group !== lastGroup) {
          lastGroup = group;
          head = `<tr class="group-row"><th colspan="5" scope="colgroup"><button class="group-filter" data-group="${esc(group)}" title="Show only ${esc(group)}">${esc(group)}</button></th></tr>`;
        }
      }
      return `${head}<tr${byGroup ? ' class="in-group"' : ''}>
        <td><a class="pkg mono" href="#/c/${esc(c.repo)}/${esc(encodeURIComponent(c.name))}">${esc(label)}</a></td>
        ${repo ? '' : `<td>${chip(c.repo)}</td>`}
        <td class="r num">${esc(count(c.versions))}</td>
        <td class="r num">${esc(bytes(c.size))}</td>
        <td class="hide-sm">${timeTag(c.latest_cached)}</td>
        <td>${timeTag(c.last_access)}</td>
      </tr>`;
    })
    .join('');

  const from = d.total ? d.offset + 1 : 0;
  const to = d.offset + d.items.length;
  const pages = Math.max(1, Math.ceil(d.total / PAGE_SIZE));
  const pager = d.total > PAGE_SIZE
    ? `<div class="pager"><span>${esc(count(from))} to ${esc(count(to))} of ${esc(count(d.total))}</span>
        <span class="page-actions">
          <button class="small" data-page="${page - 1}" ${page <= 1 ? 'disabled' : ''}>Previous</button>
          <button class="small" data-page="${page + 1}" ${page >= pages ? 'disabled' : ''}>Next</button>
        </span></div>`
    : '';

  const where = repo ? REPOS[repo].label : 'any repository';
  // Re-rendering replaces the filter box; keep focus and any text typed meanwhile.
  const liveEl = filterTyping && document.activeElement?.id === 'filter' ? document.activeElement : null;
  filterTyping = false;
  const live = liveEl ? { value: liveEl.value, pos: liveEl.selectionStart } : null;

  const empty = q
    ? `<div class="empty"><strong>No cached packages match “${esc(q)}”</strong>Granary lists packages once a client has downloaded them through it.</div>`
    : `<div class="empty"><strong>Nothing cached in ${esc(where)} yet</strong>Packages appear here after a client downloads them through Granary. <a href="#/repos">Client setup</a></div>`;

  setView(`
    <div class="page-head">
      <div><h1>${q ? `Results for “${esc(q)}”` : 'Browse'}</h1>
      <p>${esc(plural(d.total, 'package'))} ${q ? 'match' : 'cached'}${repo ? ' in ' + esc(REPOS[repo].label) : ''}.</p></div>
    </div>
    <div class="toolbar">
      <nav class="seg" aria-label="Repository">${tabs}</nav>
      <div class="grow"><label class="sr-only" for="filter">Filter by name</label>
        <input id="filter" type="search" placeholder="Filter by name" value="${esc(q)}" autocomplete="off"></div>
    </div>
    <div class="panel">
      ${d.items.length
        ? `<div class="table-wrap"><table>
            <thead><tr>${col('name', repo === 'maven' ? 'Group / artifact' : 'Package')}${repo ? '' : '<th>Repository</th>'}${col('versions', 'Versions', 'r')}${col('size', 'Size', 'r')}${col('cached', 'Cached', 'hide-sm')}${col('used', 'Last downloaded')}</tr></thead>
            <tbody>${rows}</tbody></table></div>${pager}`
        : empty}
    </div>`);

  const update = (changes) => {
    const next = new URLSearchParams(query);
    for (const [k, v] of Object.entries(changes)) {
      if (v === '' || v == null) next.delete(k);
      else next.set(k, v);
    }
    go('/browse' + (repo ? '/' + repo : ''), next, { replace: true });
  };

  const filter = $('#filter');
  const onFilter = debounce(() => {
    filterTyping = true;
    update({ q: filter.value.trim(), page: '' });
  }, 250);
  filter.addEventListener('input', onFilter);
  if (live) {
    filter.value = live.value;
    filter.focus();
    filter.setSelectionRange(live.pos, live.pos);
    if (live.value.trim() !== q) onFilter();
  }
  view().addEventListener('click', (e) => {
    const s = e.target.closest('[data-sort]');
    if (s) return update({ sort: s.dataset.sort, order: s.dataset.order, page: '' });
    const g = e.target.closest('[data-group]');
    if (g) return update({ q: g.dataset.group + ':', page: '' });
    const p = e.target.closest('[data-page]');
    if (p && !p.disabled) update({ page: p.dataset.page });
  });
}

// --------------------------------------------------------------- component

async function renderComponent({ params, isCurrent }) {
  const [repo, name] = params;
  // Maven: Browse > group > artifact, then its versions.
  const maven = repo === 'maven' ? mavenName(name) : null;
  const crumbs = `<div class="crumbs"><a href="#/browse/${repo}">Browse ${esc(REPOS[repo].label)}</a>${
    maven && maven.group ? ` <span aria-hidden="true">/</span> <a class="mono" href="#/browse/maven?q=${esc(encodeURIComponent(maven.group + ':'))}">${esc(maven.group)}</a>` : ''
  }</div>`;
  const title = maven ? maven.artifact : name;
  let d;
  try {
    d = await api(`/api/component?repo=${repo}&name=${encodeURIComponent(name)}`);
  } catch (e) {
    if (e.status !== 404) throw e;
    if (!isCurrent()) return;
    setView(`${crumbs}
      <div class="page-head"><div><h1 class="title-mono">${esc(title)}</h1><p>This package is not cached. It will be downloaded the next time a client asks for it.</p></div></div>`);
    return;
  }
  if (!isCurrent()) return;

  const rows = d.versions
    .map((v, i) => {
      const single = v.files.length === 1 ? v.files[0] : null;
      const actions = single
        ? `<a class="btn small" href="${esc(safeUrl(single.download))}" download>Download</a>
           <button class="small danger admin-only" data-delete-file="${esc(single.path)}" data-label="${esc(single.file_name)}">Delete</button>`
        : '';
      const versionCell = single
        ? `<span class="mono">${esc(v.version)}</span>`
        : `<button class="expand" aria-expanded="false" aria-controls="files-${i}" data-expand="${i}">${icons.chevron}${esc(v.version)}</button>`;
      const filesRow = single
        ? ''
        : `<tr class="files hidden" id="files-${i}"><td colspan="6"><table><tbody>${v.files
            .map(
              (f) => `<tr>
                <td class="mono">${esc(f.file_name)}</td>
                <td class="r num">${esc(bytes(f.size))}</td>
                <td class="hide-sm">${timeTag(f.cached_at)}</td>
                <td>${timeTag(f.last_access)}</td>
                <td class="actions"><a class="btn small" href="${esc(safeUrl(f.download))}" download>Download</a>
                  <button class="small danger admin-only" data-delete-file="${esc(f.path)}" data-label="${esc(f.file_name)}">Delete</button></td>
              </tr>`
            )
            .join('')}</tbody></table></td></tr>`;
      return `<tr>
          <td>${versionCell}</td>
          <td class="r num">${esc(count(v.files.length))}</td>
          <td class="r num">${esc(bytes(v.size))}</td>
          <td class="hide-sm">${timeTag(v.cached_at)}</td>
          <td>${timeTag(v.last_access)}</td>
          <td class="actions">${actions}</td>
        </tr>${filesRow}`;
    })
    .join('');

  setView(`
    ${crumbs}
    <div class="page-head">
      <div>
        <h1 class="title-mono">${esc(title)}</h1>
        <div class="summary-line">
          ${chip(repo)}
          ${maven && maven.group ? `<span>Group <b class="mono">${esc(maven.group)}</b></span>` : ''}
          <span><b class="num">${esc(plural(d.versions.length, 'version'))}</b> cached</span>
          <span><b class="num">${esc(bytes(d.size))}</b> on disk</span>
          ${d.upstream ? `<a href="${esc(safeUrl(d.upstream.url))}" target="_blank" rel="noopener noreferrer">View on ${esc(d.upstream.label)}</a>` : ''}
        </div>
      </div>
      <div class="page-actions admin-only"><button class="danger" data-delete-package>Delete package</button></div>
    </div>
    <div class="panel table-wrap"><table>
      <thead><tr><th>Version</th><th class="r">Files</th><th class="r">Size</th><th class="hide-sm">Cached</th><th>Last downloaded</th><th class="r"><span class="sr-only">Actions</span></th></tr></thead>
      <tbody>${rows}</tbody>
    </table></div>`);

  view().addEventListener('click', async (e) => {
    const exp = e.target.closest('[data-expand]');
    if (exp) {
      const open = exp.getAttribute('aria-expanded') !== 'true';
      exp.setAttribute('aria-expanded', open);
      $('#files-' + exp.dataset.expand).classList.toggle('hidden', !open);
      return;
    }
    const delFile = e.target.closest('[data-delete-file]');
    if (delFile) {
      const ok = await confirmAction({
        title: `Delete ${delFile.dataset.label}?`,
        body: `Granary will download it again from ${REPOS[repo].upstream} the next time a client asks for it.`,
        ok: 'Delete file',
      });
      if (!ok) return;
      try {
        const r = await act(() => api(`/api/asset?repo=${repo}&path=${encodeURIComponent(delFile.dataset.deleteFile)}`, { method: 'DELETE' }));
        toast(`Deleted ${delFile.dataset.label}, freed ${bytes(r.bytes)}`);
        route();
      } catch (err) {
        toast(err.message, 'error');
      }
      return;
    }
    if (e.target.closest('[data-delete-package]')) {
      const ok = await confirmAction({
        title: `Delete ${d.name}?`,
        body: `This removes all ${plural(d.files, 'cached file')} (${bytes(d.size)}) and its cached metadata. Clients will download it from ${REPOS[repo].upstream} again when they need it.`,
        ok: 'Delete package',
      });
      if (!ok) return;
      try {
        const r = await act(() => api(`/api/component?repo=${repo}&name=${encodeURIComponent(d.name)}`, { method: 'DELETE' }));
        toast(`Deleted ${d.name}, freed ${bytes(r.bytes)}`);
        location.hash = '#/browse/' + repo;
      } catch (err) {
        toast(err.message, 'error');
      }
    }
  });
}

// ------------------------------------------------------------ get packages

const GET_ECOS = ['npm', 'cargo', 'pypi', 'maven'];
const SEARCH_HINTS = {
  npm: { placeholder: 'Search npm, e.g. react', lookup: 'Exact package name, e.g. react' },
  cargo: { placeholder: 'Search crates.io, e.g. serde', lookup: 'Exact crate name, e.g. serde' },
  pypi: { placeholder: 'Exact project name, e.g. requests', lookup: 'Exact project name, e.g. requests' },
  maven: { placeholder: 'Search Maven Central, e.g. guava', lookup: 'group:artifact, e.g. com.google.guava:guava' },
};

async function renderGet({ params, query, isCurrent }) {
  const eco = params[0] || 'npm';
  const q = (query.get('q') || '').trim();
  const status = await api('/api/packages/status');
  let d = null;
  let error = '';
  if (q) {
    try {
      d = await api(`/api/packages/search?eco=${eco}&q=${encodeURIComponent(q)}`);
    } catch (e) {
      error = e.message;
    }
  }
  if (!isCurrent()) return;

  const searching = status.search && eco !== 'pypi';
  const hint = SEARCH_HINTS[eco];
  const tabs = GET_ECOS.map((id) => {
    const qs = q ? '?q=' + encodeURIComponent(q) : '';
    return `<a href="#/get/${id}${qs}" aria-current="${id === eco}">${esc(REPOS[id].label)}</a>`;
  }).join('');
  const notes = [];
  if (status.note) notes.push(status.note);
  else if (eco === 'pypi') notes.push('PyPI has no search API, so type the exact project name.');
  if (searching) notes.push(`Search text is sent to ${new URL(status.hosts[eco]).host}.`);

  let body = '';
  if (error) {
    body = `<div class="panel panel-pad"><p class="err">${esc(error)}</p></div>`;
  } else if (d && !d.results.length) {
    body = `<div class="panel empty"><strong>Nothing found for “${esc(q)}”</strong>${
      d.mode === 'lookup' ? 'Lookups need the exact name.' : 'Try a shorter or different search.'
    }${status.offline ? ' Offline mode only finds packages Granary has already cached.' : ''}</div>`;
  } else if (d) {
    // Maven Central's search has no descriptions; skip the empty column.
    const described = d.results.some((r) => r.description);
    const rows = d.results
      .map(
        (r) => `<tr>
          <td>${r.cached_versions ? `<a class="pkg mono" href="#/c/${eco}/${esc(encodeURIComponent(r.name))}">${esc(r.name)}</a>` : `<span class="pkg mono">${esc(r.name)}</span>`}</td>
          <td class="mono">${esc(r.latest || '')}</td>
          ${described ? `<td class="desc">${esc(r.description || '')}</td>` : ''}
          <td>${r.cached_versions ? `<span class="status"><span class="dot ok"></span>${esc(plural(r.cached_versions, 'version'))}</span>` : '<span class="muted">Not cached</span>'}</td>
          <td class="actions"><button class="small" data-cache="${esc(r.name)}" data-latest="${esc(r.latest || '')}">Cache</button></td>
        </tr>`
      )
      .join('');
    body = `<div class="panel table-wrap"><table>
      <thead><tr><th>Package</th><th>Latest</th>${described ? '<th>Description</th>' : ''}<th>In Granary</th><th class="r"><span class="sr-only">Actions</span></th></tr></thead>
      <tbody>${rows}</tbody></table></div>`;
  }

  setView(`
    <div class="page-head">
      <div><h1>Get packages</h1><p>Find a package and cache it before anyone needs it.</p></div>
    </div>
    <div class="toolbar">
      <nav class="seg" aria-label="Ecosystem">${tabs}</nav>
      <form class="grow get-search" id="get-form" role="search">
        <label class="sr-only" for="get-q">${esc(searching ? 'Search' : 'Look up')}</label>
        <input id="get-q" type="search" value="${esc(q)}" placeholder="${esc(searching ? hint.placeholder : hint.lookup)}" autocomplete="off">
        <button class="primary" type="submit">${searching ? 'Search' : 'Look up'}</button>
      </form>
    </div>
    ${notes.length ? `<p class="page-note">${notes.map(esc).join(' ')}</p>` : ''}
    ${body}
    <dialog id="cache-dialog" aria-labelledby="cache-title"></dialog>`);

  $('#get-form').addEventListener('submit', (e) => {
    e.preventDefault();
    const text = $('#get-q').value.trim();
    go('/get/' + eco, text ? new URLSearchParams({ q: text }) : null);
  });
  if (!q) $('#get-q').focus();
  view().addEventListener('click', (e) => {
    const b = e.target.closest('[data-cache]');
    if (b) openCacheDialog(eco, b.dataset.cache, b.dataset.latest, status.warmer);
  });
}

/** Pick a version (and, for PyPI, a file) and cache it. */
async function openCacheDialog(eco, name, latest, warmer) {
  const dlg = $('#cache-dialog');
  dlg.innerHTML = `
    <form method="dialog" id="cache-form">
      <h2 id="cache-title">Cache <span class="mono">${esc(name)}</span></h2>
      <p id="cache-what"></p>
      <div class="stack">
        <div class="field"><label for="cache-version">Version</label>
          <select id="cache-version" disabled><option>Loading versions…</option></select></div>
        ${warmer ? '<label class="check"><input type="checkbox" id="cache-deps" checked> With dependencies</label>' : ''}
        ${eco === 'pypi' ? '<div class="field" id="cache-file-row"><label for="cache-file">File</label><select id="cache-file" disabled><option>Choose a version first</option></select><p class="help">Wheels are built for one platform; a file ending in -none-any.whl works everywhere.</p></div>' : ''}
      </div>
      <p class="err hidden" id="cache-err"></p>
      <div class="row">
        <button value="cancel" formnovalidate>Cancel</button>
        <button class="primary" id="cache-ok" value="ok" disabled>Cache</button>
      </div>
    </form>`;
  dlg.showModal();
  const err = $('#cache-err');
  const versionSel = $('#cache-version');
  const fileSel = $('#cache-file');
  const ok = $('#cache-ok');
  const deps = $('#cache-deps');
  const single = `Caches this one version${eco === 'maven' ? ' (its POM and artifact)' : ''}. Its dependencies are fetched when a client first asks for them.`;
  const withDeps = 'Starts a warm job that installs this version with the real package tool, so every dependency is cached too, for each default platform.';
  const describe = () => {
    $('#cache-what').textContent = deps?.checked ? withDeps : single;
    $('#cache-file-row')?.classList.toggle('hidden', !!deps?.checked);
  };
  describe();
  deps?.addEventListener('change', describe);

  const loadFiles = async () => {
    if (!fileSel) return;
    fileSel.disabled = true;
    ok.disabled = true;
    fileSel.innerHTML = '<option>Loading files…</option>';
    try {
      const f = await api(`/api/packages/files?name=${encodeURIComponent(name)}&version=${encodeURIComponent(versionSel.value)}`);
      if (!f.files.length) {
        fileSel.innerHTML = '<option>No files for this version</option>';
        return;
      }
      fileSel.innerHTML = f.files
        .map((x) => `<option value="${esc(x.path)}" ${x.path === f.suggested ? 'selected' : ''}>${esc(x.filename)}</option>`)
        .join('');
      fileSel.disabled = false;
      ok.disabled = false;
    } catch (e) {
      showError(err, e.message);
    }
  };

  try {
    const v = await api(`/api/packages/versions?eco=${eco}&name=${encodeURIComponent(name)}`);
    const pick = v.latest || latest || v.versions[0];
    versionSel.innerHTML = v.versions
      .map((x) => `<option value="${esc(x)}" ${x === pick ? 'selected' : ''}>${esc(x)}${x === v.latest ? ' (latest)' : ''}</option>`)
      .join('');
    versionSel.disabled = !v.versions.length;
    ok.disabled = !v.versions.length;
    if (fileSel) await loadFiles();
  } catch (e) {
    versionSel.innerHTML = '<option>Unavailable</option>';
    showError(err, e.message);
  }
  versionSel.addEventListener('change', loadFiles);

  $('#cache-form').addEventListener('submit', async (e) => {
    if (e.submitter?.value !== 'ok') return;
    e.preventDefault();
    ok.disabled = true;
    ok.textContent = 'Caching…';
    showError(err, '');
    if (deps?.checked) {
      dlg.close();
      return startWarm('/api/packages/warm', { eco, name, version: versionSel.value });
    }
    try {
      const body = { eco, name, version: versionSel.value };
      if (fileSel) body.file = fileSel.value;
      const r = await act(() => api('/api/packages/cache', { method: 'POST', body }));
      dlg.close();
      toast(`Cached ${name} ${body.version}: ${plural(r.files.length, 'file')}, ${bytes(r.bytes)}`);
      route();
    } catch (ex) {
      showError(err, ex.message);
      ok.disabled = false;
      ok.textContent = 'Cache';
    }
  });
}

// --------------------------------------------------------------- templates

const TEMPLATE_ECOS = ['cargo', 'npm', 'pypi', 'maven'];
const LINE_EXAMPLES = {
  cargo: 'serde = { version = "1", features = ["derive"] }\nanyhow = "*"',
  npm: '"react": "^19"\n"vite": "latest"',
  pypi: 'fastapi>=0.115\nuvicorn[standard]',
  maven: 'org.springframework.boot:spring-boot-starter-web:3.4.0\ncom.google.guava:guava',
};

const packageCounts = (counts) =>
  TEMPLATE_ECOS.filter((e) => counts[e])
    .map((e) => `<span class="count">${chip(e)} <b class="num">${esc(count(counts[e]))}</b></span>`)
    .join('');

async function renderTemplates({ query, isCurrent }) {
  const [d, warmer] = await Promise.all([api('/api/templates'), api('/api/warmer')]);
  if (!isCurrent()) return;
  const tag = query.get('tag') || '';
  const q = query.get('q') || '';
  const tags = [...new Set(d.templates.flatMap((t) => t.tags))].sort();
  const tagLink = (t) => {
    const qs = new URLSearchParams();
    if (t) qs.set('tag', t);
    if (q) qs.set('q', q);
    return '#/templates' + ([...qs].length ? '?' + qs : '');
  };
  const tagNav = [['', 'All'], ...tags.map((t) => [t, t])]
    .map(([id, label]) => `<a href="${tagLink(id)}" aria-current="${id === tag}">${esc(label)}</a>`)
    .join('');

  const cards = d.templates
    .filter((t) => !tag || t.tags.includes(tag))
    .map(
      (t) => `
      <article class="tpl-card panel" data-search="${esc([t.name, t.description, ...t.tags].join(' ').toLowerCase())}">
        <div class="tpl-head">
          <h2><a href="#/templates/${t.id}">${esc(t.name)}</a></h2>
          ${t.builtin ? '<span class="kind">Built-in</span>' : '<span class="kind custom">Custom</span>'}
        </div>
        <p class="tpl-desc">${esc(t.description || 'No description.')}</p>
        <div class="tpl-counts">${packageCounts(t.counts)}</div>
        <div class="tpl-tags">${t.tags.map((x) => `<a class="tag" href="${tagLink(x)}">${esc(x)}</a>`).join('')}</div>
        <div class="tpl-foot">
          <span class="muted">Last warmed ${t.last_warmed_at ? timeTag(t.last_warmed_at) : '<span>never</span>'}${t.schedule !== 'off' ? `, re-warms ${esc(t.schedule)}` : ''}</span>
          <span class="page-actions">
            ${warmer.available ? `<button class="small primary admin-only" data-warm="${t.id}">Warm</button>` : ''}
            ${t.builtin ? '' : `<a class="btn small admin-only" href="#/templates/${t.id}/edit">Edit</a>`}
            <button class="small admin-only" data-clone="${t.id}">Clone</button>
            <a class="btn small" href="/api/templates/${t.id}/export" download>Export</a>
            <button class="small" data-starter="${t.id}">Download starter</button>
            ${t.builtin ? '' : `<button class="small danger admin-only" data-delete="${t.id}" data-name="${esc(t.name)}">Delete</button>`}
          </span>
        </div>
      </article>`
    )
    .join('');

  setView(`
    <div class="page-head">
      <div><h1>Templates</h1><p>Named sets of packages for a kind of project. Built-in templates are read-only; clone one to change it.</p></div>
      <div class="page-actions admin-only"><a class="btn primary" href="#/templates/new">New template</a></div>
    </div>
    ${warmerNote(warmer)}
    <div class="toolbar">
      ${tags.length ? `<nav class="seg" aria-label="Tag">${tagNav}</nav>` : ''}
      <div class="grow"><label class="sr-only" for="tpl-filter">Filter templates</label>
        <input id="tpl-filter" type="search" placeholder="Filter templates" value="${esc(q)}" autocomplete="off"></div>
    </div>
    <div class="tpl-grid">${cards}</div>
    <div class="panel empty hidden" id="tpl-none"><strong>No templates match</strong>Try another word or tag.</div>`);

  const filter = $('#tpl-filter');
  const apply = () => {
    const needle = filter.value.trim().toLowerCase();
    let shown = 0;
    $$('.tpl-card', view()).forEach((c) => {
      const hit = !needle || c.dataset.search.includes(needle);
      c.classList.toggle('hidden', !hit);
      if (hit) shown++;
    });
    $('#tpl-none').classList.toggle('hidden', shown > 0);
  };
  filter.addEventListener('input', apply);
  apply();
  view().addEventListener('click', (e) => templateAction(e));
}

/** Warm, starter, clone and delete buttons, on the list and on a template's page. */
async function templateAction(e) {
  const starter = e.target.closest('[data-starter]');
  if (starter) {
    starter.disabled = true;
    await downloadStarter(starter.dataset.starter);
    starter.disabled = false;
    return;
  }
  const warm = e.target.closest('[data-warm]');
  if (warm) {
    warm.disabled = true;
    await startWarm(`/api/templates/${warm.dataset.warm}/warm`);
    warm.disabled = false;
    return;
  }
  const clone = e.target.closest('[data-clone]');
  if (clone) {
    try {
      const r = await act(() => api(`/api/templates/${clone.dataset.clone}/clone`, { method: 'POST' }));
      toast(`Created ${r.name}`);
      location.hash = `#/templates/${r.id}/edit`;
    } catch (err) {
      toast(err.message, 'error');
    }
    return;
  }
  const del = e.target.closest('[data-delete]');
  if (del) {
    const ok = await confirmAction({
      title: `Delete ${del.dataset.name}?`,
      body: 'The template is removed. Packages it cached stay in Granary.',
      ok: 'Delete template',
    });
    if (!ok) return;
    try {
      await act(() => api(`/api/templates/${del.dataset.delete}`, { method: 'DELETE' }));
      toast(`Deleted ${del.dataset.name}`);
      if (location.hash.startsWith('#/templates/')) location.hash = '#/templates';
      else route();
    } catch (err) {
      toast(err.message, 'error');
    }
  }
}

/** Fetch a template's starter zip and save it, or say why it can't be made. */
async function downloadStarter(id) {
  let res;
  try {
    res = await fetch(`/api/templates/${id}/starter`, { credentials: 'same-origin' });
  } catch {
    return toast('Granary is not responding. Check that the server is running.', 'error');
  }
  if (!res.ok) {
    const text = (await res.text()).trim();
    return toast(text ? text[0].toUpperCase() + text.slice(1) + '.' : `Couldn't make the starter (HTTP ${res.status}).`, 'error');
  }
  const name = /filename="([^"]+)"/.exec(res.headers.get('Content-Disposition') || '')?.[1] || 'starter.zip';
  const url = URL.createObjectURL(await res.blob());
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10000);
}

const SCHEDULE_LABELS = { off: 'Off', daily: 'Daily', weekly: 'Weekly' };

async function renderTemplate({ params, isCurrent }) {
  const [t, list, warmer, jobs] = await Promise.all([
    api(`/api/templates/${params[0]}`),
    api('/api/templates'),
    api('/api/warmer'),
    api(`/api/jobs?template=${params[0]}`),
  ]);
  if (!isCurrent()) return;
  const platforms = list.platforms;
  const label = (id) => platforms.find((p) => p.id === id)?.label || id;
  const sections = TEMPLATE_ECOS.filter((e) => t[e].trim())
    .map(
      (e) => `<section class="tpl-section">
        <h2>${chip(e)} <span class="muted">${esc(plural(t.counts[e], 'package'))}</span></h2>
        <div class="snippet"><pre>${esc(t[e])}</pre></div>
      </section>`
    )
    .join('');
  setView(`
    <div class="crumbs"><a href="#/templates">Templates</a></div>
    <div class="page-head">
      <div>
        <h1>${esc(t.name)}</h1>
        <div class="summary-line">
          ${t.builtin ? '<span class="kind">Built-in</span>' : '<span class="kind custom">Custom</span>'}
          ${t.tags.map((x) => `<a class="tag" href="#/templates?tag=${esc(encodeURIComponent(x))}">${esc(x)}</a>`).join('')}
          <span>Last warmed ${t.last_warmed_at ? timeTag(t.last_warmed_at) : 'never'}</span>
        </div>
        ${t.description ? `<p>${esc(t.description)}</p>` : ''}
      </div>
      <div class="page-actions">
        ${warmer.available ? `<button class="primary admin-only" data-warm="${t.id}">Warm</button>` : ''}
        ${t.builtin ? '' : `<a class="btn admin-only" href="#/templates/${t.id}/edit">Edit</a>`}
        <button class="admin-only" data-clone="${t.id}">Clone</button>
        <a class="btn" href="/api/templates/${t.id}/export" download>Export</a>
        <button data-starter="${t.id}">Download starter</button>
        ${t.builtin ? '' : `<button class="danger admin-only" data-delete="${t.id}" data-name="${esc(t.name)}">Delete</button>`}
      </div>
    </div>
    <dl class="panel panel-pad tpl-facts">
      <div><dt>Platforms</dt><dd>${t.platforms.map((p) => esc(label(p))).join(', ')}</dd></div>
      <div><dt>Source distributions</dt><dd>${t.allow_sdist ? 'Allowed (building one can run code)' : 'Wheels only'}</dd></div>
      <div><dt><label for="tpl-schedule">Re-warm automatically</label></dt><dd>${
        isAdmin()
          ? `<select id="tpl-schedule">${Object.entries(SCHEDULE_LABELS).map(([id, label]) => `<option value="${id}" ${t.schedule === id ? 'selected' : ''}>${label}</option>`).join('')}</select>`
          : esc(SCHEDULE_LABELS[t.schedule] || t.schedule)
      }</dd></div>
    </dl>
    ${t.schedule !== 'off' && !warmer.available ? '<p class="page-note">Scheduled re-warms wait until the warmer is available.</p>' : ''}
    ${warmerNote(warmer)}
    ${sections}
    ${jobs.jobs.length ? `<section><h2>Recent warm jobs</h2><div class="panel table-wrap"><table>
      <thead><tr><th>Job</th><th>Status</th><th>Queued</th><th>Newly cached</th></tr></thead>
      <tbody>${jobs.jobs.slice(0, 5).map((j) => `<tr><td><a href="#/jobs/${j.id}">#${esc(j.id)}</a></td><td>${jobStatus(j.status)}</td><td>${timeTag(j.created_at)}</td><td class="num">${esc(jobAdded(j))}</td></tr>`).join('')}</tbody>
    </table></div></section>` : ''}`);
  view().addEventListener('click', (e) => templateAction(e));
  $('#tpl-schedule')?.addEventListener('change', async (e) => {
    const sel = e.target;
    sel.disabled = true;
    try {
      await act(() => api(`/api/templates/${t.id}/schedule`, { method: 'PUT', body: { schedule: sel.value } }));
      toast(sel.value === 'off' ? 'Scheduled re-warms turned off' : `${t.name} will be warmed again ${sel.value === 'daily' ? 'every day' : 'every week'}`);
    } catch (err) {
      toast(err.message, 'error');
    }
    sel.disabled = false;
  });
}

async function renderTemplateEditor({ params, isCurrent }) {
  const id = params[0] || null;
  const [list, t] = await Promise.all([api('/api/templates'), id ? api(`/api/templates/${id}`) : null]);
  if (!isCurrent()) return;
  if (t?.builtin) {
    setView(`<div class="crumbs"><a href="#/templates">Templates</a></div>
      <div class="page-head"><div><h1>${esc(t.name)} is built in</h1><p>Built-in templates are read-only. Clone it to make your own version.</p></div></div>
      <button class="primary" data-clone="${t.id}">Clone ${esc(t.name)}</button>`);
    view().addEventListener('click', (e) => templateAction(e));
    return;
  }
  const v = t || { name: '', description: '', tags: [], platforms: list.default_platforms, allow_sdist: false, cargo: '', npm: '', pypi: '', maven: '' };

  const ecoBlock = (e) => `
    <div class="tpl-eco" data-eco="${e}">
      <div class="tpl-eco-head">
        <label for="tpl-${e}">${chip(e)} <span class="muted" id="tpl-count-${e}"></span></label>
        <button type="button" class="small" data-import="${e}">Import from manifest</button>
      </div>
      <textarea id="tpl-${e}" class="mono" rows="7" spellcheck="false" placeholder="${esc(LINE_EXAMPLES[e])}">${esc(v[e])}</textarea>
      <ul class="line-errors" id="tpl-err-${e}"></ul>
    </div>`;

  setView(`
    <div class="crumbs"><a href="#/templates">Templates</a>${t ? ` <span aria-hidden="true">/</span> <a href="#/templates/${t.id}">${esc(t.name)}</a>` : ''}</div>
    <div class="page-head"><div><h1>${t ? 'Edit template' : 'New template'}</h1>
      <p>One package per line, in each ecosystem's own syntax. Lines starting with # are comments.</p></div></div>
    <form class="panel panel-pad" id="tpl-form" novalidate>
      <div class="form-grid">
        <div class="label"><label for="tpl-name">Name</label></div>
        <div><input type="text" id="tpl-name" maxlength="80" value="${esc(v.name)}"><p class="err hidden" id="tpl-err-name"></p></div>
        <div class="label"><label for="tpl-desc">Description</label></div>
        <div><input type="text" id="tpl-desc" maxlength="500" value="${esc(v.description)}"></div>
        <div class="label"><label for="tpl-tags">Tags</label></div>
        <div><input type="text" id="tpl-tags" value="${esc(v.tags.join(', '))}" placeholder="rust, web">
          <div class="help">Comma-separated. Lowercase letters, digits and dashes.</div><p class="err hidden" id="tpl-err-tags"></p></div>
        <div class="label">Platforms</div>
        <div><div class="inline">${list.platforms
          .map((p) => `<label class="check"><input type="checkbox" name="platform" value="${esc(p.id)}" ${v.platforms.includes(p.id) ? 'checked' : ''}> ${esc(p.label)}</label>`)
          .join('')}</div>
          <div class="help">Packages with per-platform builds are cached for each one chosen.</div><p class="err hidden" id="tpl-err-platforms"></p></div>
        <div class="label">Python builds</div>
        <div><label class="check"><input type="checkbox" id="tpl-sdist" ${v.allow_sdist ? 'checked' : ''}> Allow source distributions (sdists)</label>
          <div class="banner sdist-warning ${v.allow_sdist ? '' : 'hidden'}" id="tpl-sdist-warn"><span class="dot bad"></span><span>Building an sdist runs the package's own build code. Leave this off to cache wheels only.</span></div></div>
      </div>
      <div class="tpl-ecos">${TEMPLATE_ECOS.map(ecoBlock).join('')}</div>
      <p class="err hidden" id="tpl-err-packages"></p>
      <div class="page-actions tpl-save">
        <button class="primary" type="submit">${t ? 'Save changes' : 'Create template'}</button>
        <a class="btn" href="${t ? `#/templates/${t.id}` : '#/templates'}">Cancel</a>
      </div>
    </form>
    <dialog id="import-dialog" aria-labelledby="import-title"></dialog>`);

  const read = () => ({
    name: $('#tpl-name').value,
    description: $('#tpl-desc').value,
    tags: $('#tpl-tags').value.split(',').map((s) => s.trim()).filter(Boolean),
    platforms: $$('input[name="platform"]:checked', view()).map((c) => c.value),
    allow_sdist: $('#tpl-sdist').checked,
    ...Object.fromEntries(TEMPLATE_ECOS.map((e) => [e, $('#tpl-' + e).value])),
  });

  // Field problems wait until something has been typed or a save tried,
  // so a blank new template doesn't open full of errors.
  let touched = !!t;
  const sentence = (m) => m[0].toUpperCase() + m.slice(1) + '.';
  const show = (r) => {
    for (const f of ['name', 'tags', 'platforms', 'packages']) {
      const msgs = touched ? r.problems.fields.filter((x) => x.field === f).map((x) => sentence(x.message)) : [];
      showError($('#tpl-err-' + f), msgs.join(' '));
    }
    for (const e of TEMPLATE_ECOS) {
      $('#tpl-count-' + e).textContent = r.counts[e] ? plural(r.counts[e], 'package') : '';
      const errs = r.problems.lines[e] || [];
      $('#tpl-err-' + e).innerHTML = errs
        .map((x) => `<li><b>Line ${esc(x.line)}</b> <code>${esc(x.text)}</code> ${esc(sentence(x.message))}</li>`)
        .join('');
      $('#tpl-' + e).toggleAttribute('aria-invalid', errs.length > 0);
    }
  };
  const check = async () => {
    const r = await api('/api/templates/check' + (id ? '?id=' + id : ''), { method: 'POST', body: read() });
    if (isCurrent()) show(r);
    return r;
  };
  const recheck = debounce(() => check().catch(() => {}), 400);
  $('#tpl-form').addEventListener('input', () => {
    touched = true;
    recheck();
  });
  $('#tpl-sdist').addEventListener('change', (e) => $('#tpl-sdist-warn').classList.toggle('hidden', !e.target.checked));
  check().catch(() => {});

  $('#tpl-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    touched = true;
    const r = await check();
    if (!r.ok) {
      toast('Fix the problems shown before saving', 'error');
      return;
    }
    try {
      const body = read();
      const out = await act(() => api(id ? `/api/templates/${id}` : '/api/templates', { method: id ? 'PUT' : 'POST', body }));
      toast(id ? 'Template saved' : 'Template created');
      location.hash = `#/templates/${out.id}`;
    } catch (ex) {
      toast(ex.message, 'error');
    }
  });

  view().addEventListener('click', (e) => {
    const b = e.target.closest('[data-import]');
    if (b) openImportDialog(b.dataset.import, recheck);
  });
}

const MANIFESTS = { cargo: 'Cargo.toml', npm: 'package.json', pypi: 'requirements.txt', maven: 'pom.xml' };

/** Fill a section from a Cargo.toml, package.json, requirements.txt or pom.xml. */
function openImportDialog(eco, changed) {
  const dlg = $('#import-dialog');
  dlg.innerHTML = `
    <form method="dialog" id="import-form">
      <h2 id="import-title">Import from ${esc(MANIFESTS[eco])}</h2>
      <p>Choose the file or paste its contents. Its dependencies are added to the ${esc(REPOS[eco].label)} section.</p>
      <div class="stack">
        <div class="field"><label for="import-file">File</label><input type="file" id="import-file" accept=".toml,.json,.txt,.in,.xml,.pom"></div>
        <div class="field"><label for="import-text">Or paste it</label><textarea id="import-text" class="mono" rows="8" spellcheck="false"></textarea></div>
      </div>
      <ul class="line-errors" id="import-skipped"></ul>
      <p class="err hidden" id="import-err"></p>
      <div class="row">
        <button value="cancel" formnovalidate>Close</button>
        <button class="primary" value="ok">Import</button>
      </div>
    </form>`;
  dlg.showModal();
  let filename = null;
  $('#import-file').addEventListener('change', async (e) => {
    const f = e.target.files[0];
    if (!f) return;
    if (f.size > 1024 * 1024) return showError($('#import-err'), 'That file is larger than 1 MB.');
    filename = f.name;
    $('#import-text').value = await f.text();
  });
  $('#import-form').addEventListener('submit', async (e) => {
    if (e.submitter?.value !== 'ok') return;
    e.preventDefault();
    showError($('#import-err'), '');
    const content = $('#import-text').value;
    if (!content.trim()) return showError($('#import-err'), 'Choose a file or paste one first.');
    try {
      const r = await api('/api/templates/import', { method: 'POST', body: { filename: filename || MANIFESTS[eco], content } });
      if (r.ecosystem !== eco) {
        return showError($('#import-err'), `That looks like a ${REPOS[r.ecosystem].label} manifest. Use the import button on the ${REPOS[r.ecosystem].label} section.`);
      }
      const ta = $('#tpl-' + eco);
      const have = new Set(ta.value.split('\n').map((l) => l.trim()));
      const added = r.lines.filter((l) => !have.has(l));
      ta.value = [ta.value.trimEnd(), ...added].filter(Boolean).join('\n');
      changed();
      toast(`Added ${plural(added.length, 'package')}`);
      if (r.skipped.length) {
        $('#import-skipped').innerHTML = `<li><b>Skipped</b></li>` + r.skipped.map((s) => `<li>${esc(s)}</li>`).join('');
      } else {
        dlg.close();
      }
    } catch (ex) {
      showError($('#import-err'), ex.message);
    }
  });
}

// -------------------------------------------------------------------- jobs

const JOB_STATES = {
  queued: ['', 'Queued'],
  running: ['warn', 'Running'],
  done: ['ok', 'Done'],
  failed: ['bad', 'Failed'],
  cancelled: ['', 'Cancelled'],
};
const jobStatus = (s) => {
  const [dot, label] = JOB_STATES[s] || ['', s];
  return `<span class="status"><span class="dot ${dot}"></span>${esc(label)}</span>`;
};
const jobActive = (j) => j.status === 'queued' || j.status === 'running';
const jobDuration = (j) => (j.started_at ? duration((j.finished_at || Date.now() / 1000) - j.started_at) : '-');
const jobAdded = (j) =>
  j.new_files == null ? '-' : `${plural(j.new_packages, j.kind === 'git' ? 'new repo' : 'package')}, ${bytes(j.new_bytes)}`;

/** Start a warm job and open its page. */
async function startWarm(path, body) {
  try {
    const r = await act(() => api(path, { method: 'POST', body }));
    toast('Warm job queued');
    location.hash = `#/jobs/${r.id}`;
  } catch (err) {
    toast(err.message, 'error');
  }
}

/** Says why warming isn't available, for admins. */
const warmerNote = (w) => {
  if (w.available) return '';
  const why = w.offline
    ? 'Offline mode is on, so templates can\'t be warmed.'
    : !w.configured
      ? 'Warming templates needs the optional granary-warmer service. See the README to turn it on.'
      : `The warmer isn't answering${w.error ? `: ${w.error}` : ''}. Is the granary-warmer container running?`;
  return `<div class="banner admin-only"><span class="dot"></span><span>${esc(why)}</span></div>`;
};

async function renderJobs({ isCurrent }) {
  const d = await api('/api/jobs');
  if (!isCurrent()) return;
  const rows = d.jobs
    .map(
      (j) => `<tr>
        <td class="num">#${esc(j.id)}</td>
        <td><a href="#/jobs/${j.id}">${esc(j.title)}</a> <span class="muted">${j.kind === 'git' ? 'repo' : j.kind}</span></td>
        <td>${jobStatus(j.status)}</td>
        <td>${timeTag(j.created_at)} <span class="muted">by ${esc(j.created_by)}</span></td>
        <td class="num hide-sm">${esc(jobDuration(j))}</td>
        <td class="num">${esc(jobAdded(j))}</td>
        <td class="actions">${jobActive(j) ? `<button class="small admin-only" data-cancel-job="${j.id}">Cancel</button>` : ''}</td>
      </tr>`
    )
    .join('');
  setView(`
    <div class="page-head">
      <div><h1>Jobs</h1><p>Warm jobs fill the cache with every package a template needs. Up to ${esc(d.max_running)} run at once; the last 200 are kept.</p></div>
    </div>
    <div class="panel">${
      d.jobs.length
        ? `<div class="table-wrap"><table>
            <thead><tr><th>Job</th><th>What</th><th>Status</th><th>Queued</th><th class="hide-sm">Took</th><th>Newly cached</th><th class="r"><span class="sr-only">Actions</span></th></tr></thead>
            <tbody>${rows}</tbody></table></div>`
        : '<div class="empty"><strong>No jobs yet</strong>Warm a template, or cache a package with its dependencies, and its job appears here.</div>'
    }</div>`);
  view().addEventListener('click', (e) => cancelJob(e));
  // Refresh while anything is queued or running.
  if (d.jobs.some(jobActive)) {
    const t = setTimeout(() => isCurrent() && route(), 2000);
    onLeave(() => clearTimeout(t));
  }
}

async function cancelJob(e) {
  const b = e.target.closest('[data-cancel-job]');
  if (!b) return;
  b.disabled = true;
  try {
    await act(() => api(`/api/jobs/${b.dataset.cancelJob}/cancel`, { method: 'POST' }));
    toast('Cancelling job');
  } catch (err) {
    toast(err.message, 'error');
    b.disabled = false;
  }
}

async function renderJob({ params, isCurrent }) {
  const id = params[0];
  let d = await api(`/api/jobs/${id}`);
  if (!isCurrent()) return;
  let last = 0;

  const head = (j) => `
    <div class="crumbs"><a href="#/jobs">Jobs</a>${j.template_id ? ` <span aria-hidden="true">/</span> <a href="#/templates/${j.template_id}">Template</a>` : ''}</div>
    <div class="page-head">
      <div><h1>${esc(j.title)}</h1>
        <div class="summary-line">${jobStatus(j.status)}<span>Job #${esc(j.id)}</span><span>Queued ${timeTag(j.created_at)} by ${esc(j.created_by)}</span></div></div>
      <div class="page-actions">${jobActive(j) ? `<button class="admin-only" data-cancel-job="${j.id}">Cancel job</button>` : ''}</div>
    </div>
    ${j.message ? `<div class="banner"><span class="dot ${j.status === 'failed' ? 'bad' : ''}"></span><span>${esc(j.message[0].toUpperCase() + j.message.slice(1))}</span></div>` : ''}
    <dl class="panel panel-pad tpl-facts">
      <div><dt>Status</dt><dd>${esc(JOB_STATES[j.status]?.[1] || j.status)}</dd></div>
      <div><dt>Running time</dt><dd>${esc(jobDuration(j))}</dd></div>
      <div><dt>${j.kind === 'git' ? 'New repos' : 'New packages'}</dt><dd class="num">${j.new_packages == null ? '-' : esc(count(j.new_packages))}</dd></div>
      ${j.kind === 'git' ? '' : `<div><dt>New files</dt><dd class="num">${j.new_files == null ? '-' : esc(count(j.new_files))}</dd></div>`}
      <div><dt>Newly cached</dt><dd class="num">${j.new_bytes == null ? '-' : esc(bytes(j.new_bytes))}</dd></div>
    </dl>`;

  setView(`<div id="job-head">${head(d.job)}</div>
    <h2>Log</h2>
    <div class="panel log" id="job-log" role="log" aria-live="polite"></div>`);
  view().addEventListener('click', (e) => cancelJob(e));

  const box = $('#job-log');
  const append = (lines) => {
    if (!lines.length) return;
    const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
    box.insertAdjacentHTML(
      'beforeend',
      lines.map((l) => `<div class="job-line${l.line.startsWith('$ ') ? ' cmd' : l.line.startsWith('---') ? ' step' : ''}">${esc(l.line)}</div>`).join('')
    );
    last = lines[lines.length - 1].n;
    if (atBottom) box.scrollTop = box.scrollHeight;
  };
  append(d.lines);
  if (!d.lines.length) box.innerHTML = '<div class="empty"><strong>Waiting to start</strong>Lines appear here as the tools run.</div>';

  // Poll every 2 seconds for new lines until the job ends.
  const poll = async () => {
    if (!isCurrent()) return;
    try {
      d = await api(`/api/jobs/${id}?after=${last}`);
    } catch {
      return schedule();
    }
    if (!isCurrent()) return;
    if (d.lines.length && !last) box.innerHTML = '';
    append(d.lines);
    $('#job-head').innerHTML = head(d.job);
    if (jobActive(d.job)) schedule();
  };
  let timer;
  const schedule = () => {
    timer = setTimeout(poll, 2000);
  };
  onLeave(() => clearTimeout(timer));
  if (jobActive(d.job)) schedule();
}

// --------------------------------------------------------------- git repos

/** The Browse tabs: every package format, then Git. */
const browseTabs = (current, q = '') => {
  const qs = q ? '?' + new URLSearchParams({ q }) : '';
  return [['', 'All'], ...REPO_IDS.map((id) => [id, REPOS[id].label])]
    .map(([id, label]) => `<a href="#/browse${id ? '/' + id : ''}${qs}" aria-current="${id === current}">${esc(label)}</a>`)
    .concat(`<a href="#/browse/git" aria-current="${current === 'git'}">Git</a>`)
    .join('');
};

async function renderGitRepos({ isCurrent }) {
  const d = await api('/api/git/repos');
  if (!isCurrent()) return;
  const rows = d.repos
    .map(
      (r) => `<tr>
        <td><span class="pkg mono">${esc(r.host)}/${esc(r.owner)}/${esc(r.name)}</span>
          ${r.last_error ? `<div class="err" title="${esc(r.last_error)}">Last refresh failed; serving the cached copy</div>` : ''}</td>
        <td class="mono">${esc(r.default_branch || '-')}</td>
        <td class="r num">${esc(bytes(r.size))}</td>
        <td>${timeTag(r.refreshed_at)}</td>
        <td class="actions">
          <button class="small" data-copy="${esc(r.clone_url)}" title="${esc(r.clone_url)}">Copy clone URL</button>
          <button class="small admin-only" data-git-refresh="${esc(JSON.stringify({ host: r.host, owner: r.owner, name: r.name }))}" ${d.offline ? 'disabled' : ''}>Refresh</button>
          <button class="small danger admin-only" data-git-delete="${esc(new URLSearchParams({ host: r.host, owner: r.owner, name: r.name }).toString())}" data-label="${esc(r.host + '/' + r.owner + '/' + r.name)}">Delete</button>
        </td>
      </tr>`
    )
    .join('');
  setView(`
    <div class="page-head">
      <div><h1>Git</h1><p>${esc(plural(d.repos.length, 'repo'))} cached, from ${esc(d.hosts.join(', '))}.</p></div>
    </div>
    <div class="toolbar"><nav class="seg" aria-label="Repository">${browseTabs('git')}</nav></div>
    <p class="page-note">Granary keeps a mirror of each repo and serves it read-only. Submodules and Git LFS objects are not cached, and only public repos can be added.</p>
    ${d.offline ? '' : `<form class="panel panel-pad admin-only add-repo" id="add-repo">
      <label for="repo-url">Add repo</label>
      <div class="get-search">
        <input id="repo-url" type="text" placeholder="https://github.com/owner/repo" autocomplete="off" spellcheck="false">
        <button class="primary" type="submit">Add repo</button>
      </div>
      <p class="err hidden" id="repo-err"></p>
    </form>`}
    <div class="panel">${
      d.repos.length
        ? `<div class="table-wrap"><table>
            <thead><tr><th>Repo</th><th>Default branch</th><th class="r">Size</th><th>Refreshed</th><th class="r"><span class="sr-only">Actions</span></th></tr></thead>
            <tbody>${rows}</tbody></table></div>`
        : `<div class="empty"><strong>No repos cached yet</strong>Clone through Granary (see the README's git setup), or add one above.</div>`
    }</div>`);

  $('#add-repo')?.addEventListener('submit', async (e) => {
    e.preventDefault();
    const url = $('#repo-url').value.trim();
    if (!url) return showError($('#repo-err'), 'Paste a repo URL first.');
    showError($('#repo-err'), '');
    try {
      const r = await act(() => api('/api/git/repos', { method: 'POST', body: { url } }));
      toast('Caching the repo');
      location.hash = `#/jobs/${r.id}`;
    } catch (err) {
      showError($('#repo-err'), err.message);
    }
  });
  view().addEventListener('click', async (e) => {
    const refresh = e.target.closest('[data-git-refresh]');
    if (refresh) {
      refresh.disabled = true;
      try {
        const r = await act(() => api('/api/git/repos/refresh', { method: 'POST', body: JSON.parse(refresh.dataset.gitRefresh) }));
        location.hash = `#/jobs/${r.id}`;
      } catch (err) {
        toast(err.message, 'error');
        refresh.disabled = false;
      }
      return;
    }
    const del = e.target.closest('[data-git-delete]');
    if (del) {
      const ok = await confirmAction({
        title: `Delete ${del.dataset.label}?`,
        body: 'Granary deletes its copy of the repo. It is cloned again the next time a client asks for it.',
        ok: 'Delete repo',
      });
      if (!ok) return;
      try {
        const r = await act(() => api(`/api/git/repos?${del.dataset.gitDelete}`, { method: 'DELETE' }));
        toast(`Deleted ${del.dataset.label}, freed ${bytes(r.bytes)}`);
        route();
      } catch (err) {
        toast(err.message, 'error');
      }
    }
  });
}

// ----------------------------------------------------------------- cleanup

async function renderCleanup({ isCurrent }) {
  const d = await api('/api/cleanup');
  if (!isCurrent()) return;
  const p = d.policy;
  const last = d.last_run;

  const lastRun = last
    ? `Last run ${timeTag(last.at)}: removed ${esc(plural(last.files, 'file'))} and ${esc(plural(last.meta_files, 'metadata entry', 'metadata entries'))}, freeing ${esc(bytes(last.bytes + last.meta_bytes))}.`
    : 'Cleanup has not run yet.';

  setView(`
    <div class="page-head">
      <div><h1>Cleanup</h1><p>Remove packages your builds no longer use. Anything removed is downloaded again if a client asks for it later.</p></div>
    </div>
    <form class="panel panel-pad" id="policy" novalidate>
      <div class="form-grid">
        <div class="label">Schedule</div>
        <div><label class="check"><input type="checkbox" name="enabled" ${p.enabled ? 'checked' : ''}> Run automatically every day</label>
          <div class="help">When off, cleanup only runs when you choose Run cleanup now.</div></div>

        <div class="label"><label for="days">Remove packages unused for</label></div>
        <div><div class="inline"><input type="number" id="days" name="days" min="1" max="3650" step="1" value="${esc(p.unused_days)}"> days</div>
          <div class="help">A package counts as used each time a client downloads it through Granary. Packages that have never been downloaded count from the day they were cached.</div></div>

        <div class="label">Repositories</div>
        <div><div class="inline">${REPO_IDS.map((id) => `<label class="check"><input type="checkbox" name="repo" value="${id}" ${p.repos.includes(id) ? 'checked' : ''}> ${chip(id)}</label>`).join('')}</div></div>

        <div class="label">Metadata</div>
        <div><label class="check"><input type="checkbox" name="meta" ${p.include_metadata ? 'checked' : ''}> Also remove package listings nobody has requested in that time</label>
          <div class="help">Listings are small and are fetched again on demand.</div></div>
      </div>
      <p class="err hidden" id="policy-err" role="alert"></p>
      <div class="form-actions">
        <button class="primary admin-only" type="submit">Save policy</button>
        <button type="button" data-preview>Preview</button>
        <button type="button" class="danger admin-only" data-run>Run cleanup now</button>
      </div>
      <div id="cleanup-result"></div>
    </form>
    <section><h2>History</h2><p class="muted" id="last-run">${lastRun}</p></section>`);

  const form = $('#policy');
  const errEl = $('#policy-err');
  const readPolicy = () => {
    const days = Number(form.days.value);
    const repos = $$('input[name="repo"]:checked', form).map((el) => el.value);
    if (!Number.isInteger(days) || days < 1 || days > 3650) return { error: 'Enter a whole number of days between 1 and 3650.' };
    if (!repos.length) return { error: 'Choose at least one repository.' };
    return { policy: { enabled: form.enabled.checked, unused_days: days, repos, include_metadata: form.meta.checked } };
  };
  const withPolicy = (fn) => async () => {
    const { policy, error } = readPolicy();
    errEl.textContent = error || '';
    errEl.classList.toggle('hidden', !error);
    if (!error) await fn(policy);
  };
  const save = (policy) => act(() => api('/api/cleanup', { method: 'PUT', body: policy }));

  form.addEventListener('submit', (e) => {
    e.preventDefault();
    withPolicy(async (policy) => {
      try {
        await save(policy);
        toast('Cleanup policy saved');
      } catch (err) {
        toast(err.message, 'error');
      }
    })();
  });

  const showResult = (r, done) => {
    const total = r.files + r.meta_files;
    const verb = done ? 'Removed' : 'Would remove';
    const head = total
      ? `<strong>${verb} ${esc(plural(r.files, 'file'))} (${esc(bytes(r.bytes))}) and ${esc(plural(r.meta_files, 'metadata entry', 'metadata entries'))} (${esc(bytes(r.meta_bytes))}).</strong>`
      : `<strong>${done ? 'Nothing needed removing.' : 'Nothing would be removed.'}</strong> Every cached package has been used in the last ${esc(plural(r.unused_days, 'day'))}.`;
    const items = !done && r.items.length
      ? `<div class="table-wrap"><table><thead><tr><th>Package</th><th>Version</th><th>Repository</th><th class="r">Size</th><th>Last used</th></tr></thead><tbody>${r.items
          .slice(0, 100)
          .map((i) => `<tr><td class="mono">${esc(i.package)}</td><td class="mono">${esc(i.version)}</td><td>${chip(i.repo)}</td><td class="r num">${esc(bytes(i.size))}</td><td>${timeTag(i.last_used)}</td></tr>`)
          .join('')}</tbody></table></div>${r.items.length > 100 ? `<p class="muted">Showing the 100 largest of ${esc(count(r.files))} files.</p>` : ''}`
      : '';
    const out = $('#cleanup-result');
    if (!out) return; // the user moved to another page meanwhile
    out.innerHTML = `<div class="result">${head}${items ? '<div class="panel">' + items + '</div>' : ''}</div>`;
  };

  $('[data-preview]', form).addEventListener(
    'click',
    withPolicy(async (policy) => {
      try {
        showResult(await api('/api/cleanup/preview', { method: 'POST', body: policy }), false);
      } catch (err) {
        toast(err.message, 'error');
      }
    })
  );

  $('[data-run]', form).addEventListener(
    'click',
    withPolicy(async (policy) => {
      const repos = policy.repos.map((r) => REPOS[r].label).join(', ');
      const ok = await confirmAction({
        title: 'Run cleanup now?',
        body: `This saves these rules, then deletes every ${repos} package nobody has downloaded in ${plural(policy.unused_days, 'day')}. Deleted packages are downloaded again if a client needs them.`,
        ok: 'Run cleanup',
      });
      if (!ok) return;
      try {
        await save(policy);
        const r = await act(() => api('/api/cleanup/run', { method: 'POST' }));
        showResult(r, true);
        const last = $('#last-run');
        if (last) last.innerHTML = `Last run ${timeTag(r.at)}: removed ${esc(plural(r.files, 'file'))} and ${esc(plural(r.meta_files, 'metadata entry', 'metadata entries'))}, freeing ${esc(bytes(r.bytes + r.meta_bytes))}.`;
        toast(`Cleanup removed ${plural(r.files, 'file')}, freed ${bytes(r.bytes + r.meta_bytes)}`);
      } catch (err) {
        toast(err.message, 'error');
      }
    })
  );
}

// ------------------------------------------------------------------- tasks

async function renderTasks({ isCurrent }) {
  const draw = async () => {
    const d = await api('/api/tasks');
    if (!isCurrent()) return;
    const rows = d.tasks
      .map((t) => {
        const result = t.running
          ? '<span class="status"><span class="dot warn"></span>Running</span>'
          : t.last_result
            ? `<span class="status"><span class="dot ${t.last_ok ? 'ok' : 'bad'}"></span>${esc(t.last_result)}</span>`
            : '<span class="muted">Not run yet</span>';
        const schedule = t.enabled ? esc(t.schedule) : '<span class="muted">Off</span>' + (t.id === 'cleanup' ? ' <a href="#/cleanup">Turn on</a>' : '');
        return `<tr>
          <td><strong>${esc(t.name)}</strong><div class="muted">${esc(t.description)}</div></td>
          <td class="nowrap">${schedule}</td>
          <td>${timeTag(t.last_run)}${t.last_duration_ms != null ? `<div class="muted num nowrap">took ${t.last_duration_ms < 1000 ? t.last_duration_ms + ' ms' : (t.last_duration_ms / 1000).toFixed(1) + ' s'}</div>` : ''}</td>
          <td>${result}</td>
          <td>${t.next_run ? timeTag(t.next_run) : '<span class="muted">-</span>'}</td>
          <td class="actions"><button class="small admin-only" data-run-task="${t.id}" data-name="${esc(t.name)}" ${t.running ? 'disabled' : ''}>Run now</button></td>
        </tr>`;
      })
      .join('');
    const tbody = $('#task-rows');
    if (tbody) {
      tbody.innerHTML = rows;
      return;
    }
    setView(`
      <div class="page-head"><div><h1>Tasks</h1><p>Background jobs Granary runs on a schedule. This page refreshes on its own.</p></div></div>
      <div class="panel table-wrap"><table>
        <thead><tr><th>Task</th><th>Schedule</th><th>Last run</th><th>Result</th><th>Next run</th><th class="r"><span class="sr-only">Actions</span></th></tr></thead>
        <tbody id="task-rows">${rows}</tbody>
      </table></div>`);
    view().addEventListener('click', async (e) => {
      const b = e.target.closest('[data-run-task]');
      if (!b) return;
      if (b.dataset.runTask === 'cleanup') {
        const ok = await confirmAction({
          title: 'Run cleanup now?',
          body: 'This deletes cached packages that match the saved cleanup policy.',
          ok: 'Run cleanup',
        });
        if (!ok) return;
      }
      try {
        b.disabled = true;
        await act(() => api(`/api/tasks/${b.dataset.runTask}/run`, { method: 'POST' }));
        toast(`Started ${b.dataset.name.toLowerCase()}`);
        setTimeout(() => draw().catch(() => {}), 600);
      } catch (err) {
        b.disabled = false;
        toast(err.message, 'error');
      }
    });
  };
  await draw();
  const timer = setInterval(() => draw().catch(() => {}), 4000);
  onLeave(() => clearInterval(timer));
}

// ------------------------------------------------------------------ system

async function renderSystem({ isCurrent }) {
  const d = await api('/api/system');
  if (!isCurrent()) return;
  const disk = d.disk;
  const usedPct = disk && disk.total ? ((disk.total - disk.free) / disk.total) * 100 : 0;
  const health = d.health || [];

  const host = location.host;
  const promSnippet = `scrape_configs:
  - job_name: granary
    scheme: ${location.protocol.replace(':', '')}
    authorization:
      credentials: YOUR_TOKEN
    static_configs:
      - targets: ['${host}']`;

  const upRows = REPO_IDS.map((id) => {
    const h = health.find((x) => x.repo === id);
    if (!h) return `<tr><td>${chip(id)}</td><td colspan="4" class="muted">Not checked yet</td></tr>`;
    const status = h.offline
      ? '<span class="status"><span class="dot"></span>Not contacted: offline mode</span>'
      : h.ok
      ? `<span class="status"><span class="dot ok"></span>Reachable${h.status ? ` (HTTP ${h.status})` : ''}</span>`
      : `<span class="status"><span class="dot bad"></span>Unreachable: ${esc(h.error || 'HTTP ' + h.status)}</span>`;
    return `<tr><td>${chip(id)}</td><td class="mono">${esc(h.url)}</td><td>${status}</td><td class="r num">${esc(h.latency_ms)} ms</td><td>${timeTag(h.checked_at)}</td></tr>`;
  }).join('');

  setView(`
    <div class="page-head">
      <div><h1>System</h1><p>Granary ${esc(d.version)}, running for ${esc(duration(d.uptime_secs))}.</p></div>
      <div class="page-actions admin-only"><button data-check>Check upstreams now</button></div>
    </div>
    <section>
      <h2>Upstream registries</h2>
      <div class="panel table-wrap"><table>
        <thead><tr><th>Repository</th><th>Checked URL</th><th>Status</th><th class="r">Latency</th><th>Checked</th></tr></thead>
        <tbody>${upRows}</tbody></table></div>
    </section>
    <section>
      <h2>Storage</h2>
      <dl class="panel facts">
        <div class="fact"><dt>Disk</dt><dd class="num">${disk ? `${esc(bytes(disk.total - disk.free))} used of ${esc(bytes(disk.total))}` : 'Unknown'}
          ${disk ? `<small>${esc(bytes(disk.free))} free</small><div class="meter ${usedPct > 95 ? 'full' : usedPct > 85 ? 'high' : ''}" role="img" aria-label="${Math.round(usedPct)}% used"><span data-pct="${usedPct.toFixed(1)}"></span></div>` : ''}</dd></div>
      </dl>
    </section>
    <section>
      <h2>Monitoring</h2>
      <p class="muted">Granary serves Prometheus metrics at <code>/metrics</code>: cache hits and misses, rejected downloads, storage per repository, disk space, upstream health and task results. Scrapers sign in with an access token.</p>
      <div class="snippet"><pre>${esc(promSnippet)}</pre><button class="small" type="button" data-copy="${esc(promSnippet)}">Copy</button></div>
    </section>
    <section>
      <h2>Configuration</h2>
      <p class="muted">Set with environment variables. Restart Granary after changing them.</p>
      <div class="panel table-wrap"><table>
        <thead><tr><th>Setting</th><th>Value</th><th>Environment variable</th></tr></thead>
        <tbody>${d.config.map((c) => `<tr><td>${esc(c.label)}</td><td class="mono">${esc(c.value)}</td><td><code>${esc(c.key)}</code></td></tr>`).join('')}</tbody>
      </table></div>
    </section>`);

  $$('.meter span', view()).forEach((el) => el.style.setProperty('--pct', el.dataset.pct + '%'));
  $('[data-check]', view()).addEventListener('click', async (e) => {
    const b = e.currentTarget;
    b.disabled = true;
    b.textContent = 'Checking…';
    try {
      const r = await act(() => api('/api/system/check', { method: 'POST' }));
      const up = r.health.filter((h) => h.ok).length;
      if (r.health.some((h) => h.offline)) toast('Offline mode is on, so upstreams were not contacted');
      else toast(`${up} of ${r.health.length} upstreams reachable`, up === r.health.length ? '' : 'error');
      route();
    } catch (err) {
      toast(err.message, 'error');
      b.disabled = false;
      b.textContent = 'Check upstreams now';
    }
  });
}

// -------------------------------------------------------------------- logs

async function renderLogs({ query, isCurrent }) {
  const level = query.get('level') || '';
  const levels = [['', 'All'], ['info', 'Info'], ['warn', 'Warnings'], ['error', 'Errors']];
  setView(`
    <div class="page-head"><div><h1>Logs</h1><p>The most recent server events, newest first. The full log goes to the container's standard output.</p></div></div>
    <div class="toolbar">
      <div class="seg" role="group" aria-label="Level">${levels
        .map(([v, l]) => `<button type="button" data-level="${v}" aria-pressed="${v === level}">${esc(l)}</button>`)
        .join('')}</div>
      <div class="grow"><label class="sr-only" for="log-filter">Filter log messages</label>
        <input id="log-filter" type="search" placeholder="Filter messages" autocomplete="off" value="${esc(query.get('q') || '')}"></div>
      <label class="check"><input type="checkbox" id="log-live" checked> Refresh automatically</label>
    </div>
    <div class="panel log" id="log" aria-live="off"></div>`);

  // Responses can arrive out of order (typing while the timer polls); only
  // the newest request may draw.
  let seq = 0;
  const draw = async () => {
    const mine = ++seq;
    const qs = new URLSearchParams();
    if (level) qs.set('level', level);
    const f = $('#log-filter')?.value.trim();
    if (f) qs.set('q', f);
    const d = await api('/api/logs?' + qs);
    if (!isCurrent() || mine !== seq) return;
    $('#log').innerHTML = d.entries.length
      ? d.entries
          .map((e) => {
            const t = new Date(e.ts);
            return `<div class="log-line"><span class="t" title="${esc(t.toLocaleString())}">${esc(t.toLocaleDateString(undefined, { month: 'short', day: 'numeric' }))} ${esc(t.toLocaleTimeString())}</span><span class="l ${esc(e.level)}">${esc(e.level)}</span><span class="m">${esc(e.message)}</span></div>`;
          })
          .join('')
      : '<div class="empty"><strong>No log entries match</strong>Try another level or filter.</div>';
  };
  await draw();

  $$('[data-level]', view()).forEach((b) =>
    b.addEventListener('click', () => {
      const qs = new URLSearchParams();
      if (b.dataset.level) qs.set('level', b.dataset.level);
      const f = $('#log-filter')?.value.trim();
      if (f) qs.set('q', f);
      go('/logs', qs, { replace: true });
    })
  );
  $('#log-filter').addEventListener('input', debounce(() => draw().catch(() => {}), 250));
  const timer = setInterval(() => {
    if ($('#log-live')?.checked) draw().catch(() => {});
  }, 3000);
  onLeave(() => clearInterval(timer));
}

// ------------------------------------------------------------------- audit

const AUDIT_ACTIONS = {
  'auth.login': 'Signed in',
  'auth.login_2fa': 'Two-factor code',
  'auth.logout': 'Signed out',
  'account.password_change': 'Changed password',
  'account.password_check': 'Password confirmation',
  'account.2fa_enable': 'Turned on two-factor',
  'account.2fa_disable': 'Turned off two-factor',
  'token.create': 'Created access token',
  'token.revoke': 'Revoked access token',
  'user.create': 'Added user',
  'user.role': 'Changed role',
  'user.disable': 'Disabled user',
  'user.enable': 'Enabled user',
  'user.delete': 'Deleted user',
  'user.reset_password': 'Reset password',
  'user.reset_2fa': 'Turned off two-factor for user',
  'package.delete': 'Deleted package',
  'file.delete': 'Deleted file',
  'repo.invalidate': 'Invalidated cache',
  'cleanup.policy': 'Changed cleanup rules',
  'task.run': 'Ran task',
  'audit.export': 'Exported audit log',
};

const AUDIT_FILTERS = [
  ['', 'All actions'],
  ['auth', 'Sign-ins'],
  ['account', 'Account changes'],
  ['token', 'Access tokens'],
  ['user', 'User management'],
  ['package', 'Package deletions'],
  ['file', 'File deletions'],
  ['repo', 'Cache invalidation'],
  ['cleanup', 'Cleanup rules'],
  ['task', 'Tasks'],
];

const AUDIT_PAGE = 100;

async function renderAudit({ query, isCurrent }) {
  const q = query.get('q') || '';
  const action = query.get('action') || '';
  const failed = query.get('failed') === '1';
  const page = Math.max(1, parseInt(query.get('page') || '1', 10) || 1);

  const apiQuery = new URLSearchParams({ limit: AUDIT_PAGE, offset: (page - 1) * AUDIT_PAGE });
  if (q) apiQuery.set('q', q);
  if (action) apiQuery.set('action', action);
  if (failed) apiQuery.set('failed', 'true');
  const d = await api('/api/audit?' + apiQuery);
  if (!isCurrent()) return;

  const exportQuery = new URLSearchParams(apiQuery);
  exportQuery.delete('limit');
  exportQuery.delete('offset');

  const rows = d.entries
    .map(
      (e) => `<tr>
        <td><time datetime="${esc(new Date(e.at * 1000).toISOString())}" title="${esc(ago(e.at))}">${esc(fullTime(e.at))}</time></td>
        <td><strong>${esc(e.actor)}</strong></td>
        <td>${e.ok ? '' : '<span class="status"><span class="dot bad"></span>Failed:</span> '}${esc(AUDIT_ACTIONS[e.action] || e.action)}</td>
        <td class="mono">${esc(e.target)}</td>
        <td class="mono hide-sm">${esc(e.ip)}</td>
      </tr>`
    )
    .join('');

  const pages = Math.max(1, Math.ceil(d.total / AUDIT_PAGE));
  const from = d.total ? d.offset + 1 : 0;
  const to = d.offset + d.entries.length;
  const pager = d.total > AUDIT_PAGE
    ? `<div class="pager"><span>${esc(count(from))} to ${esc(count(to))} of ${esc(count(d.total))}</span>
        <span class="page-actions">
          <button class="small" data-page="${page - 1}" ${page <= 1 ? 'disabled' : ''}>Previous</button>
          <button class="small" data-page="${page + 1}" ${page >= pages ? 'disabled' : ''}>Next</button>
        </span></div>`
    : '';

  setView(`
    <div class="page-head">
      <div><h1>Audit log</h1>
      <p>Who signed in and what they changed. Entries are kept for ${esc(plural(d.retention_days, 'day'))} (<code>GRANARY_AUDIT_DAYS</code>) and survive restarts.</p></div>
      <div class="page-actions"><a class="btn" href="/api/audit.csv?${esc(exportQuery.toString())}" download>Export CSV</a></div>
    </div>
    <form class="toolbar" id="audit-filter" role="search">
      <div class="grow"><label class="sr-only" for="audit-q">Search user, target or address</label>
        <input id="audit-q" type="search" placeholder="Search user, target or address" value="${esc(q)}" autocomplete="off"></div>
      <label class="sr-only" for="audit-action">Action</label>
      <select id="audit-action">${AUDIT_FILTERS.map(([v, l]) => `<option value="${esc(v)}" ${v === action ? 'selected' : ''}>${esc(l)}</option>`).join('')}</select>
      <label class="check"><input type="checkbox" id="audit-failed" ${failed ? 'checked' : ''}> Failures only</label>
    </form>
    <div class="panel">
      ${d.entries.length
        ? `<div class="table-wrap"><table>
            <thead><tr><th>Time</th><th>User</th><th>Action</th><th>Target</th><th class="hide-sm">Address</th></tr></thead>
            <tbody>${rows}</tbody></table></div>${pager}`
        : '<div class="empty"><strong>No audit entries match</strong>Try a different search or action.</div>'}
    </div>`);

  const update = (changes) => {
    const next = new URLSearchParams(query);
    for (const [k, v] of Object.entries(changes)) {
      if (v === '' || v == null || v === false) next.delete(k);
      else next.set(k, v);
    }
    go('/audit', next, { replace: true });
  };
  $('#audit-filter').addEventListener('submit', (e) => {
    e.preventDefault();
    update({ q: $('#audit-q').value.trim(), page: '' });
  });
  $('#audit-q').addEventListener(
    'search',
    () => !$('#audit-q').value && update({ q: '', page: '' })
  );
  $('#audit-action').addEventListener('change', (e) => update({ action: e.target.value, page: '' }));
  $('#audit-failed').addEventListener('change', (e) => update({ failed: e.target.checked ? '1' : '', page: '' }));
  view().addEventListener('click', (e) => {
    const p = e.target.closest('[data-page]');
    if (p && !p.disabled) update({ page: p.dataset.page });
  });
}

// ----------------------------------------------------------------- account

async function renderAccount({ isCurrent }) {
  me = await api('/api/auth/me');
  if (!isCurrent()) return;
  const u = me.user;
  setView(`
    <div class="page-head"><div><h1>Account</h1><p>Signed in as <strong>${esc(u.username)}</strong>, ${u.role === 'admin' ? 'an admin' : 'a viewer'}.</p></div></div>
    <section>
      <h2>Password</h2>
      <form class="panel panel-pad narrow stack" id="acct-pw" novalidate>
        ${passwordFields('ap', 'Current password')}
        <p class="err hidden" id="ap-err" role="alert"></p>
        <div><button class="primary" type="submit">Change password</button></div>
      </form>
    </section>
    <section>
      <h2>Two-factor authentication</h2>
      <div class="panel panel-pad narrow" id="tfa"></div>
    </section>`);

  bindPasswordForm($('#acct-pw'), 'ap', () => toast('Password changed. Other devices were signed out.'));

  const tfa = $('#tfa');
  const drawOff = () => {
    tfa.innerHTML = `
      <p class="status"><span class="dot"></span>Off</p>
      <p class="muted">Ask for a code from an authenticator app (such as Aegis, 2FAS or Bitwarden) whenever you sign in.</p>
      <button data-tfa-setup>Set up two-factor</button>`;
  };
  const drawOn = () => {
    tfa.innerHTML = `
      <p class="status"><span class="dot ok"></span>On</p>
      <p class="muted">You enter a code from your authenticator app each time you sign in.</p>
      <form class="stack" id="tfa-off" novalidate>
        <div class="field"><label for="tfa-pw">Password</label><input id="tfa-pw" type="password" autocomplete="current-password"></div>
        <p class="err hidden" id="tfa-off-err" role="alert"></p>
        <div><button class="danger" type="submit">Turn off two-factor</button></div>
      </form>`;
    $('#tfa-off').addEventListener('submit', async (e) => {
      e.preventDefault();
      try {
        await api('/api/account/2fa/disable', { method: 'POST', body: { password: $('#tfa-pw').value } });
        toast('Two-factor authentication turned off');
        me.user.totp_enabled = false;
        drawOff();
      } catch (ex) {
        showError($('#tfa-off-err'), ex.message);
      }
    });
  };
  (u.totp_enabled ? drawOn : drawOff)();

  tfa.addEventListener('click', async (e) => {
    if (!e.target.closest('[data-tfa-setup]')) return;
    try {
      const r = await api('/api/account/2fa/setup', { method: 'POST' });
      tfa.innerHTML = `
        <p>Scan this code with your authenticator app, then enter the 6-digit code it shows.</p>
        <div class="tfa-setup">
          <img class="qr" src="${esc(safeUrl(r.qr))}" alt="QR code for your authenticator app">
          <div class="stack">
            <div class="field"><span class="label">Or enter this key by hand</span>${reveal(r.secret)}</div>
            <form class="stack" id="tfa-on" novalidate>
              <div class="field"><label for="tfa-code">Code from the app</label>
                <input id="tfa-code" type="text" inputmode="numeric" autocomplete="one-time-code" maxlength="9"></div>
              <p class="err hidden" id="tfa-on-err" role="alert"></p>
              <div><button class="primary" type="submit">Turn on two-factor</button></div>
            </form>
          </div>
        </div>`;
      $('#tfa-code').focus();
      $('#tfa-on').addEventListener('submit', async (ev) => {
        ev.preventDefault();
        try {
          await api('/api/account/2fa/enable', { method: 'POST', body: { code: $('#tfa-code').value } });
          toast('Two-factor authentication is on');
          me.user.totp_enabled = true;
          drawOn();
        } catch (ex) {
          showError($('#tfa-on-err'), ex.message);
        }
      });
    } catch (ex) {
      toast(ex.message, 'error');
    }
  });
}

// ------------------------------------------------------------ access tokens

const EXPIRY_OPTIONS = [
  ['90', '90 days'],
  ['30', '30 days'],
  ['365', '1 year'],
  ['0', 'Never'],
];

function expiryText(t) {
  if (!t.expires_at) return '<span class="muted">Never</span>';
  if (t.expires_at * 1000 < Date.now()) return '<span class="status"><span class="dot bad"></span>Expired</span>';
  return timeTag(t.expires_at);
}

async function renderTokens({ query, isCurrent }) {
  const all = isAdmin() && query.get('all') === '1';
  const [d, repos] = await Promise.all([api('/api/tokens' + (all ? '?all=true' : '')), api('/api/repos')]);
  if (!isCurrent()) return;

  const tokenRows = (tokens) => tokens
    .map(
      (t) => `<tr>
        <td><strong>${esc(t.name)}</strong>${t.system ? ' <span class="kind" title="Created by Granary for the warmer. It can only download packages, and is made again if revoked.">System</span>' : ''}</td>
        ${all ? `<td>${esc(t.system ? 'Granary' : t.username)}</td>` : ''}
        <td class="mono">${esc(t.hint)}</td>
        <td>${timeTag(t.created_at)}</td>
        <td>${timeTag(t.last_used)}</td>
        <td>${expiryText(t)}</td>
        <td class="actions"><button class="small danger" data-revoke="${t.id}" data-name="${esc(t.name)}">Revoke</button></td>
      </tr>`
    )
    .join('');
  const tokenList = (tokens) => tokens.length
    ? `<div class="table-wrap"><table><thead><tr><th>Name</th>${all ? '<th>Owner</th>' : ''}<th>Token</th><th>Created</th><th>Last used</th><th>Expires</th><th class="r"><span class="sr-only">Actions</span></th></tr></thead><tbody>${tokenRows(tokens)}</tbody></table></div>`
    : '<div class="empty"><strong>No tokens yet</strong>Create one above for each machine that installs packages.</div>';

  setView(`
    <div class="page-head">
      <div><h1>Access tokens</h1>
      <p>Cargo, npm, pip, Maven and Gradle use a token to download through Granary. Make one per machine so you can revoke it on its own.</p></div>
      ${isAdmin() ? `<div class="page-actions"><nav class="seg" aria-label="Whose tokens">
        <a href="#/tokens" aria-current="${!all}">Mine</a><a href="#/tokens?all=1" aria-current="${all}">Everyone's</a></nav></div>` : ''}
    </div>
    ${me.client_auth ? '' : `<div class="banner"><span class="dot warn"></span><span>Package downloads don't need a token right now, because <code>GRANARY_CLIENT_AUTH</code> is off.</span></div>`}
    <form class="panel panel-pad" id="new-token" novalidate>
      <div class="inline-form">
        <div class="field grow"><label for="tk-name">Name</label><input id="tk-name" type="text" maxlength="80" placeholder="e.g. build server, work laptop"></div>
        <div class="field"><label for="tk-exp">Expires</label><select id="tk-exp">${EXPIRY_OPTIONS.map(([v, l]) => `<option value="${v}">${l}</option>`).join('')}</select></div>
        <div class="field"><span class="label">&nbsp;</span><button class="primary" type="submit">Create token</button></div>
      </div>
      <p class="err hidden" id="tk-err" role="alert"></p>
      <div id="tk-result"></div>
    </form>
    <section>
      <h2>${all ? 'All tokens' : 'Your tokens'}</h2>
      <div class="panel" id="tk-list">${tokenList(d.tokens)}</div>
    </section>`);

  $('#new-token').addEventListener('submit', async (e) => {
    e.preventDefault();
    const name = $('#tk-name').value.trim();
    if (!name) return showError($('#tk-err'), 'Give the token a name so you can recognise it later.');
    try {
      const r = await api('/api/tokens', { method: 'POST', body: { name, expires_days: Number($('#tk-exp').value) } });
      showError($('#tk-err'), '');
      const snippets = repos.repos
        .flatMap((repo) =>
          setupSnippets(repo.setup).map((s) => {
            const snippet = s.snippet.replaceAll('YOUR_TOKEN', () => r.token);
            const after = s.after ? `<p class="snippet-file">${esc(s.after)}</p>` : '';
            const label = s.label ? ` <span class="muted">${esc(s.label)}</span>` : '';
            return `<div class="setup-block"><div class="snippet-file">${chip(repo.id)}${label} Add to ${esc(s.file)}</div>
            <div class="snippet"><pre>${esc(snippet)}</pre><button class="small" type="button" data-copy="${esc(snippet)}">Copy</button></div>${after}</div>`;
          })
        )
        .join('');
      $('#tk-result').innerHTML = `
        <div class="result">
          <strong>Copy this token now. Granary only shows it once.</strong>
          ${reveal(r.token)}
          <details class="setup" open><summary>Client setup with this token</summary>${snippets}</details>
        </div>`;
      $('#tk-name').value = '';
      const fresh = await api('/api/tokens' + (all ? '?all=true' : ''));
      if (isCurrent()) $('#tk-list').innerHTML = tokenList(fresh.tokens);
    } catch (ex) {
      if (isCurrent()) showError($('#tk-err'), ex.message);
    }
  });

  view().addEventListener('click', async (e) => {
    const b = e.target.closest('[data-revoke]');
    if (!b) return;
    const ok = await confirmAction({
      title: `Revoke ${b.dataset.name}?`,
      body: 'Anything using this token stops being able to download packages right away.',
      ok: 'Revoke token',
    });
    if (!ok) return;
    try {
      await api(`/api/tokens/${b.dataset.revoke}`, { method: 'DELETE' });
      toast(`Revoked ${b.dataset.name}`);
      route();
    } catch (ex) {
      toast(ex.message, 'error');
    }
  });
}

// -------------------------------------------------------------------- users

async function renderUsers({ isCurrent }) {
  const d = await api('/api/users');
  if (!isCurrent()) return;
  const rows = d.users
    .map((u) => {
      const self = u.id === me.user.id;
      return `<tr>
        <td><strong>${esc(u.username)}</strong>${self ? ' <span class="muted">(you)</span>' : ''}</td>
        <td><label class="sr-only" for="role-${u.id}">Role for ${esc(u.username)}</label>
          <select id="role-${u.id}" data-role-for="${u.id}" data-name="${esc(u.username)}">
            <option value="admin" ${u.role === 'admin' ? 'selected' : ''}>Admin</option>
            <option value="viewer" ${u.role === 'viewer' ? 'selected' : ''}>Viewer</option>
          </select></td>
        <td>${u.totp_enabled ? '<span class="status"><span class="dot ok"></span>On</span>' : '<span class="muted">Off</span>'}</td>
        <td>${timeTag(u.last_login)}</td>
        <td class="r num">${esc(count(u.tokens))}</td>
        <td>${u.disabled ? '<span class="status"><span class="dot bad"></span>Disabled</span>' : u.must_change_password ? '<span class="status"><span class="dot warn"></span>Temporary password</span>' : '<span class="status"><span class="dot ok"></span>Active</span>'}</td>
        <td class="actions">
          <button class="small" data-user-action="reset-password" data-id="${u.id}" data-name="${esc(u.username)}">Reset password</button>
          ${u.totp_enabled ? `<button class="small" data-user-action="reset-2fa" data-id="${u.id}" data-name="${esc(u.username)}">Turn off 2FA</button>` : ''}
          ${self ? '' : `<button class="small" data-user-action="${u.disabled ? 'enable' : 'disable'}" data-id="${u.id}" data-name="${esc(u.username)}">${u.disabled ? 'Enable' : 'Disable'}</button>
          <button class="small danger" data-user-action="delete" data-id="${u.id}" data-name="${esc(u.username)}">Delete</button>`}
        </td>
      </tr>`;
    })
    .join('');

  setView(`
    <div class="page-head"><div><h1>Users</h1><p>Admins can delete packages, change settings and manage users. Viewers can browse and make their own access tokens.</p></div></div>
    <form class="panel panel-pad" id="new-user" novalidate>
      <div class="inline-form">
        <div class="field grow"><label for="nu-name">Username</label><input id="nu-name" type="text" autocapitalize="off" spellcheck="false" maxlength="64"></div>
        <div class="field"><label for="nu-role">Role</label><select id="nu-role"><option value="viewer">Viewer</option><option value="admin">Admin</option></select></div>
        <div class="field"><span class="label">&nbsp;</span><button class="primary" type="submit">Add user</button></div>
      </div>
      <p class="err hidden" id="nu-err" role="alert"></p>
      <div id="nu-result"></div>
    </form>
    <section>
      <div class="panel table-wrap"><table>
        <thead><tr><th>User</th><th>Role</th><th>Two-factor</th><th>Last sign-in</th><th class="r">Tokens</th><th>Status</th><th class="r"><span class="sr-only">Actions</span></th></tr></thead>
        <tbody>${rows}</tbody>
      </table></div>
    </section>`);

  const showTemp = (name, password) => {
    $('#nu-result').innerHTML = `<div class="result"><strong>Give ${esc(name)} this temporary password.</strong>
      <p class="muted">They'll choose their own the first time they sign in. Granary only shows it once.</p>${reveal(password)}</div>`;
    $('#nu-result').scrollIntoView({ block: 'nearest' });
  };

  $('#new-user').addEventListener('submit', async (e) => {
    e.preventDefault();
    const username = $('#nu-name').value.trim();
    if (!username) return showError($('#nu-err'), 'Enter a username.');
    try {
      const r = await api('/api/users', { method: 'POST', body: { username, role: $('#nu-role').value } });
      showError($('#nu-err'), '');
      await renderUsers({ isCurrent });
      showTemp(r.username, r.password);
    } catch (ex) {
      showError($('#nu-err'), ex.message);
    }
  });

  view().addEventListener('change', async (e) => {
    const sel = e.target.closest('[data-role-for]');
    if (!sel) return;
    try {
      await api(`/api/users/${sel.dataset.roleFor}`, { method: 'PATCH', body: { role: sel.value } });
      toast(`${sel.dataset.name} is now ${sel.value === 'admin' ? 'an admin' : 'a viewer'}`);
      if (Number(sel.dataset.roleFor) === me.user.id) {
        me = await api('/api/auth/me');
        showShell();
        if (!isAdmin()) return go('/');
      }
      route();
    } catch (ex) {
      toast(ex.message, 'error');
      route();
    }
  });

  view().addEventListener('click', async (e) => {
    const b = e.target.closest('[data-user-action]');
    if (!b) return;
    const { id, name } = b.dataset;
    const action = b.dataset.userAction;
    const prompts = {
      'reset-password': [`Reset ${name}'s password?`, 'Their current password stops working and Granary shows you a temporary one to give them.', 'Reset password'],
      'reset-2fa': [`Turn off two-factor for ${name}?`, 'Use this if they lost their authenticator app. They can set it up again from their account page.', 'Turn off 2FA'],
      disable: [`Disable ${name}?`, 'They are signed out and their access tokens stop working until you enable the account again.', 'Disable account'],
      delete: [`Delete ${name}?`, 'This removes the account and all of its access tokens. It can’t be undone.', 'Delete user'],
    };
    if (prompts[action]) {
      const [title, body, ok] = prompts[action];
      if (!(await confirmAction({ title, body, ok }))) return;
    }
    try {
      if (action === 'reset-password') {
        const r = await api(`/api/users/${id}/reset-password`, { method: 'POST' });
        showTemp(r.username, r.password);
        return;
      }
      if (action === 'reset-2fa') await api(`/api/users/${id}/reset-2fa`, { method: 'POST' });
      if (action === 'disable' || action === 'enable') await api(`/api/users/${id}`, { method: 'PATCH', body: { disabled: action === 'disable' } });
      if (action === 'delete') await api(`/api/users/${id}`, { method: 'DELETE' });
      const done = { 'reset-2fa': 'Turned off two-factor for', disable: 'Disabled', enable: 'Enabled', delete: 'Deleted' };
      toast(`${done[action]} ${name}`);
      route();
    } catch (ex) {
      toast(ex.message, 'error');
    }
  });
}

// -------------------------------------------------------------------- boot

window.addEventListener('hashchange', () => {
  if (me && !me.user.must_change_password) route();
});

document.addEventListener('click', (e) => {
  if (e.target.closest('[data-signout]')) signOut();
  const copy = e.target.closest('[data-copy]');
  if (copy) copyText(copy.dataset.copy, copy);
});

document.addEventListener('DOMContentLoaded', async () => {
  $('#search-form').addEventListener('submit', (e) => {
    e.preventDefault();
    const q = $('#global-search').value.trim();
    const qs = new URLSearchParams();
    if (q) qs.set('q', q);
    location.hash = '#/browse' + (q ? '?' + qs : '');
  });

  document.addEventListener('keydown', (e) => {
    const typing = /^(input|textarea|select)$/i.test(document.activeElement?.tagName || '');
    if (e.key === '/' && me && !typing && !e.metaKey && !e.ctrlKey && !$('dialog[open]')) {
      e.preventDefault();
      $('#global-search').focus();
    }
  });

  try {
    me = await api('/api/auth/me');
    enter();
  } catch (e) {
    if (e.status === 401) showSignIn();
    else {
      $('#auth').classList.remove('hidden');
      $('#auth').innerHTML = authCard(`<h1>Granary is unavailable</h1><p class="auth-note">${esc(e.message)}</p>`);
    }
  }
});

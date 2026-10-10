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

const chip = (repo) => `<span class="fmt ${esc(repo)}">${esc(REPOS[repo]?.label ?? repo)}</span>`;

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
  [/^\/browse(?:\/(cargo|npm|pypi|maven))?$/, 'browse', renderBrowse],
  [/^\/c\/(cargo|npm|pypi|maven)\/(.+)$/, 'browse', renderComponent],
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
  const packages = d.repos.reduce((s, r) => s + r.packages, 0);
  const hits = d.stats.file_hits;
  const served = hits + d.stats.file_downloads;
  const hitRate = served ? Math.round((hits / served) * 100) : null;
  const disk = d.disk;
  const used = disk ? disk.total - disk.free : 0;
  const usedPct = disk && disk.total ? (used / disk.total) * 100 : 0;
  const health = Object.fromEntries((d.health || []).map((h) => [h.repo, h]));

  const silos = d.repos
    .map((r) => {
      const share = total ? (r.size / total) * 100 : 0;
      const fill = r.size ? Math.max(4, share) : 0;
      return `
      <a class="silo-col ${esc(r.repo)}" href="#/browse/${esc(r.repo)}" aria-label="${esc(REPOS[r.repo].label)}: ${esc(bytes(r.size))}, ${esc(plural(r.packages, 'package'))}">
        <div class="silo"><div class="grain" data-fill="${fill.toFixed(1)}"></div></div>
        <div class="silo-label">
          ${chip(r.repo)}
          <span class="silo-size num">${esc(bytes(r.size))}</span>
          <span class="silo-meta">${esc(plural(r.packages, 'package'))}</span>
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
      <div><h1>Overview</h1><p>${esc(plural(packages, 'package'))} cached, using ${esc(bytes(total))}.</p></div>
    </div>
    ${offlineBanner(d.health)}
    <section class="granary">
      <div class="panel">
        <div class="silos">${silos}</div>
        <div class="upstreams">${upstreams}</div>
      </div>
      <dl class="panel facts">
        <div class="fact"><dt>Served from cache</dt><dd class="num">${hitRate == null ? '-' : hitRate + '%'}<small>${esc(count(hits))} of ${esc(count(served))} file requests since start</small></dd></div>
        <div class="fact"><dt>Fetched from upstream</dt><dd class="num">${esc(count(d.stats.file_downloads))}<small>new files since start</small></dd></div>
        <div class="fact"><dt>Metadata refreshes</dt><dd class="num">${esc(count(d.stats.meta_refreshes))}<small>${d.stats.meta_stale_served ? esc(count(d.stats.meta_stale_served)) + ' served stale while upstream was down' : 'none served stale'}</small></dd></div>
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

  const linkFor = (r) => {
    const qs = new URLSearchParams();
    if (q) qs.set('q', q);
    return '#/browse' + (r ? '/' + r : '') + (q ? '?' + qs : '');
  };
  const tabs = [['', 'All'], ...REPO_IDS.map((id) => [id, REPOS[id].label])]
    .map(([id, label]) => `<a href="${linkFor(id)}" aria-current="${id === repo}">${esc(label)}</a>`)
    .join('');

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
        <td><strong>${esc(t.name)}</strong></td>
        ${all ? `<td>${esc(t.username)}</td>` : ''}
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

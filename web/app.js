// Granary web UI. Plain JavaScript, no build step, no third-party code.
'use strict';

// ------------------------------------------------------------------ helpers

const $ = (sel, el = document) => el.querySelector(sel);
const $$ = (sel, el = document) => Array.from(el.querySelectorAll(sel));

const esc = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

const REPOS = {
  cargo: { label: 'Cargo', upstream: 'crates.io' },
  npm: { label: 'npm', upstream: 'registry.npmjs.org' },
  pypi: { label: 'PyPI', upstream: 'pypi.org' },
};
const REPO_IDS = Object.keys(REPOS);

const chip = (repo) => `<span class="fmt ${esc(repo)}">${esc(REPOS[repo]?.label ?? repo)}</span>`;

function bytes(n) {
  if (n == null) return '–';
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

// ------------------------------------------------------------ token & api

const tokenStore = {
  get() {
    try {
      return localStorage.getItem('granary.token') || '';
    } catch {
      return '';
    }
  },
  set(v) {
    try {
      if (v) localStorage.setItem('granary.token', v);
      else localStorage.removeItem('granary.token');
    } catch {
      /* storage unavailable: token lasts for this page only */
    }
    memoryToken = v;
  },
};
let memoryToken = tokenStore.get();

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function api(path, { method = 'GET', body } = {}) {
  const headers = { Accept: 'application/json' };
  if (method !== 'GET') headers['X-Granary-Request'] = '1';
  if (memoryToken) headers.Authorization = 'Bearer ' + memoryToken;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  let res;
  try {
    res = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  } catch {
    throw new ApiError(0, 'Granary is not responding. Check that the server is running.');
  }
  if (!res.ok) {
    const text = (await res.text()).trim();
    throw new ApiError(res.status, text ? text[0].toUpperCase() + text.slice(1) + '.' : `Request failed (HTTP ${res.status}).`);
  }
  return res.status === 204 ? null : res.json();
}

/** Run a change; if the server wants a token, ask for it once and retry. */
async function act(fn) {
  try {
    return await fn();
  } catch (e) {
    if (e.status === 401 && (await askToken())) return await fn();
    throw e;
  }
}

let auth = { required: false, valid: true };

async function refreshAuth() {
  try {
    auth = await api('/api/auth');
  } catch {
    /* leave as is */
  }
  renderAccess();
}

function renderAccess() {
  const el = $('#access');
  if (!auth.required) {
    el.innerHTML = '<span class="dot warn"></span><span>No admin token set</span>';
    el.title = 'Anyone who can reach Granary can delete cached packages. Set GRANARY_ADMIN_TOKEN to require a token.';
  } else if (auth.valid && memoryToken) {
    el.innerHTML = '<span class="dot ok"></span><span>Admin unlocked</span> <button class="link" data-access="lock">Lock</button>';
    el.title = '';
  } else {
    el.innerHTML = '<span class="dot"></span><span>Read-only</span> <button class="link" data-access="unlock">Unlock</button>';
    el.title = 'Enter the admin token to delete packages or run tasks.';
  }
}

function askToken() {
  const dlg = $('#token-dialog');
  const input = $('#token-input');
  const err = $('#token-err');
  input.value = '';
  err.classList.add('hidden');
  dlg.showModal();
  input.focus();
  return new Promise((resolve) => {
    const form = $('#token-form');
    const onSubmit = async (e) => {
      if (e.submitter && e.submitter.value !== 'ok') return;
      e.preventDefault();
      const candidate = input.value.trim();
      if (!candidate) return;
      const prev = memoryToken;
      memoryToken = candidate;
      let ok = false;
      try {
        ok = (await api('/api/auth')).valid;
      } catch {
        ok = false;
      }
      if (ok) {
        tokenStore.set(candidate);
        auth = { required: true, valid: true };
        renderAccess();
        dlg.close('ok');
      } else {
        memoryToken = prev;
        err.classList.remove('hidden');
        input.select();
      }
    };
    const onClose = () => {
      form.removeEventListener('submit', onSubmit);
      resolve(dlg.returnValue === 'ok');
    };
    form.addEventListener('submit', onSubmit);
    dlg.addEventListener('close', onClose, { once: true });
  });
}

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
  [/^\/browse(?:\/(cargo|npm|pypi))?$/, 'browse', renderBrowse],
  [/^\/c\/(cargo|npm|pypi)\/(.+)$/, 'browse', renderComponent],
  [/^\/cleanup$/, 'cleanup', renderCleanup],
  [/^\/tasks$/, 'tasks', renderTasks],
  [/^\/system$/, 'system', renderSystem],
  [/^\/logs$/, 'logs', renderLogs],
];

async function route() {
  cleanups.forEach((fn) => fn());
  cleanups = [];
  const token = ++navToken;
  const { path, query } = parseHash();
  const match = routes.find(([re]) => re.test(path));
  const [re, nav, render] = match || [null, '', renderNotFound];
  const params = re ? path.match(re).slice(1).map((p) => (p ? decodeURIComponent(p) : p)) : [];
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
      <a class="silo-col" href="#/browse/${r.repo}" aria-label="${esc(REPOS[r.repo].label)}: ${esc(bytes(r.size))}, ${esc(plural(r.packages, 'package'))}">
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
    const state = !h ? ['', 'not checked yet'] : h.ok ? ['ok', `reachable, ${h.latency_ms} ms`] : ['bad', `unreachable (${h.error || 'HTTP ' + h.status})`];
    return `<span class="status"><span class="dot ${state[0]}"></span>${esc(REPOS[id].upstream)} <span class="muted">${esc(state[1])}</span></span>`;
  }).join('');

  const recent = d.recent.length
    ? `<div class="panel table-wrap"><table>
        <thead><tr><th>Package</th><th>Version</th><th>Repository</th><th class="r">Size</th><th>Cached</th></tr></thead>
        <tbody>${d.recent
          .map(
            (a) => `<tr>
              <td><a class="pkg mono" href="#/c/${a.repo}/${encodeURIComponent(a.package)}">${esc(a.package)}</a></td>
              <td class="mono">${esc(a.version)}</td>
              <td>${chip(a.repo)}</td>
              <td class="r num">${esc(bytes(a.size))}</td>
              <td>${timeTag(a.cached_at)}</td></tr>`
          )
          .join('')}</tbody></table></div>`
    : `<div class="panel empty"><strong>Nothing cached yet</strong>
        Point Cargo, npm or pip at Granary and packages will appear here as they are downloaded.
        <p><a class="btn" href="#/repos">Show client setup</a></p></div>`;

  setView(`
    <div class="page-head">
      <div><h1>Overview</h1><p>${esc(plural(packages, 'package'))} cached, using ${esc(bytes(total))}.</p></div>
    </div>
    <section class="granary">
      <div class="panel">
        <div class="silos">${silos}</div>
        <div class="upstreams">${upstreams}</div>
      </div>
      <dl class="panel facts">
        <div class="fact"><dt>Served from cache</dt><dd class="num">${hitRate == null ? '–' : hitRate + '%'}<small>${esc(count(hits))} of ${esc(count(served))} file requests since start</small></dd></div>
        <div class="fact"><dt>Fetched from upstream</dt><dd class="num">${esc(count(d.stats.file_downloads))}<small>new files since start</small></dd></div>
        <div class="fact"><dt>Metadata refreshes</dt><dd class="num">${esc(count(d.stats.meta_refreshes))}<small>${d.stats.meta_stale_served ? esc(count(d.stats.meta_stale_served)) + ' served stale while upstream was down' : 'none served stale'}</small></dd></div>
        <div class="fact"><dt>Disk</dt><dd class="num">${disk ? esc(bytes(disk.free)) + ' free' : '–'}<small>${disk ? 'of ' + esc(bytes(disk.total)) : 'unknown'}</small>
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
            <button data-invalidate="${r.id}" data-name="${esc(r.name)}">Invalidate cache</button>
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
          <div class="snippet"><pre>${esc(r.setup.snippet)}</pre><button class="small" data-copy="${esc(r.setup.snippet)}">Copy</button></div>
          <div class="snippet-file">Add to ${esc(r.setup.file)}</div>
        </details>
      </article>`;
    })
    .join('');

  setView(`
    <div class="page-head">
      <div><h1>Repositories</h1><p>Each repository caches one public registry. Point your package manager at its URL.</p></div>
    </div>
    <div class="panel">${blocks}</div>`);

  view().addEventListener('click', async (e) => {
    const copy = e.target.closest('[data-copy]');
    if (copy) return copyText(copy.dataset.copy, copy);
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
  const page = Math.max(1, parseInt(query.get('page') || '1', 10));

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

  const rows = d.items
    .map(
      (c) => `<tr>
        <td><a class="pkg mono" href="#/c/${c.repo}/${encodeURIComponent(c.name)}">${esc(c.name)}</a></td>
        ${repo ? '' : `<td>${chip(c.repo)}</td>`}
        <td class="r num">${esc(count(c.versions))}</td>
        <td class="r num">${esc(bytes(c.size))}</td>
        <td class="hide-sm">${timeTag(c.latest_cached)}</td>
        <td>${timeTag(c.last_access)}</td>
      </tr>`
    )
    .join('');

  const from = d.total ? d.offset + 1 : 0;
  const to = d.offset + d.items.length;
  const pages = Math.max(1, Math.ceil(d.total / PAGE_SIZE));
  const pager = d.total > PAGE_SIZE
    ? `<div class="pager"><span>${esc(count(from))}–${esc(count(to))} of ${esc(count(d.total))}</span>
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
            <thead><tr>${col('name', 'Package')}${repo ? '' : '<th>Repository</th>'}${col('versions', 'Versions', 'r')}${col('size', 'Size', 'r')}${col('cached', 'Cached', 'hide-sm')}${col('used', 'Last downloaded')}</tr></thead>
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
    const p = e.target.closest('[data-page]');
    if (p && !p.disabled) update({ page: p.dataset.page });
  });
}

// --------------------------------------------------------------- component

async function renderComponent({ params, isCurrent }) {
  const [repo, name] = params;
  let d;
  try {
    d = await api(`/api/component?repo=${repo}&name=${encodeURIComponent(name)}`);
  } catch (e) {
    if (e.status !== 404) throw e;
    if (!isCurrent()) return;
    setView(`<div class="crumbs"><a href="#/browse/${repo}">Browse ${esc(REPOS[repo].label)}</a></div>
      <div class="page-head"><div><h1 class="title-mono">${esc(name)}</h1><p>This package is not cached. It will be downloaded the next time a client asks for it.</p></div></div>`);
    return;
  }
  if (!isCurrent()) return;

  const rows = d.versions
    .map((v, i) => {
      const single = v.files.length === 1 ? v.files[0] : null;
      const actions = single
        ? `<a class="btn small" href="${esc(single.download)}" download>Download</a>
           <button class="small danger" data-delete-file="${esc(single.path)}" data-label="${esc(single.file_name)}">Delete</button>`
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
                <td class="actions"><a class="btn small" href="${esc(f.download)}" download>Download</a>
                  <button class="small danger" data-delete-file="${esc(f.path)}" data-label="${esc(f.file_name)}">Delete</button></td>
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
    <div class="crumbs"><a href="#/browse/${repo}">Browse ${esc(REPOS[repo].label)}</a></div>
    <div class="page-head">
      <div>
        <h1 class="title-mono">${esc(d.name)}</h1>
        <div class="summary-line">
          ${chip(repo)}
          <span><b class="num">${esc(plural(d.versions.length, 'version'))}</b> cached</span>
          <span><b class="num">${esc(bytes(d.size))}</b> on disk</span>
          ${d.upstream ? `<a href="${esc(d.upstream.url)}" target="_blank" rel="noopener noreferrer">View on ${esc(d.upstream.label)}</a>` : ''}
        </div>
      </div>
      <div class="page-actions"><button class="danger" data-delete-package>Delete package</button></div>
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
    ${auth.required ? '' : `<div class="banner"><span class="dot warn"></span><span>Anyone who can reach this page can delete cached packages. Set <code>GRANARY_ADMIN_TOKEN</code> to require a token.</span></div>`}
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
        <button class="primary" type="submit">Save policy</button>
        <button type="button" data-preview>Preview</button>
        <button type="button" class="danger" data-run>Run cleanup now</button>
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
    $('#cleanup-result').innerHTML = `<div class="result">${head}${items ? '<div class="panel">' + items + '</div>' : ''}</div>`;
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
        $('#last-run').innerHTML = `Last run ${timeTag(r.at)}: removed ${esc(plural(r.files, 'file'))} and ${esc(plural(r.meta_files, 'metadata entry', 'metadata entries'))}, freeing ${esc(bytes(r.bytes + r.meta_bytes))}.`;
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
          <td>${t.next_run ? timeTag(t.next_run) : '<span class="muted">–</span>'}</td>
          <td class="actions"><button class="small" data-run-task="${t.id}" data-name="${esc(t.name)}" ${t.running ? 'disabled' : ''}>Run now</button></td>
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
        setTimeout(draw, 600);
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

  const upRows = REPO_IDS.map((id) => {
    const h = health.find((x) => x.repo === id);
    if (!h) return `<tr><td>${chip(id)}</td><td colspan="4" class="muted">Not checked yet</td></tr>`;
    const status = h.ok
      ? `<span class="status"><span class="dot ok"></span>Reachable${h.status ? ` (HTTP ${h.status})` : ''}</span>`
      : `<span class="status"><span class="dot bad"></span>Unreachable: ${esc(h.error || 'HTTP ' + h.status)}</span>`;
    return `<tr><td>${chip(id)}</td><td class="mono">${esc(h.url)}</td><td>${status}</td><td class="r num">${esc(h.latency_ms)} ms</td><td>${timeTag(h.checked_at)}</td></tr>`;
  }).join('');

  setView(`
    <div class="page-head">
      <div><h1>System</h1><p>Granary ${esc(d.version)}, running for ${esc(duration(d.uptime_secs))}.</p></div>
      <div class="page-actions"><button data-check>Check upstreams now</button></div>
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
      toast(`${up} of ${r.health.length} upstreams reachable`, up === r.health.length ? '' : 'error');
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
        <input id="log-filter" type="search" placeholder="Filter messages" autocomplete="off"></div>
      <label class="check"><input type="checkbox" id="log-live" checked> Refresh automatically</label>
    </div>
    <div class="panel log" id="log" aria-live="off"></div>`);

  const draw = async () => {
    const qs = new URLSearchParams();
    if (level) qs.set('level', level);
    const f = $('#log-filter')?.value.trim();
    if (f) qs.set('q', f);
    const d = await api('/api/logs?' + qs);
    if (!isCurrent()) return;
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
      go('/logs', qs, { replace: true });
    })
  );
  $('#log-filter').addEventListener('input', debounce(() => draw().catch(() => {}), 250));
  const timer = setInterval(() => {
    if ($('#log-live')?.checked) draw().catch(() => {});
  }, 3000);
  onLeave(() => clearInterval(timer));
}

// -------------------------------------------------------------------- boot

window.addEventListener('hashchange', route);

document.addEventListener('DOMContentLoaded', () => {
  $('#search-form').addEventListener('submit', (e) => {
    e.preventDefault();
    const q = $('#global-search').value.trim();
    const qs = new URLSearchParams();
    if (q) qs.set('q', q);
    location.hash = '#/browse' + (q ? '?' + qs : '');
  });

  $('#token-cancel').addEventListener('click', () => $('#token-dialog').close('cancel'));

  $('#access').addEventListener('click', async (e) => {
    const b = e.target.closest('[data-access]');
    if (!b) return;
    if (b.dataset.access === 'lock') {
      tokenStore.set('');
      await refreshAuth();
      toast('Admin locked in this browser');
    } else if (await askToken()) {
      toast('Admin unlocked');
    }
  });

  document.addEventListener('keydown', (e) => {
    const typing = /^(input|textarea|select)$/i.test(document.activeElement?.tagName || '');
    if (e.key === '/' && !typing && !e.metaKey && !e.ctrlKey && !$('dialog[open]')) {
      e.preventDefault();
      $('#global-search').focus();
    }
  });

  refreshAuth();
  route();
});

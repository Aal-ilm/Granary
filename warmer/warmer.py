#!/usr/bin/env python3
"""Granary warmer: runs the real package tools against Granary so that
everything they download lands in Granary's cache.

Granary sends one job per ecosystem to POST /run with the manifests to use
(Cargo.toml, package.json, requirements.txt or pom.xml), the platforms to
cover and an access token. For a template made from a repo with lockfiles,
it sends the repo instead: the warmer clones it at the pinned commit through
Granary's git cache and runs `cargo fetch --locked` or `npm ci` in the
lockfile's folder, so exactly the locked versions are cached. The warmer writes them to a fresh temporary
directory, points the tools at Granary only, runs them, streams their output
back line by line and deletes the directory. Install scripts and build steps
never run: npm uses --ignore-scripts and pip downloads wheels only, unless
the template allows source distributions.

Settings (environment variables):
  WARMER_SECRET      shared secret Granary sends as a bearer token (required)
  WARMER_ALLOW_FROM  comma-separated host names or IPs allowed to connect,
                     e.g. "granary"; empty allows any address
  WARMER_LISTEN      address to listen on, default 0.0.0.0:9090
  WARMER_PIP         pip command, default "python3 -m pip"
  WARMER_WORKDIR     where job directories are made, default the system temp

Only the Python standard library is used.
"""

import base64
import hmac
import json
import os
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SECRET = os.environ.get("WARMER_SECRET", "")
ALLOW_FROM = [h.strip() for h in os.environ.get("WARMER_ALLOW_FROM", "").split(",") if h.strip()]
LISTEN = os.environ.get("WARMER_LISTEN", "0.0.0.0:9090")
PIP = shlex.split(os.environ.get("WARMER_PIP", f"{sys.executable} -m pip"))
WORKDIR = os.environ.get("WARMER_WORKDIR") or None
MAX_BODY = 2 * 1024 * 1024
RESULT = "##granary "
MAVEN_DEPENDENCY_PLUGIN = "org.apache.maven.plugins:maven-dependency-plugin:3.8.1:go-offline"

# Wheel platform tags and npm os/cpu for each Granary platform id.
PLATFORMS = {
    "linux-x86_64": {
        "pip": ["manylinux_2_28_x86_64", "manylinux_2_17_x86_64", "manylinux2014_x86_64", "linux_x86_64"],
        "npm": ("linux", "x64"),
    },
    "linux-aarch64": {
        "pip": ["manylinux_2_28_aarch64", "manylinux_2_17_aarch64", "manylinux2014_aarch64", "linux_aarch64"],
        "npm": ("linux", "arm64"),
    },
    "macos-arm64": {
        "pip": ["macosx_14_0_arm64", "macosx_13_0_arm64", "macosx_12_0_arm64", "macosx_11_0_arm64"],
        "npm": ("darwin", "arm64"),
    },
    "macos-x86_64": {
        "pip": ["macosx_14_0_x86_64", "macosx_12_0_x86_64", "macosx_10_15_x86_64", "macosx_10_9_x86_64"],
        "npm": ("darwin", "x64"),
    },
    "windows-x86_64": {"pip": ["win_amd64"], "npm": ("win32", "x64")},
}

# Files Granary may send for each ecosystem.
FILES = {
    "cargo": {"Cargo.toml", "src/main.rs"},
    "npm": {"package.json"},
    "pypi": {"requirements.txt"},
    "maven": {"pom.xml"},
}

# Terminal colour and cursor codes some tools print even without a terminal.
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")

# Running jobs: job id -> Job.
JOBS = {}
JOBS_LOCK = threading.Lock()


class Cancelled(Exception):
    pass


class ClientGone(Exception):
    pass


class Job:
    def __init__(self, job_id, emit, secrets):
        self.id = job_id
        self.emit_raw = emit
        self.secrets = [s for s in secrets if s]
        self.proc = None
        self.cancelled = False
        # Things that went differently than asked but didn't fail the run.
        self.warnings = []

    def warn(self, message):
        self.warnings.append(message)
        self.emit(message)

    def mask(self, text):
        for s in self.secrets:
            text = text.replace(s, "***")
        return text

    def emit(self, line):
        self.emit_raw(self.mask(ANSI.sub("", line.rstrip("\r\n"))))

    def cancel(self):
        self.cancelled = True
        p = self.proc
        if p and p.poll() is None:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass

    def run(self, args, cwd, env, show=None):
        """Run a command, streaming its output. Returns True on success."""
        if self.cancelled:
            raise Cancelled()
        self.emit("$ " + (show or " ".join(shlex.quote(a) for a in args)))
        try:
            self.proc = subprocess.Popen(
                args,
                cwd=cwd,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
                text=True,
                errors="replace",
                bufsize=1,
            )
        except FileNotFoundError:
            self.emit(f"{args[0]} is not installed in the warmer")
            return False
        try:
            for line in self.proc.stdout:
                self.emit(line)
        except ClientGone:
            self.cancel()
            raise
        code = self.proc.wait()
        if self.cancelled:
            raise Cancelled()
        if code != 0:
            self.emit(f"(exited with code {code})")
        return code == 0


def base_env(home):
    env = {
        "PATH": os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin"),
        "HOME": home,
        "LANG": "C.UTF-8",
        "NO_COLOR": "1",
        "CARGO_TERM_COLOR": "never",
        "npm_config_color": "false",
        "npm_config_update_notifier": "false",
        "PIP_DISABLE_PIP_VERSION_CHECK": "1",
    }
    for key in ("JAVA_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "SSL_CERT_FILE"):
        if key in os.environ:
            env[key] = os.environ[key]
    # rustup finds its toolchains through RUSTUP_HOME, which defaults to
    # ~/.rustup; jobs get their own HOME, so point it at the real one.
    if "RUSTUP_HOME" not in env:
        for candidate in (os.path.expanduser("~/.rustup"), "/usr/local/rustup"):
            if os.path.isdir(candidate):
                env["RUSTUP_HOME"] = candidate
                break
    return env


def write(path, text):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


def git_config(req, d):
    """A git config for the job: clones of allowed hosts (git dependencies,
    the template's repo) go through Granary with the job's token, and git
    runs no hooks and never uses the file protocol."""
    url = req["granary_url"]
    basic = base64.b64encode(f"token:{req['token']}".encode()).decode()
    lines = [
        "[core]\n\thooksPath = /dev/null\n",
        "[protocol \"file\"]\n\tallow = never\n",
        "[protocol \"ext\"]\n\tallow = never\n",
        "[advice]\n\tdetachedHead = false\n",
        f"[http \"{url}/\"]\n\textraHeader = Authorization: Basic {basic}\n",
    ]
    for host in req.get("git_hosts") or []:
        lines.append(f"[url \"{url}/git/{host}/\"]\n\tinsteadOf = https://{host}/\n")
    path = os.path.join(d, "gitconfig")
    write(path, "".join(lines))
    return path


# Tool settings a repo may carry that could send downloads elsewhere or ask
# for a toolchain the warmer can't install without the internet.
REPO_CONFIG = {".npmrc", ".yarnrc", ".yarnrc.yml", "rust-toolchain", "rust-toolchain.toml"}


def checkout(job, req, d, env):
    """Clone the repo at its commit through Granary; returns the folder to
    run in, or None."""
    repo = req["repo"]
    src = os.path.join(d, "src")
    os.makedirs(src)
    if not (
        job.run(["git", "init", "-q"], src, env)
        and job.run(["git", "fetch", "-q", "--depth", "1", "--no-tags", repo["url"], repo["commit"]], src, env)
        and job.run(["git", "checkout", "-q", "--detach", "FETCH_HEAD"], src, env)
    ):
        return None
    removed = 0
    for root, dirs, files in os.walk(src):
        dirs[:] = [x for x in dirs if x != ".git"]
        for name in files:
            in_cargo = os.path.basename(root) == ".cargo" and name in ("config", "config.toml")
            if name in REPO_CONFIG or in_cargo:
                os.remove(os.path.join(root, name))
                removed += 1
    if removed:
        job.emit(f"Removed {removed} tool config file(s) from the checkout so downloads go through Granary.")
    folder = os.path.normpath(os.path.join(src, repo["dir"]))
    if not folder.startswith(src) or not os.path.isdir(folder):
        job.emit(f"The repo has no folder {repo['dir']!r} at this commit")
        return None
    return folder


# The private registry's name in the warmer's Cargo config. Unusual on
# purpose, so it never clashes with a registry name a repo uses.
PRIVATE_REGISTRY = "granary-warm-private"
CRATES_IO = {"registry+https://github.com/rust-lang/crates.io-index", "sparse+https://index.crates.io/"}


def cargo_config(req):
    url = req["granary_url"]
    config = (
        f'[registries.granary]\nindex = "sparse+{url}/cargo/"\ncredential-provider = "cargo:token"\n\n'
        '[source.crates-io]\nreplace-with = "granary"\n\n'
        # Cargo's built-in git client can't send the token header.
        '[net]\nretry = 3\ngit-fetch-with-cli = true\n'
    )
    if req.get("cargo_private"):
        config += (
            f'\n[registries.{PRIVATE_REGISTRY}]\nindex = "sparse+{req["cargo_private"]}"\n'
            'credential-provider = "cargo:token"\n'
        )
    return config


def cargo_env(req, d, env):
    cargo_home = os.path.join(d, "cargo-home")
    write(os.path.join(cargo_home, "config.toml"), cargo_config(req))
    env = dict(env, CARGO_HOME=cargo_home, CARGO_REGISTRIES_GRANARY_TOKEN=req["token"])
    if req.get("cargo_private"):
        env[f"CARGO_REGISTRIES_{PRIVATE_REGISTRY.upper().replace('-', '_')}_TOKEN"] = req["token"]
    return env


def repo_registries(folder):
    """The registries a checkout's crates come from besides crates.io: the
    names its Cargo.toml files give (`registry = "..."`) and the index URLs
    its Cargo.lock files record."""
    names, sources = set(), []

    def deps(table):
        for key in ("dependencies", "dev-dependencies", "build-dependencies"):
            for spec in (table.get(key) or {}).values():
                if isinstance(spec, dict) and isinstance(spec.get("registry"), str):
                    names.add(spec["registry"])

    for root, dirs, files in os.walk(folder):
        dirs[:] = [x for x in dirs if x not in (".git", "target")]
        for name in files:
            path = os.path.join(root, name)
            try:
                if name == "Cargo.toml":
                    with open(path, "rb") as f:
                        doc = tomllib.load(f)
                    deps(doc)
                    deps(doc.get("workspace") or {})
                    for target in (doc.get("target") or {}).values():
                        if isinstance(target, dict):
                            deps(target)
                elif name == "Cargo.lock":
                    with open(path, "rb") as f:
                        doc = tomllib.load(f)
                    for pkg in doc.get("package") or []:
                        src = pkg.get("source") or ""
                        if src.startswith(("sparse+", "registry+")) and src not in CRATES_IO and src not in sources:
                            sources.append(src)
            except (OSError, tomllib.TOMLDecodeError):
                continue
    return sorted(names), sources


def private_crates(job, req, folder):
    """Cargo config that sends a checkout's private crates to Granary's
    private registry: each registry name the repo uses, and each private
    index its lockfile records, is replaced by it. The lockfile keeps its own
    URLs, so `cargo fetch --locked` still works."""
    names, sources = repo_registries(folder)
    if not names and not sources:
        return ""
    if not req.get("cargo_private"):
        job.emit(
            "This repo uses crates from another registry (" + ", ".join(names or sources) + "). "
            "They can only come from Granary's private Cargo registry, and whoever started this job has no private access, so they will fail."
        )
        return ""
    if len(sources) > 1:
        job.warn("The repo's Cargo.lock names several private registries; all of them are fetched from Granary's private registry.")
    config = ""
    # The repo's own registry names: the URL its lockfile records, so the
    # lockfile still matches; without one, the private registry itself.
    index = sources[0] if sources else f"sparse+{req['cargo_private']}"
    for name in names:
        config += f'\n[registries.{name}]\nindex = "{index}"\n'
    for i, src in enumerate(sources):
        config += f'\n[source.granary-lock-{i}]\nregistry = "{src}"\nreplace-with = "{PRIVATE_REGISTRY}"\n'
    job.emit(f"Private crates ({', '.join(names or sources)}) come from Granary's private Cargo registry.")
    return config


def warm_cargo(job, req, d, env):
    env = cargo_env(req, d, env)
    # cargo fetch downloads every target's dependencies, so one run covers
    # all platforms.
    if req.get("repo"):
        folder = checkout(job, req, d, env)
        if folder is None:
            return False, {}
        extra = private_crates(job, req, folder)
        if extra:
            write(os.path.join(env["CARGO_HOME"], "config.toml"), cargo_config(req) + extra)
        if job.run(["cargo", "fetch", "--locked"], folder, env):
            return True, {}
        # A lockfile that's out of date with its manifests (common for fuzz
        # targets and examples) can't be used as it is; cache the newest
        # matching versions instead of failing.
        rel = req["repo"]["dir"] or "the repo root"
        job.warn(f"Cargo.lock in {rel} doesn't match its Cargo.toml, so the newest matching versions were cached instead of the locked ones.")
        return job.run(["cargo", "fetch"], folder, env), {}
    ok = job.run(["cargo", "generate-lockfile"], d, env) and job.run(["cargo", "fetch"], d, env)
    return ok, lockfiles(d, ["Cargo.lock"])


def warm_npm(job, req, d, env):
    url = req["granary_url"]
    hostport = urllib.parse.urlsplit(url).netloc
    npmrc = f"registry={url}/npm/\n//{hostport}/npm/:_authToken={req['token']}\nfund=false\naudit=false\n"
    write(os.path.join(d, ".npmrc"), npmrc)
    env = dict(env, npm_config_cache=os.path.join(d, "npm-cache"))
    if req.get("repo"):
        # The repo's own .npmrc was removed; the registry also comes from the
        # environment, which outranks any project setting.
        write(os.path.join(d, "user.npmrc"), npmrc)
        env = dict(env, npm_config_userconfig=os.path.join(d, "user.npmrc"), npm_config_registry=f"{url}/npm/")
        folder = checkout(job, req, d, env)
        if folder is None:
            return False, {}
        ok = True
        for plat in req["platforms"]:
            os_, cpu = PLATFORMS[plat]["npm"]
            flags = ["--ignore-scripts", "--no-audit", "--no-fund", f"--os={os_}", f"--cpu={cpu}"]
            if job.run(["npm", "ci"] + flags, folder, env):
                continue
            # npm ci refuses a lockfile that's out of step with package.json.
            rel = req["repo"]["dir"] or "the repo root"
            note = f"package-lock.json in {rel} doesn't match package.json, so npm install picked the versions instead of the locked ones."
            if note not in job.warnings:
                job.warn(note)
            ok = job.run(["npm", "install"] + flags, folder, env) and ok
        return ok, {}
    ok = True
    for plat in req["platforms"]:
        os_, cpu = PLATFORMS[plat]["npm"]
        # The lockfile from the first run keeps versions the same for every
        # platform; only node_modules is cleared between runs.
        shutil.rmtree(os.path.join(d, "node_modules"), ignore_errors=True)
        ok = job.run(
            ["npm", "install", "--ignore-scripts", "--no-audit", "--no-fund", f"--os={os_}", f"--cpu={cpu}"],
            d,
            env,
        ) and ok
    return ok, lockfiles(d, ["package-lock.json"])


def warm_pypi(job, req, d, env):
    url = urllib.parse.urlsplit(req["granary_url"])
    index = f"{url.scheme}://__token__:{urllib.parse.quote(req['token'], safe='')}@{url.netloc}/pypi/simple/"
    shown_index = f"{url.scheme}://__token__:***@{url.netloc}/pypi/simple/"
    common = ["download", "--no-cache-dir", "--dest", "out", "--index-url", index]
    shown_common = ["download", "--no-cache-dir", "--dest", "out", "--index-url", shown_index]
    if url.scheme == "http":
        host = url.hostname or ""
        common += ["--trusted-host", host]
        shown_common += ["--trusted-host", host]
    ver = req["python_version"]
    abi = "cp" + ver.replace(".", "")
    ok = True
    for plat in req["platforms"]:
        target = ["--only-binary=:all:", "--python-version", ver, "--implementation", "cp",
                  "--abi", abi, "--abi", "abi3", "--abi", "none"]
        for tag in PLATFORMS[plat]["pip"]:
            target += ["--platform", tag]
        args = common + target + ["-r", "requirements.txt"]
        show = " ".join(PIP + shown_common + target + ["-r", "requirements.txt"])
        ok = job.run(PIP + args, d, env, show=show) and ok
    if req.get("allow_sdist"):
        job.emit("Source distributions are allowed for this template; building one can run its code.")
        ok = job.run(PIP + common + ["-r", "requirements.txt"], d, env,
                     show=" ".join(PIP + shown_common + ["-r", "requirements.txt"])) and ok
    # pip has no lockfile; list what was downloaded instead.
    pins = set()
    for name in os.listdir(os.path.join(d, "out")) if os.path.isdir(os.path.join(d, "out")) else []:
        parts = name.split("-")
        if name.endswith(".whl") and len(parts) >= 2:
            pins.add(f"{parts[0].replace('_', '-').lower()}=={parts[1]}")
        elif name.endswith((".tar.gz", ".zip")) and "-" in name:
            stem = name[: -len(".tar.gz")] if name.endswith(".tar.gz") else name[: -len(".zip")]
            n, v = stem.rsplit("-", 1)
            pins.add(f"{n.replace('_', '-').lower()}=={v}")
    locks = {}
    if pins:
        locks["requirements.lock.txt"] = "".join(f"{p}\n" for p in sorted(pins))
    return ok, locks


def warm_maven(job, req, d, env):
    url = req["granary_url"]
    write(
        os.path.join(d, "settings.xml"),
        f"""<settings>
  <mirrors>
    <mirror><id>granary</id><mirrorOf>*</mirrorOf><url>{url}/maven</url></mirror>
    <!-- Granary is plain HTTP on an internal network; don't block it. -->
    <mirror><id>maven-default-http-blocker</id><mirrorOf>external:dummy:*</mirrorOf><url>http://0.0.0.0/</url><blocked>false</blocked></mirror>
  </mirrors>
  <servers>
    <server><id>granary</id><username>token</username><password>{req['token']}</password></server>
  </servers>
</settings>
""",
    )
    ok = job.run(
        ["mvn", "-B", "-ntp", "-s", "settings.xml", "-Dmaven.repo.local=" + os.path.join(d, "m2"),
         MAVEN_DEPENDENCY_PLUGIN],
        d,
        env,
    )
    return ok, {}


WARMERS = {"cargo": warm_cargo, "npm": warm_npm, "pypi": warm_pypi, "maven": warm_maven}


def lockfiles(d, names):
    out = {}
    for n in names:
        p = os.path.join(d, n)
        if os.path.isfile(p) and os.path.getsize(p) < 8 * 1024 * 1024:
            with open(p, encoding="utf-8", errors="replace") as f:
                out[n] = f.read()
    return out


def check_request(req):
    eco = req.get("ecosystem")
    if eco not in WARMERS:
        return f"unknown ecosystem {eco!r}"
    files = req.get("files")
    repo = req.get("repo")
    if repo is not None:
        if eco not in ("cargo", "npm") or not isinstance(repo, dict):
            return "a repo checkout is only used for Cargo and npm lockfiles"
        prefix = str(req.get("granary_url", "")) + "/git/"
        url, commit, folder = str(repo.get("url", "")), str(repo.get("commit", "")), str(repo.get("dir", ""))
        if not url.startswith(prefix) or not url.endswith(".git"):
            return "the repo must be cloned through Granary's /git/ path"
        if len(commit) != 40 or any(c not in "0123456789abcdef" for c in commit):
            return "commit must be a full SHA"
        if folder.startswith("/") or ".." in folder.split("/") or "\\" in folder:
            return "dir must be a folder inside the repo"
    elif not isinstance(files, dict) or not files:
        return "no files"
    for path in files or {}:
        if path not in FILES[eco]:
            return f"unexpected file {path!r}"
    for host in req.get("git_hosts") or []:
        # A host name, with a port for a Gitea that has one.
        name, colon, port = host.partition(":") if isinstance(host, str) else ("", "", "")
        bad_port = colon and not (port.isdigit() and 0 < int(port) < 65536)
        if not name or any(not (c.isalnum() or c in ".-") for c in name) or bad_port:
            return "git_hosts must be host names"
    plats = req.get("platforms") or []
    if not plats or any(p not in PLATFORMS for p in plats):
        return "unknown platform"
    url = urllib.parse.urlsplit(str(req.get("granary_url", "")))
    if url.scheme not in ("http", "https") or not url.netloc:
        return "granary_url must be an http(s) URL"
    if not str(req.get("token", "")):
        return "no token"
    if req.get("cargo_private") is not None:
        # The token goes with it, so only ever to Granary itself.
        private = urllib.parse.urlsplit(str(req["cargo_private"]))
        if (private.scheme, private.netloc) != (url.scheme, url.netloc) or not private.path.endswith("/"):
            return "cargo_private must be a path on granary_url, ending in /"
    ver = str(req.get("python_version", ""))
    if not (len(ver.split(".")) == 2 and all(p.isdigit() for p in ver.split("."))):
        return "python_version must look like 3.12"
    if not isinstance(req.get("job"), int):
        return "job must be a number"
    return None


def run_job(req, emit):
    """Run one ecosystem's tools. Returns the result dict."""
    token = req["token"]
    basic = base64.b64encode(f"token:{token}".encode()).decode()
    job = Job(req["job"], emit, [token, urllib.parse.quote(token, safe=""), basic])
    with JOBS_LOCK:
        JOBS[job.id] = job
    d = tempfile.mkdtemp(prefix=f"granary-job-{job.id}-", dir=WORKDIR)
    try:
        for path, text in (req.get("files") or {}).items():
            write(os.path.join(d, path), text)
        env = base_env(os.path.join(d, "home"))
        os.makedirs(env["HOME"], exist_ok=True)
        env.update(
            GIT_CONFIG_GLOBAL=git_config(req, d),
            GIT_CONFIG_NOSYSTEM="1",
            GIT_TERMINAL_PROMPT="0",
        )
        ok, locks = WARMERS[req["ecosystem"]](job, req, d, env)
        if ok:
            return {"ok": True, "lockfiles": locks, "warnings": job.warnings}
        return {"ok": False, "error": "a tool exited with an error; see the log above", "lockfiles": locks, "warnings": job.warnings}
    except Cancelled:
        return {"ok": False, "error": "cancelled"}
    finally:
        with JOBS_LOCK:
            if JOBS.get(job.id) is job:
                del JOBS[job.id]
        shutil.rmtree(d, ignore_errors=True)


def tool_versions():
    tools = {}
    checks = {
        "cargo": ["cargo", "--version"],
        "node": ["node", "--version"],
        "npm": ["npm", "--version"],
        "pip": PIP + ["--version"],
        "maven": ["mvn", "-B", "-v"],
        "java": ["java", "-version"],
    }
    env = base_env(tempfile.gettempdir())
    for name, args in checks.items():
        try:
            r = subprocess.run(args, capture_output=True, text=True, timeout=60, env=env)
            first = (r.stdout or r.stderr).strip().splitlines()
            tools[name] = ANSI.sub("", first[0]) if first else "?"
        except (OSError, subprocess.TimeoutExpired):
            tools[name] = "missing"
    return tools


TOOLS = {}


def allowed_addresses():
    addrs = set()
    for host in ALLOW_FROM:
        try:
            for info in socket.getaddrinfo(host, None):
                addrs.add(info[4][0])
        except socket.gaierror:
            pass
    return addrs


class Handler(BaseHTTPRequestHandler):
    server_version = "granary-warmer"

    def log_message(self, fmt, *args):
        sys.stderr.write("%s %s\n" % (self.address_string(), fmt % args))

    def reply(self, code, body, ctype="application/json"):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def authorized(self):
        if ALLOW_FROM:
            peer = self.client_address[0]
            if peer.startswith("::ffff:"):
                peer = peer[7:]
            # Loopback is only reachable from inside this container (its own
            # health check); everything else must come from an allowed host.
            if peer not in ("127.0.0.1", "::1") and peer not in allowed_addresses():
                self.reply(403, json.dumps({"error": "this address may not use the warmer"}))
                return False
        auth = self.headers.get("Authorization", "")
        given = auth[7:] if auth.startswith("Bearer ") else ""
        if not hmac.compare_digest(given.encode(), SECRET.encode()):
            self.reply(401, json.dumps({"error": "wrong or missing secret"}))
            return False
        return True

    def do_GET(self):
        if not self.authorized():
            return
        if self.path == "/health":
            with JOBS_LOCK:
                running = sorted(JOBS)
            self.reply(200, json.dumps({"ok": True, "tools": TOOLS, "running": running}))
        else:
            self.reply(404, json.dumps({"error": "not found"}))

    def do_DELETE(self):
        if not self.authorized():
            return
        parts = self.path.strip("/").split("/")
        if len(parts) == 2 and parts[0] == "run" and parts[1].isdigit():
            with JOBS_LOCK:
                job = JOBS.get(int(parts[1]))
            if job:
                job.cancel()
            self.reply(200, json.dumps({"stopped": bool(job)}))
        else:
            self.reply(404, json.dumps({"error": "not found"}))

    def do_POST(self):
        if not self.authorized():
            return
        if self.path != "/run":
            self.reply(404, json.dumps({"error": "not found"}))
            return
        length = int(self.headers.get("Content-Length") or 0)
        if length <= 0 or length > MAX_BODY:
            self.reply(400, json.dumps({"error": "missing or oversized body"}))
            return
        try:
            req = json.loads(self.rfile.read(length))
        except ValueError:
            self.reply(400, json.dumps({"error": "body is not JSON"}))
            return
        problem = check_request(req)
        if problem:
            self.reply(400, json.dumps({"error": problem}))
            return
        with JOBS_LOCK:
            busy = req["job"] in JOBS
        if busy:
            self.reply(409, json.dumps({"error": "that job is already running"}))
            return

        # Stream plain text lines; the connection closing ends the body.
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.close_connection = True

        def emit(line):
            try:
                self.wfile.write((line + "\n").encode("utf-8", "replace"))
                self.wfile.flush()
            except OSError as e:
                raise ClientGone() from e

        try:
            result = run_job(req, emit)
            emit(RESULT + json.dumps(result))
        except ClientGone:
            sys.stderr.write(f"job {req['job']}: Granary went away; stopped\n")


def healthcheck():
    """`granary-warmer healthcheck` for the container's HEALTHCHECK."""
    import urllib.request

    port = LISTEN.rsplit(":", 1)[-1]
    r = urllib.request.Request(f"http://127.0.0.1:{port}/health", headers={"Authorization": f"Bearer {SECRET}"})
    try:
        with urllib.request.urlopen(r, timeout=5) as resp:
            sys.exit(0 if resp.status == 200 else 1)
    except Exception:
        sys.exit(1)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "healthcheck":
        healthcheck()
    if len(SECRET) < 16:
        sys.exit("granary-warmer: set WARMER_SECRET to the same value as GRANARY_WARMER_SECRET (16+ characters)")
    TOOLS.update(tool_versions())
    host, port = LISTEN.rsplit(":", 1)
    server = ThreadingHTTPServer((host, int(port)), Handler)
    server.daemon_threads = True
    print(f"granary-warmer listening on {LISTEN}", flush=True)
    for name, version in TOOLS.items():
        print(f"  {name}: {version}", flush=True)
    if ALLOW_FROM:
        print(f"  accepting connections from: {', '.join(ALLOW_FROM)}", flush=True)
    else:
        print("  WARMER_ALLOW_FROM is empty: any address with the secret may connect", flush=True)
    # Exit straight away on SIGTERM; job directories are temporary anyway.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    server.serve_forever()


if __name__ == "__main__":
    main()

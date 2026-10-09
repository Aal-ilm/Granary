#!/usr/bin/env bash
# Point this machine's npm, pip and Cargo at Granary, or switch them back to
# the public registries, without retyping anything.
#
#   granary-client.sh use-granary   install through Granary
#   granary-client.sh use-public    install straight from the public registries
#   granary-client.sh status        show where each tool points
#   granary-client.sh remove        undo everything, including the saved token
#   granary-client.sh fix-lockfile DIR
#                                   make a project's package-lock.json use
#                                   registry.npmjs.org URLs (works either way)
#
# Settings (environment variables):
#   GRANARY_URL     Granary's address, e.g. http://10.212.46.20:8480
#                   (remembered after the first use-granary)
#   GRANARY_TOKEN   access token; asked for once if not set, then remembered
#   TOOLS           which tools to change, default "npm pip cargo"
#
# Compatible with Bash 3.2 (macOS) and later. Only edits per-user config:
# ~/.npmrc, your user pip.conf and $CARGO_HOME/config.toml. Each file is
# backed up before its first change.

set -euo pipefail

STATE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/granary-client"
URL_FILE="$STATE_DIR/url"
TOKEN_FILE="$STATE_DIR/token"
CARGO_DIR="${CARGO_HOME:-$HOME/.cargo}"
TOOLS="${TOOLS:-npm pip cargo}"
PIP=(python3 -m pip)

REG_BEGIN="# >>> granary registry (managed by granary-client) >>>"
REG_END="# <<< granary registry <<<"
USE_BEGIN="# >>> granary mirror (managed by granary-client) >>>"
USE_END="# <<< granary mirror <<<"

info() { printf '  %s\n' "$*"; }
ok() { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*" >&2; }
die() {
  printf '\033[31merror:\033[0m %s\n' "$*" >&2
  exit 1
}
has() { command -v "$1" >/dev/null 2>&1; }
wants() { case " $TOOLS " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# ------------------------------------------------------------------ settings

load_url() {
  local url="${GRANARY_URL:-}"
  if [ -z "$url" ] && [ -f "$URL_FILE" ]; then url="$(cat "$URL_FILE")"; fi
  [ -n "$url" ] || die "set GRANARY_URL to Granary's address, e.g. make use-granary GRANARY_URL=http://10.212.46.20:8480"
  url="${url%/}"
  case "$url" in
    http://* | https://*) ;;
    *) die "GRANARY_URL must start with http:// or https:// (got $url)" ;;
  esac
  URL="$url"
  SCHEME="${URL%%://*}"
  HOSTPORT="${URL#*://}"
  HOSTPORT="${HOSTPORT%%/*}"
}

save_state() {
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  printf '%s\n' "$URL" >"$URL_FILE"
  (umask 077 && printf '%s\n' "$TOKEN" >"$TOKEN_FILE")
}

granary_up() {
  curl -fsS -m 5 -o /dev/null "$URL/healthz" 2>/dev/null
}

load_token() {
  TOKEN="${GRANARY_TOKEN:-}"
  if [ -z "$TOKEN" ] && [ -f "$TOKEN_FILE" ]; then TOKEN="$(cat "$TOKEN_FILE")"; fi
  if [ -z "$TOKEN" ]; then
    [ -r /dev/tty ] || die "no token saved; set GRANARY_TOKEN"
    printf 'Granary access token (create one under Access tokens): ' >/dev/tty
    IFS= read -rs TOKEN </dev/tty
    printf '\n' >/dev/tty
  fi
  TOKEN="$(printf '%s' "$TOKEN" | tr -d '[:space:]')"
  [ -n "$TOKEN" ] || die "no token entered"
  local code
  code="$(curl -s -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$URL/cargo/config.json" || true)"
  case "$code" in
    200) ;;
    401) die "Granary rejected that token. Create a new one under Access tokens" ;;
    *) die "couldn't check the token (HTTP $code from $URL)" ;;
  esac
}

backup() {
  local file="$1"
  [ -f "$file" ] || return 0
  local dir="$STATE_DIR/backups"
  local name
  name="$(printf '%s' "$file" | tr '/' '_')"
  # Keep the original from before granary-client ever touched it.
  if [ ! -f "$dir/$name.orig" ]; then
    mkdir -p "$dir"
    chmod 700 "$STATE_DIR" "$dir"
    cp -p "$file" "$dir/$name.orig"
  fi
}

# Drop blank lines at the end of a file (left behind by removed blocks).
trim_trailing_blank() {
  local file="$1"
  [ -f "$file" ] || return 0
  local tmp="$file.granary-tmp"
  awk 'NF { for (i = 0; i < blank; i++) print ""; blank = 0; print; next } { blank++ }' "$file" >"$tmp"
  cat "$tmp" >"$file"
  rm -f "$tmp"
}

# Remove the lines between two markers (inclusive) from a file.
strip_block() {
  local file="$1" begin="$2" end="$3"
  [ -f "$file" ] || return 0
  local tmp="$file.granary-tmp"
  awk -v b="$begin" -v e="$end" '
    $0 == b { skip = 1; next }
    $0 == e { skip = 0; next }
    !skip
  ' "$file" >"$tmp"
  cat "$tmp" >"$file"
  rm -f "$tmp"
  trim_trailing_blank "$file"
}

# --------------------------------------------------------------------- npm

# npm's per-user config file. Newer npm won't report this through
# "npm config get", so follow npm's documented lookup instead.
npm_userconfig() {
  printf '%s' "${NPM_CONFIG_USERCONFIG:-${npm_config_userconfig:-$HOME/.npmrc}}"
}

npm_use_granary() {
  has npm || { info "npm: not installed, skipped"; return 0; }
  local rc
  rc="$(npm_userconfig)"
  backup "$rc"
  npm config set registry "$URL/npm/"
  npm config set "//$HOSTPORT/npm/:_authToken" "$TOKEN"
  chmod 600 "$rc"
  ok "npm  -> $URL/npm/"
}

npm_use_public() {
  has npm || return 0
  local current
  current="$(npm config get registry)"
  case "$current" in
    "$URL"/npm/*) npm config delete registry ;;
  esac
  ok "npm  -> $(npm config get registry)"
  info "     projects whose package-lock.json lists Granary URLs need: make fix-lockfile DIR=<project>"
}

npm_remove() {
  has npm || return 0
  npm_use_public >/dev/null
  npm config delete "//$HOSTPORT/npm/:_authToken" 2>/dev/null || true
  ok "npm: Granary settings and token removed"
}

npm_status() {
  has npm || { info "npm    not installed"; return 0; }
  info "npm    $(npm config get registry)"
}

# --------------------------------------------------------------------- pip

pip_ok() { has python3 && "${PIP[@]}" --version >/dev/null 2>&1; }

pip_use_granary() {
  pip_ok || { info "pip: not installed, skipped"; return 0; }
  local out file
  out="$("${PIP[@]}" config --user set global.index-url "$SCHEME://__token__:$TOKEN@$HOSTPORT/pypi/simple/" 2>&1)"
  file="$(printf '%s\n' "$out" | sed -n 's/^Writing to //p' | tail -n 1)"
  [ -n "$file" ] && [ -f "$file" ] && chmod 600 "$file"
  ok "pip  -> $URL/pypi/simple/"
}

pip_use_public() {
  pip_ok || return 0
  local current
  current="$("${PIP[@]}" config --user get global.index-url 2>/dev/null || true)"
  case "$current" in
    *"@$HOSTPORT/pypi/"* | *"//$HOSTPORT/pypi/"*)
      "${PIP[@]}" config --user unset global.index-url >/dev/null 2>&1 || true
      ;;
  esac
  ok "pip  -> https://pypi.org/simple/ (default)"
}

pip_remove() {
  pip_ok || return 0
  pip_use_public >/dev/null
  ok "pip: Granary index and token removed"
}

pip_status() {
  pip_ok || { info "pip    not installed"; return 0; }
  local current
  current="$("${PIP[@]}" config --user get global.index-url 2>/dev/null || true)"
  # Never print the token embedded in the URL.
  current="$(printf '%s' "$current" | sed 's#//[^/@]*@#//***@#')"
  info "pip    ${current:-https://pypi.org/simple/ (default)}"
}

# ------------------------------------------------------------------- cargo

cargo_file() {
  # Cargo also reads the legacy name "config" without the extension.
  if [ -f "$CARGO_DIR/config" ] && [ ! -f "$CARGO_DIR/config.toml" ]; then
    printf '%s' "$CARGO_DIR/config"
  else
    printf '%s' "$CARGO_DIR/config.toml"
  fi
}

cargo_credentials() {
  if [ -f "$CARGO_DIR/credentials" ] && [ ! -f "$CARGO_DIR/credentials.toml" ]; then
    printf '%s' "$CARGO_DIR/credentials"
  else
    printf '%s' "$CARGO_DIR/credentials.toml"
  fi
}

# Remove any [registries.granary] table from Cargo's credentials file. That
# file only holds tokens, so replacing Granary's entry is always safe.
cargo_forget_token() {
  local file
  file="$(cargo_credentials)"
  [ -f "$file" ] || return 0
  local tmp="$file.granary-tmp"
  awk '
    /^[[:space:]]*\[/ { skip = ($0 ~ /^[[:space:]]*\[registries\.granary\][[:space:]]*$/) }
    !skip
  ' "$file" >"$tmp"
  cat "$tmp" >"$file"
  rm -f "$tmp"
  trim_trailing_blank "$file"
}

# Store the token where Cargo's built-in "cargo:token" provider reads it.
# (This is what "cargo login" writes, without needing a network round trip.)
cargo_save_token() {
  local file
  file="$(cargo_credentials)"
  cargo_forget_token
  (umask 077 && touch "$file")
  {
    [ -s "$file" ] && printf '\n'
    printf '[registries.granary]\ntoken = "%s"\n' "$TOKEN"
  } >>"$file"
  chmod 600 "$file"
}

# Fail if the user's own config already defines what we need to add.
cargo_check_conflicts() {
  local file="$1"
  [ -f "$file" ] || return 0
  local outside
  outside="$(awk -v b1="$REG_BEGIN" -v e1="$REG_END" -v b2="$USE_BEGIN" -v e2="$USE_END" '
    $0 == b1 || $0 == b2 { skip = 1; next }
    $0 == e1 || $0 == e2 { skip = 0; next }
    !skip
  ' "$file")"
  if printf '%s\n' "$outside" | grep -Eq '^[[:space:]]*\[(source\.crates-io|registries\.granary)\]'; then
    die "$file already has its own [source.crates-io] or [registries.granary] section. Remove it first, then run this again"
  fi
}

cargo_use_granary() {
  has cargo || { info "cargo: not installed, skipped"; return 0; }
  local file
  file="$(cargo_file)"
  mkdir -p "$CARGO_DIR"
  cargo_check_conflicts "$file"
  backup "$file"
  strip_block "$file" "$REG_BEGIN" "$REG_END"
  strip_block "$file" "$USE_BEGIN" "$USE_END"
  {
    [ -s "$file" ] && printf '\n'
    printf '%s\n' "$REG_BEGIN"
    printf '[registries.granary]\nindex = "sparse+%s/cargo/"\ncredential-provider = "cargo:token"\n' "$URL"
    printf '%s\n\n' "$REG_END"
    printf '%s\n' "$USE_BEGIN"
    printf '[source.crates-io]\nreplace-with = "granary"\n'
    printf '%s\n' "$USE_END"
  } >>"$file"
  cargo_save_token
  ok "cargo -> $URL/cargo/"
}

cargo_use_public() {
  has cargo || return 0
  local file
  file="$(cargo_file)"
  # Keep the registry and its token so switching back needs nothing.
  strip_block "$file" "$USE_BEGIN" "$USE_END"
  ok "cargo -> crates.io (default)"
}

cargo_remove() {
  has cargo || return 0
  local file
  file="$(cargo_file)"
  strip_block "$file" "$USE_BEGIN" "$USE_END"
  cargo_forget_token
  strip_block "$file" "$REG_BEGIN" "$REG_END"
  ok "cargo: Granary registry and token removed"
}

cargo_status() {
  has cargo || { info "cargo  not installed"; return 0; }
  local file
  file="$(cargo_file)"
  if [ -f "$file" ] && grep -qxF "$USE_BEGIN" "$file"; then
    local idx
    idx="$(awk -F'"' '/^index = "sparse\+/ { sub(/^sparse\+/, "", $2); print $2; exit }' "$file")"
    info "cargo  ${idx:-Granary}"
  else
    info "cargo  https://index.crates.io/ (default)"
  fi
}

# -------------------------------------------------------------- commands

cmd_use_granary() {
  load_url
  echo "Pointing package managers at Granary ($URL)"
  granary_up || die "Granary isn't answering at $URL. Check the address, or stay on the public registries"
  load_token
  for t in $TOOLS; do "${t}_use_granary"; done
  save_state
  echo "Done. Installs now go through Granary."
}

cmd_use_public() {
  load_url
  echo "Pointing package managers at the public registries"
  for t in $TOOLS; do "${t}_use_public"; done
  echo "Done. Token and Granary address are kept, so 'use-granary' switches back instantly."
}

cmd_remove() {
  load_url
  echo "Removing all Granary settings from this machine"
  for t in $TOOLS; do "${t}_remove"; done
  rm -f "$TOKEN_FILE" "$URL_FILE"
  ok "saved token and address removed"
  [ -d "$STATE_DIR/backups" ] && info "original config backups remain in $STATE_DIR/backups"
  echo "Done. Revoke the token under Access tokens if it's no longer needed."
}

# Rewrite Granary tarball URLs in a project's npm lockfile to the public
# registry. The integrity hashes are identical, so this is always safe, and
# npm still downloads through Granary when it is your registry (npm maps
# registry.npmjs.org URLs onto the configured registry by default).
cmd_fix_lockfile() {
  load_url
  local dir="${1:-.}"
  [ -d "$dir" ] || die "no such folder: $dir"
  local esc changed=0
  esc="$(printf '%s' "$URL/npm/" | sed 's/[][\.*^$|/]/\\&/g')"
  for f in "$dir/package-lock.json" "$dir/npm-shrinkwrap.json"; do
    [ -f "$f" ] || continue
    local n
    n="$(grep -c "$URL/npm/" "$f" || true)"
    if [ "$n" -gt 0 ]; then
      sed "s|$esc|https://registry.npmjs.org/|g" "$f" >"$f.granary-tmp"
      cat "$f.granary-tmp" >"$f"
      rm -f "$f.granary-tmp"
      ok "$f: $n Granary URLs now point at registry.npmjs.org"
      changed=1
    fi
  done
  [ "$changed" = 1 ] || info "no Granary URLs found in $dir"
}

cmd_status() {
  local url="${GRANARY_URL:-}"
  [ -z "$url" ] && [ -f "$URL_FILE" ] && url="$(cat "$URL_FILE")"
  if [ -n "$url" ]; then
    URL="${url%/}"
    if granary_up; then echo "Granary: $URL (up)"; else echo "Granary: $URL (not answering)"; fi
  else
    echo "Granary: no address saved yet"
  fi
  [ -f "$TOKEN_FILE" ] && echo "Token:   saved in $TOKEN_FILE" || echo "Token:   not saved"
  echo "Package managers:"
  for t in $TOOLS; do "${t}_status"; done
}

for t in $TOOLS; do
  case "$t" in npm | pip | cargo) ;; *) die "unknown tool in TOOLS: $t (use npm, pip, cargo)" ;; esac
done

case "${1:-}" in
  use-granary) cmd_use_granary ;;
  use-public) cmd_use_public ;;
  remove) cmd_remove ;;
  status) cmd_status ;;
  fix-lockfile) cmd_fix_lockfile "${2:-.}" ;;
  *) die "usage: $0 use-granary | use-public | status | remove | fix-lockfile [dir]" ;;
esac

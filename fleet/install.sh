#!/bin/sh
# OpenBot Fleet one-shot installer: run this ON the VPS.
#
# Installs, entirely under $HOME and with no sudo:
#   1. herdr-core  -> ~/.local/bin/herdr-core (plus an `hc` symlink)
#   2. Pi          -> the npm package @mariozechner/pi-coding-agent
#   3. a pre-created herdr-app space whose terminal runs Pi
#      (via precreate.py, next to this script)
#
# This script NEVER writes an API key and NEVER touches any .env file.
# Pi's own provider configuration is the operator's job, afterwards.
#
# On Linux it also aligns the core with login sessions (linger plus
# the runtime-dir socket), so an install driven from a non-login
# context stays reachable over ssh; see ensure_runtime_context below.
#
# Re-running is safe: every step skips work that is already done.
set -eu

CORE_BIN=""
CORE_URL=""
CORE_SHA256=""
PI_PACKAGE="@mariozechner/pi-coding-agent"
SKIP_CORE=0
SKIP_PI=0
SKIP_SPACE=0
SPACE_NAME=""
TERMINAL_NAME="pi"
PI_CMD="pi"

RELEASES_BASE="https://github.com/aimachy/herdr-core/releases"
MANIFEST_URL="$RELEASES_BASE/latest/download/core-latest.json"

say() { printf '%s\n' "$*"; }
die() { printf 'install: error: %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
usage: install.sh [options]

  --core-bin <file>     install this local herdr-core binary
                        (the download-then-upload delivery path)
  --core-url <url>      download herdr-core from this URL instead of the
                        fork's release manifest; a .tar.gz/.tgz/.tar URL is
                        unpacked, anything else is taken as the raw binary
  --core-sha256 <hex>   expected sha256 for --core-url downloads
  --skip-core           do not install/update herdr-core
  --pi-package <name>   npm package for Pi (default: @mariozechner/pi-coding-agent)
  --skip-pi             do not install Pi
  --space-name <name>   pre-created space name (default: hostname)
  --terminal-name <n>   terminal name (default: pi)
  --pi-cmd <cmd>        command the pre-created terminal runs (default: pi)
  --skip-space          do not pre-create the space
  -h, --help            this text
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --core-bin)    [ $# -ge 2 ] || die "--core-bin needs a value"; CORE_BIN=$2; shift 2 ;;
        --core-url)    [ $# -ge 2 ] || die "--core-url needs a value"; CORE_URL=$2; shift 2 ;;
        --core-sha256) [ $# -ge 2 ] || die "--core-sha256 needs a value"; CORE_SHA256=$2; shift 2 ;;
        --pi-package)  [ $# -ge 2 ] || die "--pi-package needs a value"; PI_PACKAGE=$2; shift 2 ;;
        --space-name)  [ $# -ge 2 ] || die "--space-name needs a value"; SPACE_NAME=$2; shift 2 ;;
        --terminal-name) [ $# -ge 2 ] || die "--terminal-name needs a value"; TERMINAL_NAME=$2; shift 2 ;;
        --pi-cmd)      [ $# -ge 2 ] || die "--pi-cmd needs a value"; PI_CMD=$2; shift 2 ;;
        --skip-core)   SKIP_CORE=1; shift ;;
        --skip-pi)     SKIP_PI=1; shift ;;
        --skip-space)  SKIP_SPACE=1; shift ;;
        -h|--help)     usage; exit 0 ;;
        *)             die "unknown option: $1 (try --help)" ;;
    esac
done

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BIN_DIR="$HOME/.local/bin"
mkdir -p "$BIN_DIR"
PATH="$BIN_DIR:$PATH"
export PATH

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM

# ---------------------------------------------------------------- platform
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        x86_64|amd64) arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *) die "unsupported CPU architecture: $arch" ;;
    esac
    case "$os" in
        Linux)  echo "linux-$arch" ;;
        Darwin) echo "macos-$arch" ;;
        *) die "unsupported operating system: $os" ;;
    esac
}

# ---------------------------------------------------------------- helpers
download() { # url dest
    if command -v curl >/dev/null 2>&1; then
        curl -fsL --retry 2 --connect-timeout 10 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "need curl or wget to download $1"
    fi
}

sha256_of() { # file
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "need sha256sum or shasum to verify downloads"
    fi
}

check_sha256() { # file expected
    actual=$(sha256_of "$1")
    [ "$actual" = "$2" ] || die "sha256 mismatch for $1: got $actual, want $2"
    say "install: sha256 ok ($2)"
}

# Print "<url> <sha256>" for TARGET from a core-latest.json manifest file.
manifest_entry() { # manifest_file target
    if command -v python3 >/dev/null 2>&1; then
        python3 - "$1" "$2" <<'PYEOF'
import json, sys
with open(sys.argv[1]) as fh:
    data = json.load(fh)
entry = (data.get("targets") or {}).get(sys.argv[2])
if not entry:
    sys.exit(1)
print(entry.get("url", ""), entry.get("sha256", ""))
PYEOF
    else
        die "python3 is required to read the release manifest"
    fi
}

# Install a downloaded payload (archive or raw binary) as herdr-core.
install_payload() { # payload
    src=""
    case "$1" in
        *.tar.gz|*.tgz)
            mkdir -p "$WORK/unpack"
            tar -xzf "$1" -C "$WORK/unpack" || die "cannot unpack $1"
            src=$(find "$WORK/unpack" -name herdr-core -type f | head -n 1)
            ;;
        *.tar)
            mkdir -p "$WORK/unpack"
            tar -xf "$1" -C "$WORK/unpack" || die "cannot unpack $1"
            src=$(find "$WORK/unpack" -name herdr-core -type f | head -n 1)
            ;;
        *)
            src=$1
            ;;
    esac
    [ -n "$src" ] && [ -f "$src" ] || die "payload holds no herdr-core binary"
    cp "$src" "$WORK/herdr-core.new"
    chmod +x "$WORK/herdr-core.new"
    mv "$WORK/herdr-core.new" "$BIN_DIR/herdr-core"
    ln -sf herdr-core "$BIN_DIR/hc"
    say "install: herdr-core installed to $BIN_DIR/herdr-core (hc symlink added)"
}

# ---------------------------------------------------------------- core
install_core() {
    if [ -n "$CORE_BIN" ]; then
        [ -f "$CORE_BIN" ] || die "--core-bin file not found: $CORE_BIN"
        say "install: installing herdr-core from local file $CORE_BIN"
        install_payload "$CORE_BIN"
    elif [ -n "$CORE_URL" ]; then
        say "install: downloading herdr-core from $CORE_URL"
        payload="$WORK/core-payload"
        download "$CORE_URL" "$payload" || die "download failed: $CORE_URL"
        [ -z "$CORE_SHA256" ] || check_sha256 "$payload" "$CORE_SHA256"
        # Archive-ness follows the URL suffix; a raw binary keeps its name.
        case "$CORE_URL" in
            *.tar.gz) mv "$payload" "$payload.tar.gz"; payload="$payload.tar.gz" ;;
            *.tgz)    mv "$payload" "$payload.tgz";    payload="$payload.tgz" ;;
            *.tar)    mv "$payload" "$payload.tar";    payload="$payload.tar" ;;
        esac
        install_payload "$payload"
    else
        target=$(detect_target)
        say "install: platform target: $target"
        say "install: fetching release manifest $MANIFEST_URL"
        if ! download "$MANIFEST_URL" "$WORK/core-latest.json"; then
            cat >&2 <<EOF
install: error: cannot download the release manifest:
  $MANIFEST_URL
The release manifest could not be downloaded from that URL, so the
default download path cannot work right now. Use one of the explicit
delivery paths instead:
  * build or download herdr-core on another machine, copy it to this
    host, and re-run:  install.sh --core-bin <file>
  * or host the binary/archive somewhere and re-run:
        install.sh --core-url <url> [--core-sha256 <hex>]
EOF
            exit 1
        fi
        entry=$(manifest_entry "$WORK/core-latest.json" "$target") \
            || die "release manifest has no build for target $target"
        url=${entry% *}
        sha=${entry##* }
        [ -n "$url" ] || die "release manifest entry for $target has no url"
        say "install: downloading $url"
        payload="$WORK/core-payload"
        download "$url" "$payload" || die "download failed: $url"
        [ -z "$sha" ] || [ "$sha" = "$url" ] || check_sha256 "$payload" "$sha"
        case "$url" in
            *.tar.gz) mv "$payload" "$payload.tar.gz"; payload="$payload.tar.gz" ;;
            *.tgz)    mv "$payload" "$payload.tgz";    payload="$payload.tgz" ;;
            *.tar)    mv "$payload" "$payload.tar";    payload="$payload.tar" ;;
        esac
        install_payload "$payload"
    fi
    say "install: self-check: $("$BIN_DIR/herdr-core" --version)"
}

# ------------------------------------------------------- session context
# Where the core listens is decided by the context that starts it: with
# a systemd user manager it listens under the runtime dir
# (/run/user/<uid>, the XDG_RUNTIME_DIR of every login session);
# without one it falls back to beside its state. An install driven
# from a non-login context (sudo -u, cron, cloud-init) before the
# first login therefore leaves a core that later logins cannot find --
# "installed but cannot connect". So: turn linger on (it keeps the
# user manager, and with it the runtime dir, alive with nobody logged
# in), point this script's own hc calls where logins look, and move a
# core that is already listening beside its state -- unless it still
# serves live terminals, which a move would end; then it stays in
# place with a warning, and this script talks to it where it is.
ensure_runtime_context() {
    [ "$(uname -s)" = "Linux" ] || return 0
    # Isolated sessions keep their own directories, and an explicit or
    # configured socket is the operator's choice: placements this step
    # must not second-guess.
    [ -z "${HC_SESSION:-}" ] || return 0
    [ -z "${HC_SOCKET:-}" ] || return 0
    uid=$(id -u)
    user=$(id -un)
    run_dir="/run/user/$uid"
    config_file="${XDG_CONFIG_HOME:-$HOME/.config}/herdr-core/config.toml"
    if [ -f "$config_file" ] && grep -q '^[[:space:]]*socket[[:space:]]*=' "$config_file"; then
        say "install: note: $config_file sets a socket; leaving its placement alone"
        return 0
    fi
    case "${XDG_RUNTIME_DIR:-}" in
        ""|"$run_dir") ;;
        *)
            say "install: note: XDG_RUNTIME_DIR is custom ($XDG_RUNTIME_DIR);"
            say "install:   leaving the core's socket placement to it"
            return 0
            ;;
    esac
    if ! command -v loginctl >/dev/null 2>&1; then
        say "install: note: no loginctl (no systemd user manager): the core listens"
        say "install:   beside its state, and every context here agrees on that place"
        return 0
    fi
    # Linger first: it is what makes the runtime dir exist at all when
    # nobody is logged in. Best effort -- enabling it can need an admin.
    if [ ! -e "/var/lib/systemd/linger/$user" ]; then
        if loginctl --no-ask-password enable-linger -- "$user" >/dev/null 2>&1 \
            && [ -e "/var/lib/systemd/linger/$user" ]; then
            say "install: enabled linger for $user, so the core outlives logins"
        else
            say "install: WARNING: could not enable linger for $user."
            say "install:   Without it the core may stop, or become unreachable, once"
            say "install:   no login session is left. An administrator can enable it with:"
            say "install:     sudo loginctl enable-linger $user"
        fi
    fi
    # Enabling linger starts the user manager, which creates the
    # runtime dir; give it a moment to appear.
    tries=0
    while [ ! -d "$run_dir" ] && [ "$tries" -lt 20 ]; do
        sleep 0.5
        tries=$((tries + 1))
    done
    if [ ! -d "$run_dir" ]; then
        say "install: WARNING: $run_dir does not exist (the user manager is not up),"
        say "install:   so the core will listen beside its state for now. Once the"
        say "install:   user manager runs (linger above, or a first login), re-run"
        say "install:   this script to move the core onto the runtime dir."
        return 0
    fi
    # From here on, this script's hc calls look where login sessions look.
    if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
        XDG_RUNTIME_DIR="$run_dir"
        export XDG_RUNTIME_DIR
    fi
    [ -x "$BIN_DIR/herdr-core" ] || return 0
    state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/herdr-core/default"
    lock="$state_dir/core.lock"
    [ -f "$lock" ] || return 0
    actual=$(cat "$lock" 2>/dev/null || true)
    want="$run_dir/herdr-core/default.sock"
    [ -n "$actual" ] || return 0
    [ "$actual" != "$want" ] || return 0
    [ -S "$actual" ] || return 0
    # A core is (or was) listening beside its state. Moving it ends the
    # terminals it serves -- their holders live beside the old socket --
    # so inspect it first and never move past live terminals. (The ls
    # talks to the core at its recorded socket; if nothing answers
    # there, leave the placement alone rather than guess.)
    listing=$(HC_SOCKET="$actual" "$BIN_DIR/herdr-core" ls 2>/dev/null) || listing=""
    if [ -z "$listing" ]; then
        say "install: WARNING: a core socket is recorded at $actual, where login"
        say "install:   sessions do not look ($want), but the core there did not"
        say "install:   answer an inspection. Leaving it in place; this script will"
        say "install:   talk to it where it is."
        HC_SOCKET="$actual"
        export HC_SOCKET
        return 0
    fi
    live=$(printf '%s' "$listing" | grep -c '"state": *"live"' || true)
    if [ "${live:-0}" -gt 0 ]; then
        say "install: WARNING: the running core listens at $actual,"
        say "install:   where login sessions do not look ($want), but it still serves"
        say "install:   $live live terminal(s), which a move would end. Leaving it in"
        say "install:   place; this script will talk to it where it is. To converge:"
        say "install:   end those terminals and re-run this script -- or move it by"
        say "install:   hand with"
        say "install:     HC_SOCKET=$actual $BIN_DIR/hc server stop"
        say "install:   and then re-run this script."
        HC_SOCKET="$actual"
        export HC_SOCKET
        return 0
    fi
    say "install: the running core listens at $actual,"
    say "install:   but login sessions look at $want; restarting the core onto it"
    HC_SOCKET="$actual" "$BIN_DIR/herdr-core" server stop >/dev/null 2>&1 || true
    # Start the new core the way a login session would see it: a
    # login-like environment (the whitelist scripts/restart-core.sh
    # uses), never this installer's possibly foreign one.
    if ! env -i \
        PATH="$PATH" HOME="$HOME" USER="$user" LOGNAME="$user" \
        ${SHELL:+"SHELL=$SHELL"} ${LANG:+"LANG=$LANG"} ${LC_ALL:+"LC_ALL=$LC_ALL"} \
        XDG_RUNTIME_DIR="$run_dir" \
        "$BIN_DIR/herdr-core" server start >/dev/null; then
        die "the core did not restart onto $want; start it with: XDG_RUNTIME_DIR=$run_dir $BIN_DIR/hc server start"
    fi
    say "install: core restarted; it now listens at $want"
}

# ---------------------------------------------------------------- pi
install_pi() {
    if command -v pi >/dev/null 2>&1; then
        say "install: pi already present: $(pi --version 2>&1 || echo '(version unknown)'); skipping"
        return 0
    fi
    command -v npm >/dev/null 2>&1 || die "pi is not installed and npm was not found on PATH; install Node.js/npm first, or re-run with --skip-pi"
    say "install: installing Pi via npm: $PI_PACKAGE"
    npm install -g "$PI_PACKAGE"
    command -v pi >/dev/null 2>&1 || die "npm install finished but pi is still not on PATH"
    say "install: pi installed: $(pi --version 2>&1 || echo '(version unknown)')"
}

# ---------------------------------------------------------------- space
precreate_space() {
    [ -f "$SCRIPT_DIR/precreate.py" ] || die "precreate.py not found next to install.sh ($SCRIPT_DIR)"
    command -v python3 >/dev/null 2>&1 || die "python3 is required for the pre-created space step (or re-run with --skip-space)"
    if [ -z "$SPACE_NAME" ]; then
        SPACE_NAME=$(hostname 2>/dev/null || echo fleet)
    fi
    say "install: pre-creating space '$SPACE_NAME' (terminal '$TERMINAL_NAME' running '$PI_CMD')"
    python3 "$SCRIPT_DIR/precreate.py" \
        --space-name "$SPACE_NAME" \
        --terminal-name "$TERMINAL_NAME" \
        --pi-cmd "$PI_CMD" \
        --folder "$HOME"
}

# ---------------------------------------------------------------- main
[ "$SKIP_CORE" -eq 1 ] && say "install: --skip-core: leaving herdr-core as-is" || install_core
ensure_runtime_context
[ "$SKIP_PI" -eq 1 ] && say "install: --skip-pi: leaving Pi as-is" || install_pi
[ "$SKIP_SPACE" -eq 1 ] && say "install: --skip-space: no space pre-created" || precreate_space
say "install: done."
say "install: note: no API key was written anywhere; configure Pi's provider credentials yourself."

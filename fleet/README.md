# fleet/ — OpenBot Fleet one-shot installer

Server-side install scripts for a Fleet worker VPS. Running `install.sh`
**on the VPS** sets up, entirely under `$HOME` and with no sudo:

1. **herdr-core** at `~/.local/bin/herdr-core`, plus an `hc` symlink.
2. **Pi** — the npm package `@mariozechner/pi-coding-agent` (body only).
3. **A pre-created herdr-app space** whose single terminal runs Pi, so the
   Herdr desktop app shows the machine with a named space and a live Pi
   terminal the moment it connects. The space is written with the exact
   sequence verified in Spike B2: create the terminal with no labels,
   write the `herdr-app` document (CAS-checked `doc.set`) referencing it,
   then label the terminal `herdr.space` / `herdr.tab` / `herdr.seen=0`.
   That logic lives in `precreate.py` (python3, standard library only).

## What it never does

- **It never writes an API key** — not into Pi's config, not anywhere else.
- **It never touches any `.env` file.**
- Nothing outside `$HOME`; no sudo; no system packages.

Configure Pi's provider credentials yourself after installing.

## Delivery path A: the VPS fetches everything itself

```sh
# on the VPS, from a checkout of this repo (fleet branch)
sh fleet/install.sh
```

By default herdr-core comes from the fork's GitHub Releases manifest
(`https://github.com/aimachy/herdr-core/releases/latest/download/core-latest.json`),
picked by platform target (`linux-x86_64`, `linux-aarch64`, `macos-aarch64`,
…) and verified against the manifest's sha256 before unpacking.

> **Current status:** the fork has not published a GitHub Release yet, so
> the default path fails today. The script says so plainly and points at
> the two explicit paths below — it never pretends to have installed
> anything. Once a release exists, this path works unchanged.

## Delivery path B: download elsewhere, upload, install from the file

```sh
# on your own computer: get a herdr-core binary for the VPS's platform,
# then upload the binary and this fleet/ directory
scp herdr-core install.sh precreate.py user@vps:~/
# on the VPS
sh install.sh --core-bin ~/herdr-core
```

Or host the binary/archive at any URL:

```sh
sh install.sh --core-url https://example.com/herdr-core.tar.gz \
              --core-sha256 <hex>   # optional but recommended
```

A `.tar.gz` / `.tgz` / `.tar` payload is unpacked and its `herdr-core`
taken; anything else is treated as the raw binary.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--core-bin <file>` | — | install this local herdr-core binary |
| `--core-url <url>` | — | download herdr-core from this URL |
| `--core-sha256 <hex>` | — | expected sha256 for `--core-url` |
| `--skip-core` | — | don't install/update herdr-core |
| `--pi-package <name>` | `@mariozechner/pi-coding-agent` | npm package installed as Pi |
| `--skip-pi` | — | don't install Pi |
| `--space-name <name>` | hostname | name of the pre-created space |
| `--terminal-name <n>` | `pi` | name of the pre-created terminal |
| `--pi-cmd <cmd>` | `pi` | command the pre-created terminal runs |
| `--skip-space` | — | don't pre-create the space |

Pi step behavior: if `pi` is already on `PATH`, its version is reported
and the step is skipped. Otherwise Pi is installed with
`npm install -g <package>`; with no `npm` on `PATH` the script stops with
a clear error (install Node.js first, or use `--skip-pi`).

## Login sessions, linger and the core's socket (Linux)

The core listens under the login runtime dir (`/run/user/<uid>`, the
`XDG_RUNTIME_DIR` of every ssh login) when a systemd user manager is
up, and beside its state (`~/.local/state/herdr-core/`) when it is not.
An install started from a non-login context (sudo -u, cron,
cloud-init) before the first login would otherwise leave a core that
ssh logins cannot find — installed, but impossible to connect to.
Right after installing the core, `install.sh` therefore:

- tries to enable systemd **linger** for the user, best effort — it is
  what keeps the user manager (and the runtime dir) alive with nobody
  logged in. If enabling fails, the script prints the exact
  `sudo loginctl enable-linger <user>` command instead of failing
  silently;
- points its own `hc` calls at the runtime dir, the way a login does;
- restarts a core that is already listening beside its state onto the
  runtime dir — **only while it serves no live terminals**: their
  holders live beside the old socket, and a move would end them. With
  live terminals the core stays where it is, the script talks to it
  there, and the warning it prints explains how to converge later.

If that restart (or a reboot) does end the pre-created space's
terminal, the next run repairs the space in place — see below.

## Idempotency and rollback

- Re-running `install.sh` is safe: an existing Pi is skipped, and
  `precreate.py` checks the `herdr-app` document first — if a space with
  the target name already exists and its terminal is live, it reports
  that and creates nothing. If that terminal has finished (or is gone),
  the space is repaired in place: a fresh terminal is created, the
  space's tab is repointed at it, and it is labelled into the space.
- herdr-core is simply replaced in place (same path, same symlink).
- If space pre-creation fails after its terminal was created, the
  terminal is killed and removed again: no half-created space, no orphan
  terminal.
- The script downloads into a temporary directory that is always cleaned
  up, on success and on failure alike.

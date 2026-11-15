#!/usr/bin/env python3
"""Pre-create a herdr-app space that adopts a fresh Pi terminal.

Sequence (verified by Spike B2; do not reorder the core steps):
  0. doc.get pre-check: if a space with the target name already exists,
     report and exit 0 without creating anything (idempotent re-runs).
  1. `hc run --bg` a terminal running the Pi command, with NO herdr.*
     labels, and learn its address.
  2. doc.get again (fresh CAS version) and re-check the name.
  3. doc.set a new space whose single tab's root block references that
     terminal.
  4. `hc label` the terminal with herdr.space / herdr.tab / herdr.seen.

Any failure after step 1 kills and removes the terminal again, so a
failed run leaves no half-created state behind.

Talks to the core only through the `hc` CLI: `hc connect` plain mode
(line-delimited JSON-RPC) for doc.get/doc.set, plain subcommands for
run/label/kill/rm. Standard library only. Never touches API keys or
any .env file.
"""
import argparse
import json
import os
import re
import secrets
import shutil
import subprocess
import sys

DOC_NAME = "herdr-app"


def find_hc(explicit):
    if explicit:
        return explicit
    env = os.environ.get("HC_BIN")
    if env:
        return env
    home = os.path.expanduser("~")
    for cand in (os.path.join(home, ".local", "bin", "hc"),
                 os.path.join(home, ".local", "bin", "herdr-core")):
        if os.path.isfile(cand) and os.access(cand, os.X_OK):
            return cand
    for name in ("hc", "herdr-core"):
        found = shutil.which(name)
        if found:
            return found
    return None


def run_hc(hc, args, check=True):
    proc = subprocess.run([hc] + args, capture_output=True, text=True)
    if check and proc.returncode != 0:
        raise RuntimeError(
            "hc %s failed (exit %d): %s" % (args[0], proc.returncode,
                                            (proc.stderr or proc.stdout).strip()))
    return proc


def terminal_address(run_stdout):
    """Extract the terminal address (t<hex>) from `hc run --json` output."""
    try:
        data = json.loads(run_stdout)
    except json.JSONDecodeError:
        data = None
    if isinstance(data, dict):
        for key in ("address", "terminal", "id", "name"):
            val = data.get(key)
            if isinstance(val, str) and re.fullmatch(r"t[0-9a-z]+", val):
                return val
        for val in data.values():
            if isinstance(val, str) and re.fullmatch(r"t[0-9a-z]+", val):
                return val
    match = re.search(r"\bt[0-9a-f]{1,8}\b", run_stdout)
    if match:
        return match.group(0)
    raise RuntimeError("could not parse terminal address from hc run output: %r"
                       % run_stdout.strip()[:200])


class DocRpc:
    """One persistent `hc connect` plain-mode session (see spike-b2)."""

    def __init__(self, hc):
        self.proc = subprocess.Popen([hc, "connect"], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, text=True, bufsize=1)
        self.rid = 0

    def call(self, method, params):
        self.rid += 1
        rid = self.rid
        self.proc.stdin.write(json.dumps(
            {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("hc connect closed during %s" % method)
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == rid:
                if "result" not in msg:
                    raise RuntimeError("%s failed: %s" % (method, json.dumps(msg)))
                return msg["result"]

    def get_doc(self):
        result = self.call("doc.get", {"name": DOC_NAME})
        value = result.get("value") or {"spaces": []}
        return value, result.get("version")

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        self.proc.terminate()


def space_named(value, name):
    for space in value.get("spaces", []):
        if space.get("name") == name:
            return space
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--hc", default=None, help="path to hc/herdr-core binary")
    ap.add_argument("--space-name", required=True)
    ap.add_argument("--terminal-name", default="pi")
    ap.add_argument("--pi-cmd", default="pi", help="command the terminal runs")
    ap.add_argument("--folder", default=os.path.expanduser("~"),
                    help="folder recorded on the space (default: $HOME)")
    args = ap.parse_args()

    hc = find_hc(args.hc)
    if not hc:
        print("precreate: no hc/herdr-core binary found "
              "(looked in ~/.local/bin and PATH)", file=sys.stderr)
        return 1

    rpc = DocRpc(hc)
    terminal = None
    created_space = None
    try:
        # Step 0: idempotency pre-check, before creating anything.
        value, _version = rpc.get_doc()
        existing = space_named(value, args.space_name)
        if existing:
            print("precreate: space %r already exists (id %s); nothing to do"
                  % (args.space_name, existing.get("id")))
            return 0

        # Step 1: bare terminal, no herdr.* labels yet.
        out = run_hc(hc, ["run", "--bg", "--json", "--name", args.terminal_name,
                          "--no-parent", "--no-notify", "--", args.pi_cmd])
        terminal = terminal_address(out.stdout)
        print("precreate: created terminal %s running %r" % (terminal, args.pi_cmd))

        # Steps 2-3: fresh doc read (CAS), then append the new space.
        value, version = rpc.get_doc()
        existing = space_named(value, args.space_name)
        if existing:
            raise RuntimeError("space %r appeared meanwhile (id %s); aborting"
                               % (args.space_name, existing.get("id")))
        sp = "sp" + secrets.token_hex(16)
        tb = "tb" + secrets.token_hex(16)
        pane = "p" + secrets.token_hex(16)
        new_space = {
            "folder": args.folder,
            "id": sp,
            "name": args.space_name,
            "tabs": [{
                "id": tb,
                "name": "",
                "root": {
                    "children": [{"content": {"terminal": terminal},
                                  "id": pane, "type": "block"}],
                    "id": tb,
                    "type": "row",
                },
            }],
        }
        new_value = dict(value)
        new_value["spaces"] = list(value.get("spaces", [])) + [new_space]
        rpc.call("doc.set", {"name": DOC_NAME, "version": version, "value": new_value})
        created_space = sp
        print("precreate: wrote space %s (%s) referencing %s" % (sp, args.space_name, terminal))

        # Step 4: only now label the terminal into the space.
        run_hc(hc, ["label", terminal,
                    "herdr.space=" + sp, "herdr.tab=" + tb, "herdr.seen=0"])
        print("precreate: labelled %s herdr.space=%s herdr.tab=%s herdr.seen=0"
              % (terminal, sp, tb))
        print("precreate: done: space %r (%s), terminal %s"
              % (args.space_name, sp, terminal))
        terminal = None  # success: nothing left to roll back
        return 0
    except Exception as exc:
        print("precreate: error: %s" % exc, file=sys.stderr)
        return 1
    finally:
        if terminal:
            print("precreate: rolling back terminal %s" % terminal, file=sys.stderr)
            run_hc(hc, ["kill", terminal], check=False)
            run_hc(hc, ["rm", terminal], check=False)
        if created_space and terminal:
            # doc.set already landed: remove the space again too, so no
            # half-created space is left pointing at a dead terminal.
            try:
                for _attempt in range(3):
                    value, version = rpc.get_doc()
                    kept = [s for s in value.get("spaces", [])
                            if s.get("id") != created_space]
                    if len(kept) == len(value.get("spaces", [])):
                        break
                    new_value = dict(value)
                    new_value["spaces"] = kept
                    try:
                        rpc.call("doc.set", {"name": DOC_NAME,
                                             "version": version, "value": new_value})
                        print("precreate: rolled back space %s" % created_space,
                              file=sys.stderr)
                        break
                    except RuntimeError:
                        continue  # CAS conflict: re-read and retry
            except Exception as rb_exc:
                print("precreate: warning: space rollback failed: %s" % rb_exc,
                      file=sys.stderr)
        rpc.close()


if __name__ == "__main__":
    sys.exit(main())

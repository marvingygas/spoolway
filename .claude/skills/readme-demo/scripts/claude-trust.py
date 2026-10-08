#!/usr/bin/env python3
"""Mark a directory trusted in Claude Code's ~/.claude.json, or remove it.

Usage: claude-trust.py add|remove <dir>

A Claude lane in an untrusted project stops on "Do you trust the files in
this folder?" with nobody there to answer, and spoolway reports it as
`agent_not_ready`. A task's worktree counts as part of its main checkout, so
trusting the checkout covers every lane. `remove` drops the whole entry,
which `add` created for the demo and nothing else uses.

The file is rewritten in one rename, so a running Claude Code never reads it
half written.
"""
import json
import os
import sys
import tempfile

action, directory = sys.argv[1], os.path.abspath(os.path.expanduser(sys.argv[2]))
path = os.path.expanduser("~/.claude.json")

with open(path) as f:
    config = json.load(f)
projects = config.setdefault("projects", {})

if action == "add":
    projects.setdefault(directory, {})["hasTrustDialogAccepted"] = True
elif action == "remove":
    if projects.pop(directory, None) is None:
        sys.exit(0)
else:
    sys.exit(f"unknown action {action!r}: use add or remove")

fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".claude.json.")
with os.fdopen(fd, "w") as f:
    json.dump(config, f, indent=2)
os.chmod(tmp, os.stat(path).st_mode & 0o777)
os.replace(tmp, path)

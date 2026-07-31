#!/usr/bin/env python3
import json
import os
import tempfile
import sys


def load_ids(path):
    with open(path, "r", encoding="utf-8") as handle:
        value = json.load(handle)
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise SystemExit(f"{path}: expected a JSON array of strings")
    return value


state_path, previous_declarative_path, declarative_path, enabled_path = sys.argv[1:5]
previous_declarative = set()
if os.path.exists(previous_declarative_path):
    previous_declarative = set(load_ids(previous_declarative_path))
declarative = set(load_ids(declarative_path))
enabled = set(load_ids(enabled_path))

current = []
if os.path.exists(state_path):
    with open(state_path, "r", encoding="utf-8") as handle:
        state = json.load(handle)
    if state.get("version") != 1 or not isinstance(state.get("enabled"), list):
        raise SystemExit(f"{state_path}: unsupported Lavis external module state")
    current = state["enabled"]
    if not all(isinstance(item, str) for item in current):
        raise SystemExit(f"{state_path}: enabled must be a JSON array of strings")

managed = previous_declarative | declarative
merged = sorted((set(current) - managed) | enabled)
directory = os.path.dirname(state_path)
os.makedirs(directory, mode=0o700, exist_ok=True)

temporary = None
try:
    fd, temporary = tempfile.mkstemp(
        prefix=".external-modules.json.nixos.",
        suffix=".tmp",
        dir=directory,
        text=True,
    )
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump({"version": 1, "enabled": merged}, handle, ensure_ascii=False, indent=2)
        handle.write("\n")
    os.chmod(temporary, 0o600)
    os.replace(temporary, state_path)
    temporary = None

    fd, temporary = tempfile.mkstemp(
        prefix=".declarative-modules.json.nixos.",
        suffix=".tmp",
        dir=directory,
        text=True,
    )
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump(sorted(declarative), handle, ensure_ascii=False, indent=2)
        handle.write("\n")
    os.chmod(temporary, 0o600)
    os.replace(temporary, previous_declarative_path)
    temporary = None
finally:
    if temporary is not None:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass

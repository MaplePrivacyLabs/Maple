"""Apply classes.py to every non-test source file and measure selected lines.

Usage: python3 -I measure.py ROOT inv.json out.json
"""
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import classes  # noqa: E402
import tslex  # noqa: E402

ROOT, INV, OUT = sys.argv[1:4]
inv = json.load(open(INV))


def members(path):
    out = subprocess.check_output([sys.executable, "-I", "-B", os.path.join(HERE, "members.py"), path]).decode()
    res = []
    for line in out.splitlines():
        m = re.match(r"\s*(\d+)-(\d+)\s+(\d+)\s+(\d+)\s+(.*)$", line)
        if m:
            res.append((int(m.group(1)), int(m.group(2)), m.group(5).split()[-1]))
    return res


def rng(spec):
    a, b = spec.split("-")
    return set(range(int(a), int(b) + 1))


def measure(rel, spec):
    full = os.path.join(ROOT, rel)
    text = open(full, encoding="utf-8").read()
    phys, code, per_line = tslex.analyze(text)
    if spec.get("keep"):
        lines = set()
        for r in spec["keep"]:
            lines |= rng(r)
        return phys, code, len(lines), sum(1 for i in lines if per_line[i - 1]), []
    cut_names = set(spec.get("cut", []))
    if not cut_names and not spec.get("ranges"):
        return phys, code, phys, code, []
    mem = members(full)
    member_lines = set()
    cut_lines = set()
    found = set()
    for s, e, name in mem:
        member_lines |= set(range(s, e + 1))
        if name in cut_names:
            cut_lines |= set(range(s, e + 1))
            found.add(name)
    for r in spec.get("ranges", []):
        cut_lines |= rng(r)
    missing = sorted(cut_names - found)
    member_code = sum(1 for i in member_lines if per_line[i - 1])
    header_code = sum(1 for i, h in enumerate(per_line, 1) if h and i not in member_lines)
    kept_member_code = member_code - sum(1 for i in cut_lines & member_lines if per_line[i - 1])
    cut_nonmember = sum(1 for i in cut_lines - member_lines if per_line[i - 1])
    share = kept_member_code / member_code if member_code else 1
    kept_code = kept_member_code + (header_code - cut_nonmember) * share
    return phys, code, phys - len(cut_lines), round(kept_code), missing


def exclude_reason(rel):
    if classes.EXCLUDE_OVERRIDES.get(rel):
        return classes.EXCLUDE_OVERRIDES[rel]
    for rx, reason in classes.EXCLUDE_RULES:
        if re.search(rx, rel):
            if reason == "API:":
                base = os.path.basename(rel)[:-3]
                return f"{classes.API_NAMES.get(base, base)}; inference goes only through Maple's chat-completions backend" if not base.startswith("openai-responses") and "Copilot" not in classes.API_NAMES.get(base, "") and "registry" not in classes.API_NAMES.get(base, "") else classes.API_NAMES[base]
            if reason == "UTILS":
                base = os.path.basename(rel)[:-3]
                return classes.UTIL_REASONS[base]
            return reason
    return None


rows = []
unmatched = []
for rel, meta in sorted(inv.items()):
    if meta["test"] or meta["pkg"] not in ("agent", "ai", "coding-agent"):
        continue
    if rel in classes.SELECTED:
        spec = classes.SELECTED[rel]
        phys, code, sp, sc, missing = measure(rel, spec)
        rows.append(dict(path=rel, pkg=meta["pkg"], cls=spec["cls"], crate=spec["crate"], rust_module=spec["rust_module"],
                         adaptations=spec["adaptations"], reason=spec["reason"],
                         phys=phys, code=code, sel_phys=sp, sel_code=sc, generated=meta["generated"], missing=missing))
    else:
        reason = exclude_reason(rel)
        if reason is None:
            unmatched.append(rel)
            reason = "?"
        rows.append(dict(path=rel, pkg=meta["pkg"], cls="Exclude", crate="", reason=reason, phys=meta["phys"],
                         code=meta["code"], sel_phys=0, sel_code=0, generated=meta["generated"], missing=[]))
# selected files from other packages
for rel, spec in classes.SELECTED.items():
    meta = inv.get(rel)
    if meta and meta["pkg"] not in ("agent", "ai", "coding-agent"):
        phys, code, sp, sc, missing = measure(rel, spec)
        rows.append(dict(path=rel, pkg=meta["pkg"], cls=spec["cls"], crate=spec["crate"], rust_module=spec["rust_module"],
                         adaptations=spec["adaptations"], reason=spec["reason"],
                         phys=phys, code=code, sel_phys=sp, sel_code=sc, generated=False, missing=missing))
json.dump(dict(rows=rows, unmatched=unmatched), open(OUT, "w"), indent=1)
print("rows", len(rows), "unmatched", len(unmatched))
for u in unmatched:
    print("  UNMATCHED", u)
for r in rows:
    if r["missing"]:
        print("  MISSING CUT NAMES", r["path"], r["missing"])
from collections import defaultdict
tot = defaultdict(lambda: [0, 0, 0, 0, 0])
for r in rows:
    k = (r["crate"] or "-", r["cls"])
    t = tot[k]
    t[0] += 1
    t[1] += r["phys"]
    t[2] += r["code"]
    t[3] += r["sel_phys"]
    t[4] += r["sel_code"]
for k in sorted(tot):
    print(k, tot[k])

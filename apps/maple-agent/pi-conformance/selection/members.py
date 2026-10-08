"""List top-level declarations and class members with line ranges and code
lines, using brace depth from the lexer (comments and strings ignored).

Usage: python3 -I members.py FILE
A declaration starts on a line at brace depth 0 (top level) or at depth 1
inside a class body (members). Its range starts at its leading comment
block and ends just before the next declaration's leading comment block,
or at the closing brace of the enclosing class.
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tslex  # noqa: E402

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
lines = text.split("\n")
if lines and lines[-1] == "":
    lines = lines[:-1]
_, _, per_line = tslex.analyze(text)
clean = tslex.strip_comments_and_strings(text).split("\n")

TOP = re.compile(r"^(export\s+)?(default\s+)?(declare\s+)?(async\s+)?(abstract\s+)?(function\*?|class|const|let|var|type|interface|enum)\s+([\w$]+)")
MEM = re.compile(r"^\s*(?:(?:private|public|protected|static|async|readonly|override|abstract|declare|get|set)\s+)*(#?[\w$]+)\s*(?:[(<:=?!;]|$)")

depth = 0
starts = []  # (line_index, kind, name, extra)
class_depth = None  # depth of class body when inside a top-level class
for i, l in enumerate(lines):
    d0 = depth
    c = clean[i] if i < len(clean) else ""
    if d0 == 0:
        m = TOP.match(l)
        if m:
            starts.append((i, "top", m.group(7), m.group(6)))
            class_depth = 1 if m.group(6) == "class" else None
    elif class_depth is not None and d0 == class_depth and per_line[i]:
        m = MEM.match(l)
        if m and l.startswith("\t") and not l.startswith("\t\t") and m.group(1) not in ("}", ")"):
            starts.append((i, "mem", m.group(1), ""))
    for ch in c:
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
    if class_depth is not None and d0 >= 1 and depth == 0:
        starts.append((i, "end", "}", ""))
        class_depth = None


def lead(i):
    j = i
    while j > 0 and not per_line[j - 1] and lines[j - 1].strip() != "" and lines[j - 1].strip().startswith(("/", "*")):
        j -= 1
    return j


marks = []
for idx, (i, kind, name, extra) in enumerate(starts):
    if kind == "end":
        continue
    nxt = len(lines)
    for (k, kind2, _, _) in starts[idx + 1:]:
        nxt = lead(k) if kind2 != "end" else k
        break
    s = lead(i)
    marks.append((s + 1, nxt, kind, name, extra))

for s, e, kind, name, extra in marks:
    code = sum(1 for x in per_line[s - 1:e] if x)
    tag = extra if kind == "top" else "  ."
    print(f"{s:5}-{e:<5} {e - s + 1:5} {code:5}  {tag} {name}")

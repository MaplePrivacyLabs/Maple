"""Count it()/test() calls per top-level describe block.

Usage: python3 -I describes.py FILE...
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tslex  # noqa: E402

TEST_CALL = re.compile(r"(?<![\w$.])(it|test)((?:\.(?:only|concurrent|sequential|fails|skipIf\([^)]*\)|runIf\([^)]*\)))*)\s*\(")
DESC = re.compile(r"^\t?(describe(?:\.\w+(?:\([^)]*\))?)?)\s*\(\s*(['\"`])(.*?)\2", re.M)

for path in sys.argv[1:]:
    text = open(path, encoding="utf-8").read()
    clean = tslex.strip_comments(text)
    print("==", os.path.basename(path), "total", len(TEST_CALL.findall(clean)))
    for m in DESC.finditer(clean):
        k = clean.find("{", clean.find("=>", m.end()))
        depth = 0
        end = k
        for idx in range(k, len(clean)):
            ch = clean[idx]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    end = idx
                    break
        line = clean.count("\n", 0, m.start()) + 1
        print(f"   {line:5} {len(TEST_CALL.findall(clean[k:end])):4}  {m.group(1)} {m.group(3)[:70]}")

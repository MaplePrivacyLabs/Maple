"""Inventory of Pi v1.0.4 sources: lines, code lines, imports, exports.

Usage: python3 -I inv.py <pi-root> <out.json>
Reads only. Writes one JSON file.
"""
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tslex  # noqa: E402

ROOT = sys.argv[1]
OUT = sys.argv[2]
WITH_TESTS = "--with-tests" in sys.argv

PKG_DIRS = {
    "@earendil-works/pi-ai": "ai",
    "@earendil-works/pi-agent-core": "agent",
    "@earendil-works/pi-coding-agent": "coding-agent",
    "@earendil-works/pi-tui": "tui",
    "@earendil-works/pi-mcp": "mcp",
    "@earendil-works/pi-codemode": "codemode",
    "@earendil-works/pi-telemetry": "telemetry",
    "@earendil-works/chord": "chord",
    "@earendil-works/pi-durable": "durable",
    "@earendil-works/pi-env": "env",
    "@earendil-works/pi-protocol": "protocol",
    "@earendil-works/pi-client": "client",
    "@earendil-works/pi-server": "server",
}


def load_exports_map(pkgdir):
    pj = json.load(open(os.path.join(ROOT, "packages", pkgdir, "package.json")))
    return pj.get("exports") or {}


EXPORTS = {d: load_exports_map(d) for d in PKG_DIRS.values()}


def resolve_pkg(spec):
    for name, d in sorted(PKG_DIRS.items(), key=lambda kv: -len(kv[0])):
        if spec == name or spec.startswith(name + "/"):
            sub = spec[len(name):]  # "" or "/x"
            key = "." + sub
            ex = EXPORTS[d]
            target = None
            if key in ex:
                v = ex[key]
                if isinstance(v, dict):
                    target = v.get("source") or v.get("import")
                else:
                    target = v
            else:
                for k, v in ex.items():
                    if "*" in k:
                        pre, post = k.split("*", 1)
                        if key.startswith(pre) and key.endswith(post):
                            star = key[len(pre): len(key) - len(post) if post else None]
                            tv = v.get("source") or v.get("import") if isinstance(v, dict) else v
                            target = tv.replace("*", star)
                            break
            if target is None and not ex:
                target = "./src/index.ts" if sub == "" else "./src" + sub + ".ts"
            if target is None:
                return ("unresolved-pkg", spec)
            t = target
            if t.startswith("./dist/"):
                t = "./src/" + t[len("./dist/"):]
            if t.endswith(".js"):
                t = t[:-3] + ".ts"
            p = os.path.normpath(os.path.join("packages", d, t))
            if os.path.exists(os.path.join(ROOT, p)):
                return ("file", p)
            return ("unresolved-pkg", spec + " -> " + p)
    return None


def resolve(spec, from_file):
    if spec.startswith("."):
        base = os.path.normpath(os.path.join(os.path.dirname(from_file), spec))
        cands = [base]
        if base.endswith(".js"):
            cands.insert(0, base[:-3] + ".ts")
        cands += [base + ".ts", os.path.join(base, "index.ts")]
        for c in cands:
            if os.path.isfile(os.path.join(ROOT, c)):
                return ("file", c)
        return ("missing", base)
    if spec.startswith("node:"):
        return ("node", spec)
    r = resolve_pkg(spec)
    if r:
        return r
    NODE_BUILTINS = {"fs", "path", "os", "child_process", "crypto", "url", "util", "events",
                     "stream", "http", "https", "net", "tls", "zlib", "readline", "buffer",
                     "worker_threads", "module", "process", "assert", "timers", "fs/promises",
                     "stream/promises", "timers/promises", "v8", "vm", "dns", "querystring",
                     "string_decoder", "perf_hooks", "async_hooks", "tty"}
    if spec in NODE_BUILTINS:
        return ("node", "node:" + spec)
    # npm package name
    parts = spec.split("/")
    name = "/".join(parts[:2]) if spec.startswith("@") else parts[0]
    return ("npm", name, spec)


IMPORT_RE = re.compile(
    r"""(?P<kw>\bimport|\bexport)\s+(?P<type>type\s+)?(?P<clause>[^;'"`]*?)\s*\bfrom\s*(?P<q>['"])(?P<spec>[^'"]+)(?P=q)""",
    re.S,
)
SIDE_RE = re.compile(r"""\bimport\s*(?P<q>['"])(?P<spec>[^'"]+)(?P=q)""")
DYN_RE = re.compile(r"""\bimport\s*\(\s*(?P<q>['"`])(?P<spec>[^'"`]+)(?P=q)\s*\)""")
REQ_RE = re.compile(r"""\brequire(?:\.resolve)?\s*\(\s*(?P<q>['"])(?P<spec>[^'"]+)(?P=q)\s*\)""")


def parse_clause(kw, clause, is_type):
    """Return list of (imported_name, local_name, type_only) or special markers."""
    clause = clause.strip()
    names = []
    if kw == "export":
        if clause.startswith("*"):
            m = re.match(r"\*\s*(?:as\s+(\w+))?", clause)
            if m and m.group(1):
                names.append(("*ns", m.group(1), is_type))
            else:
                names.append(("*", "*", is_type))
            return names
    m = re.match(r"^(?P<def>[\w$]+)?\s*,?\s*(?P<rest>.*)$", clause, re.S)
    rest = clause
    if m and m.group("def") and m.group("def") not in ("type",):
        names.append(("default", m.group("def"), is_type))
        rest = m.group("rest")
    rest = rest.strip()
    if rest.startswith("* as"):
        ns = rest[4:].strip()
        names.append(("*ns", ns, is_type))
    elif rest.startswith("{"):
        inner = rest[1: rest.rfind("}")]
        for part in inner.split(","):
            part = part.strip()
            if not part:
                continue
            t = is_type
            if part.startswith("type "):
                t = True
                part = part[5:].strip()
            if " as " in part:
                a, b = [x.strip() for x in part.split(" as ", 1)]
            else:
                a = b = part
            names.append((a, b, t))
    return names


EXPORT_DECL_RE = re.compile(
    r"^\s*export\s+(?:declare\s+)?(?:default\s+)?(?:async\s+)?(?:abstract\s+)?"
    r"(?P<kind>function\*?|class|const|let|var|type|interface|enum|namespace)\s+(?P<name>[\w$]+)",
    re.M,
)
EXPORT_LIST_RE = re.compile(r"^\s*export\s+(?P<type>type\s+)?\{(?P<inner>[^}]*)\}\s*;?\s*$", re.M)
EXPORT_DESTRUCT_RE = re.compile(r"^\s*export\s+const\s+\{(?P<inner>[^}]*)\}\s*=", re.M)


def own_exports(stripped):
    names = {}
    for m in EXPORT_DECL_RE.finditer(stripped):
        names[m.group("name")] = m.group("kind")
    for m in EXPORT_LIST_RE.finditer(stripped):
        for part in m.group("inner").split(","):
            part = part.strip()
            if not part:
                continue
            if part.startswith("type "):
                part = part[5:].strip()
            if " as " in part:
                a, b = [x.strip() for x in part.split(" as ", 1)]
            else:
                a = b = part
            names[b] = "local:" + a
    for m in EXPORT_DESTRUCT_RE.finditer(stripped):
        for part in m.group("inner").split(","):
            part = part.strip().split(":")[-1].strip()
            if part:
                names[part] = "const"
    if re.search(r"^\s*export\s+default\b", stripped, re.M):
        names.setdefault("default", "default")
    return names


def line_of(text, idx):
    return text.count("\n", 0, idx) + 1


def is_generated(rel, text):
    head = "\n".join(text.splitlines()[:3]).lower()
    return rel.endswith(".generated.ts") or "auto-generated" in head or "do not edit manually" in head


def main():
    files = {}
    roots = []
    for pkg in os.listdir(os.path.join(ROOT, "packages")):
        roots.append((pkg, os.path.join(ROOT, "packages", pkg, "src")))
        if WITH_TESTS and pkg in ("agent", "ai", "coding-agent"):
            roots.append((pkg, os.path.join(ROOT, "packages", pkg, "test")))
    for pkg, srcdir in roots:
        if not os.path.isdir(srcdir):
            continue
        for dp, dn, fn in os.walk(srcdir):
            for f in fn:
                if not f.endswith(".ts"):
                    continue
                full = os.path.join(dp, f)
                rel = os.path.relpath(full, ROOT)
                text = open(full, encoding="utf-8").read()
                phys, code, per_line = tslex.analyze(text)
                if phys != len(per_line):
                    print("WARN line mapping", rel, phys, len(per_line), file=sys.stderr)
                stripped = tslex.strip_comments(text)
                code_only = tslex.strip_comments_and_strings(text)
                imports = []
                spans = []
                for m in IMPORT_RE.finditer(stripped):
                    # A command or prose string containing "import" is not an import.
                    if code_only[m.start():m.start() + 6] != stripped[m.start():m.start() + 6]:
                        continue
                    # guard: clause must not contain statements
                    clause = m.group("clause")
                    kw = m.group("kw")
                    if kw == "import" and clause.strip().startswith("("):
                        continue
                    if kw == "export" and not (clause.strip().startswith("{") or clause.strip().startswith("*")):
                        continue
                    if kw == "import" and not (clause.strip().startswith("{") or clause.strip().startswith("*") or re.match(r"^[\w$]+", clause.strip())):
                        continue
                    spans.append((m.start(), m.end()))
                    spec = m.group("spec")
                    imports.append({
                        "kind": kw + ("-type" if m.group("type") else ""),
                        "spec": spec,
                        "line": line_of(text, m.start()),
                        "names": parse_clause(kw, clause, bool(m.group("type"))),
                        "res": resolve(spec, rel),
                    })
                for m in SIDE_RE.finditer(stripped):
                    # A command or prose string containing "import" is not an import.
                    if code_only[m.start():m.start() + 6] != stripped[m.start():m.start() + 6]:
                        continue
                    if any(a <= m.start() < b for a, b in spans):
                        continue
                    imports.append({"kind": "side", "spec": m.group("spec"), "line": line_of(text, m.start()),
                                    "names": [], "res": resolve(m.group("spec"), rel)})
                for m in DYN_RE.finditer(stripped):
                    # A command or prose string containing "import" is not an import.
                    if code_only[m.start():m.start() + 6] != stripped[m.start():m.start() + 6]:
                        continue
                    imports.append({"kind": "dynamic", "spec": m.group("spec"), "line": line_of(text, m.start()),
                                    "names": [], "res": resolve(m.group("spec"), rel)})
                for m in REQ_RE.finditer(stripped):
                    # A command or prose string containing "import" is not an import.
                    if code_only[m.start():m.start() + 6] != stripped[m.start():m.start() + 6]:
                        continue
                    imports.append({"kind": "require", "spec": m.group("spec"), "line": line_of(text, m.start()),
                                    "names": [], "res": resolve(m.group("spec"), rel)})
                rel_in_pkg = os.path.relpath(full, os.path.join(ROOT, "packages", pkg))
                files[rel] = {
                    "pkg": pkg,
                    "phys": phys,
                    "code": code,
                    "generated": is_generated(rel, text),
                    "test": rel_in_pkg.startswith("test/") or f.endswith(".test.ts") or f.endswith(".spec.ts"),
                    "imports": imports,
                    "exports": own_exports(stripped),
                }
    json.dump(files, open(OUT, "w"), indent=1, sort_keys=True)
    print("files", len(files))


main()

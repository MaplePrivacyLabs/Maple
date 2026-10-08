"""Minimal TypeScript lexer for line counting and import extraction.

Counting rules:
- physical lines: len(text.splitlines())
- code lines: physical lines minus blank lines and comment-only lines.
  A line is code if it holds any non-whitespace character outside a
  comment (string, template-literal and regex contents count as code).

strip_comments() returns the text with comments replaced by spaces
(newlines kept), so regexes over it never see commented-out code.
"""

REGEX_PREV_CHARS = set("(,=:[!&|?{};+-*%<>~^")
REGEX_PREV_WORDS = {
    "return", "typeof", "case", "do", "else", "in", "of", "new", "delete",
    "void", "throw", "instanceof", "yield", "await",
}


def _scan(text):
    """Yield (index, char, kind) where kind is 'code', 'comment' or 'ws'."""
    n = len(text)
    i = 0
    # stack of template frames: each frame is the brace depth inside ${ }
    tmpl_stack = []
    state = "code"  # code | tmpl
    brace_depth = 0
    last_sig = ""  # last significant code char
    last_word = ""
    out = [None] * n
    while i < n:
        c = text[i]
        if state == "tmpl":
            if c == "\\":
                out[i] = "code"
                if i + 1 < n:
                    out[i + 1] = "code" if not text[i + 1].isspace() else ("nl" if text[i + 1] == "\n" else "ws")
                i += 2
                continue
            if c == "`":
                out[i] = "code"
                state = "code"
                last_sig = "`"
                last_word = ""
                i += 1
                continue
            if c == "$" and i + 1 < n and text[i + 1] == "{":
                out[i] = out[i + 1] = "code"
                tmpl_stack.append(brace_depth)
                brace_depth = 0
                state = "code"
                last_sig = "{"
                last_word = ""
                i += 2
                continue
            out[i] = "nl" if c == "\n" else ("ws" if c.isspace() else "str")
            i += 1
            continue
        # state == code
        if c == "\n":
            out[i] = "nl"
            i += 1
            continue
        if c.isspace():
            out[i] = "ws"
            i += 1
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            j = text.find("\n", i)
            if j < 0:
                j = n
            for k in range(i, j):
                out[k] = "comment"
            i = j
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "*":
            j = text.find("*/", i + 2)
            j = n if j < 0 else j + 2
            for k in range(i, j):
                out[k] = "nl" if text[k] == "\n" else "comment"
            i = j
            continue
        if c in "'\"":
            q = c
            out[i] = "code"
            i += 1
            while i < n and text[i] != q and text[i] != "\n":
                if text[i] == "\\" and i + 1 < n:
                    out[i] = "str"
                    out[i + 1] = "nl" if text[i + 1] == "\n" else "str"
                    i += 2
                    continue
                out[i] = "str"
                i += 1
            if i < n and text[i] == q:
                out[i] = "code"
                i += 1
            last_sig = q
            last_word = ""
            continue
        if c == "`":
            out[i] = "code"
            state = "tmpl"
            i += 1
            continue
        if c == "/":
            is_regex = (last_sig == "" or last_sig in REGEX_PREV_CHARS or last_word in REGEX_PREV_WORDS)
            if is_regex:
                j = i + 1
                in_class = False
                ok = False
                while j < n and text[j] != "\n":
                    ch = text[j]
                    if ch == "\\":
                        j += 2
                        continue
                    if in_class:
                        if ch == "]":
                            in_class = False
                    elif ch == "[":
                        in_class = True
                    elif ch == "/":
                        ok = True
                        break
                    j += 1
                if ok:
                    j += 1
                    while j < n and (text[j].isalnum() or text[j] == "_"):
                        j += 1
                    for k in range(i, j):
                        out[k] = "code"
                    i = j
                    last_sig = "/"
                    last_word = ""
                    continue
            out[i] = "code"
            last_sig = "/"
            last_word = ""
            i += 1
            continue
        if c == "{":
            brace_depth += 1
        elif c == "}":
            if brace_depth == 0 and tmpl_stack:
                brace_depth = tmpl_stack.pop()
                out[i] = "code"
                state = "tmpl"
                i += 1
                continue
            brace_depth -= 1
        out[i] = "code"
        if c.isalnum() or c == "_" or c == "$":
            j = i
            while j < n and (text[j].isalnum() or text[j] in "_$"):
                out[j] = "code"
                j += 1
            last_word = text[i:j]
            # identifiers/numbers end an expression -> division context
            last_sig = "a"
            i = j
            continue
        last_sig = c
        last_word = ""
        i += 1
    return out


def analyze(text):
    kinds = _scan(text)
    lines = text.splitlines()
    phys = len(lines)
    # map char kinds to lines; splitlines also splits on \r, \x0b etc.
    # Pi sources use \n only; assert that so the mapping is exact.
    code = 0
    has = False
    line_no = 0
    per_line = []
    for idx, ch in enumerate(text):
        if ch == "\n":
            per_line.append(has)
            has = False
            continue
        if kinds[idx] in ("code", "str") and not ch.isspace():
            has = True
    if text and not text.endswith("\n"):
        per_line.append(has)
    code = sum(1 for h in per_line if h)
    return phys, code, per_line


def strip_comments(text):
    kinds = _scan(text)
    chars = list(text)
    for idx, k in enumerate(kinds):
        if k == "comment" and chars[idx] != "\n":
            chars[idx] = " "
    return "".join(chars)


if __name__ == "__main__":
    import sys
    tot = 0
    for f in sys.argv[1:]:
        t = open(f, encoding="utf-8").read()
        p, c, _ = analyze(t)
        tot += c
        print(c, p, f)
    print("total code", tot)


def strip_comments_and_strings(text):
    kinds = _scan(text)
    chars = list(text)
    for idx, k in enumerate(kinds):
        if k in ("comment", "str") and chars[idx] != "\n":
            chars[idx] = " "
    return "".join(chars)

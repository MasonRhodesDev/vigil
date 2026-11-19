import argparse
import collections
import datetime
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import urllib.parse

BASELINE = "tests/gates/baseline.tsv"
LABEL = "owner-approved"
CRATES = "crates/"
COUNTED = ("unsafe", "lint-allow", "expose")
HORIZON_DAYS = 183

WORDS = {"unsafe": re.compile(rb"\bunsafe\b"), "expose": re.compile(rb"\bexpose\b")}
LINT_LEVEL = re.compile(rb"\b(?:allow|expect|warn)\s*\(")
WEAKENED_LINT = re.compile(rb"unsafe|clippy|\bwarnings\b")
CFG = re.compile(rb"\bcfg(?:_attr)?\s*!?\s*\(")
CFG_CLIPPY = re.compile(rb"\bclippy\b(?!\s*::)")
INCLUDE = re.compile(rb"\binclude\s*!|::\s*include\b|\binclude\s+as\b")
PATH_ATTRIBUTE = re.compile(rb"#\s*!?\s*\[\s*path\b")
CFG_ATTR = re.compile(rb"\bcfg_attr\s*\(")
PATH_KEY = re.compile(rb"\bpath\s*=")

LINT_CONFIG_NAMES = {"clippy.toml", ".clippy.toml", "rust-toolchain", "rust-toolchain.toml"}
LINT_CONFIG_ALLOWED = {"clippy.toml"}
ROOT_ATTRIBUTES = {"forbid": "#![forbid(unsafe_code)]", "deny": "#![deny(unsafe_code)]"}
REQUIRED_LINTS = {
    "rust": ["unsafe_code", "unsafe_op_in_unsafe_fn"],
    "clippy": [
        "undocumented_unsafe_blocks",
        "multiple_unsafe_ops_per_block",
        "disallowed_methods",
        "disallowed_types",
    ],
}
EXPOSE_BAN = "vigil_secret::Secret::expose"
ADVISORY_ID = re.compile(r"(?:RUSTSEC-\d{4}-\d{4}|GHSA(?:-[23456789cfghjmpqrvwx]{4}){3})")
ADVISORY_REASON = re.compile(r"expires (\d{4}-\d{2}-\d{2}): \S")

CANARY = ["cargo", "clippy", "--quiet", "-p", "vigil-secret", "--example", "lint_canary", "--features", "lint-canary", "--message-format=json"]
CANARY_LINTS = [
    "unsafe_code",
    "E0133",
    "clippy::undocumented_unsafe_blocks",
    "clippy::multiple_unsafe_ops_per_block",
    "clippy::disallowed_methods",
]

GUARDED = re.compile(
    r"tests/gates/(?!baseline\.tsv$).+"
    r"|tests/check[^/]*\.sh"
    r"|\.github/.+"
    r"|Cargo\.toml|clippy\.toml|deny\.toml"
    r"|(?:.+/)?(?:\.clippy\.toml|clippy\.toml|rust-toolchain(?:\.toml)?)"
    r"|(?:.+/)?\.cargo/.+"
    r"|crates/vigil-secret/.+"
)


class Findings:
    def __init__(self):
        self.counts = collections.Counter()
        self.details = collections.defaultdict(list)

    def add(self, path, rule, detail, weight=1):
        self.counts[path, rule] += weight
        self.details[path, rule].append(detail)


class Unlexable(Exception):
    pass


def die(message):
    sys.exit(f"gate: {message}")


def run(argv, **kwargs):
    return subprocess.run(argv, capture_output=True, text=True, check=False, **kwargs)


def read_bytes(path):
    with open(path, "rb") as f:
        return f.read()


def listed():
    result = run(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"])
    if result.returncode != 0:
        die(f"git ls-files: {result.stderr.strip()}")
    return [p for p in result.stdout.split("\0")[:-1] if os.path.lexists(p)]


def balanced(text, start, open_char, close_char):
    depth, i = 0, start
    while i < len(text):
        if text[i] == open_char:
            depth += 1
        elif text[i] == close_char:
            depth -= 1
            if depth == 0:
                return text[start : i + 1]
        i += 1
    return text[start:]


def groups(pattern, text):
    for match in pattern.finditer(text):
        yield balanced(text, match.end() - 1, ord("("), ord(")"))


def ident_char(c):
    return c.isalnum() or c == "_"


def string_end(text, i):
    while i < len(text):
        if text[i] == "\\":
            i += 2
        elif text[i] == '"':
            return i + 1
        else:
            i += 1
    raise Unlexable("unterminated string")


def raw_string_end(text, i):
    hashes = 0
    while i < len(text) and text[i] == "#":
        hashes, i = hashes + 1, i + 1
    if i >= len(text) or text[i] != '"':
        raise Unlexable("malformed raw string")
    end = text.find('"' + "#" * hashes, i + 1)
    if end < 0:
        raise Unlexable("unterminated raw string")
    return end + 1 + hashes


def char_end(text, i):
    if text[i] == "\\":
        end = text.find("'", i + 2)
    else:
        end = i + 1 if i + 1 < len(text) and text[i + 1] == "'" else -1
    if end < 0:
        raise Unlexable("unterminated character literal")
    return end + 1


def lex(text):
    tokens, i, n = [], 0, len(text)
    while i < n:
        c = text[i]
        if c.isspace():
            i += 1
        elif text.startswith("//", i):
            end = text.find("\n", i)
            i = n if end < 0 else end
        elif text.startswith("/*", i):
            depth, i = 1, i + 2
            while depth:
                if i >= n:
                    raise Unlexable("unterminated block comment")
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        elif c == '"':
            i = string_end(text, i + 1)
        elif c == "'":
            if i + 1 < n and (text[i + 1] == "\\" or (i + 2 < n and text[i + 2] == "'")):
                i = char_end(text, i + 1)
            else:
                i += 1
                if text.startswith("r#", i):
                    i += 2
                while i < n and ident_char(text[i]):
                    i += 1
        elif c.isdigit():
            while i < n and ident_char(text[i]):
                i += 1
            if text.startswith(".", i) and i + 1 < n and text[i + 1].isdigit():
                i += 1
                while i < n and ident_char(text[i]):
                    i += 1
        elif ident_char(c):
            j = i
            while j < n and ident_char(text[j]):
                j += 1
            word, after = text[i:j], text[j : j + 1]
            if word == "r" and after == "#" and j + 1 < n and ident_char(text[j + 1]):
                k = j + 1
                while k < n and ident_char(text[k]):
                    k += 1
                tokens.append(text[j + 1 : k])
                i = k
            elif word in ("r", "br", "cr") and after in ('"', "#"):
                i = raw_string_end(text, j)
            elif word in ("b", "c") and after == '"':
                i = string_end(text, j + 1)
            elif word == "b" and after == "'":
                i = char_end(text, j + 1)
            else:
                tokens.append(word)
                i = j
        else:
            tokens.append(c)
            i += 1
    return tokens


def group_end(tokens, start):
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = []
    for i in range(start, len(tokens)):
        if tokens[i] in pairs:
            stack.append(pairs[tokens[i]])
        elif stack and tokens[i] == stack[-1]:
            stack.pop()
            if not stack:
                return i
    return len(tokens) - 1


def attributes(tokens):
    for i, token in enumerate(tokens):
        if token != "#":
            continue
        j = i + 2 if tokens[i + 1 : i + 2] == ["!"] else i + 1
        if tokens[j : j + 1] == ["["]:
            yield tokens[j : group_end(tokens, j) + 1]


def macro_groups(tokens):
    for i, token in enumerate(tokens):
        if tokens[i + 1 : i + 2] != ["!"] or not ident_char(token[0]):
            continue
        start = i + 3 if token == "macro_rules" else i + 2
        if tokens[start : start + 1] and tokens[start] in ("(", "[", "{"):
            yield token, tokens[start : group_end(tokens, start) + 1]


def weakening_levels(attribute):
    count = 0
    for k, token in enumerate(attribute):
        if token in ("allow", "expect", "warn") and attribute[k + 1 : k + 2] == ["("]:
            group = attribute[k + 1 : group_end(attribute, k + 1) + 1]
            count += any("unsafe" in t or t in ("clippy", "warnings") for t in group)
    return count


def measure(found, path, raw, tokens):
    for rule, pattern in WORDS.items():
        count = len(pattern.findall(raw))
        if count:
            found.add(path, rule, f"{path}: {count} {rule}", weight=count)
    allows = sum(1 for g in groups(LINT_LEVEL, raw) if WEAKENED_LINT.search(g))
    if tokens is not None:
        allows = max(allows, sum(weakening_levels(a) for a in attributes(tokens)))
    if allows:
        found.add(path, "lint-allow", f"{path}: {allows} allow, expect or warn naming unsafe, clippy or warnings", weight=allows)


def scan_rust(found, path, raw):
    if any(CFG_CLIPPY.search(g) for g in groups(CFG, raw)):
        found.add(path, "cfg-clippy", f"{path}: a cfg on clippy hides code from the lints")
    if INCLUDE.search(raw):
        found.add(path, "include-macro", f"{path}: include! pulls in code from outside the module tree")
    if PATH_ATTRIBUTE.search(raw) or any(PATH_KEY.search(g) for g in groups(CFG_ATTR, raw)):
        found.add(path, "path-attribute", f"{path}: #[path] loads a module from an arbitrary file")
    try:
        tokens = lex(raw.decode("utf-8"))
    except (UnicodeDecodeError, Unlexable) as e:
        found.add(path, "unlexable", f"{path}: the gate cannot lex this file ({e})")
        return None
    for i, token in enumerate(tokens):
        if token == "clippy" and tokens[i + 1 : i + 3] != [":", ":"]:
            found.add(path, "cfg-clippy", f"{path}: clippy used as a cfg name")
        if token == "include":
            found.add(path, "include-macro", f"{path}: include names the include! macro")
    for attribute in attributes(tokens):
        if any(t == "path" and attribute[k + 1 : k + 2] == ["="] for k, t in enumerate(attribute)):
            found.add(path, "path-attribute", f"{path}: #[path] loads a module from an arbitrary file")
        if "$" in attribute:
            found.add(path, "macro-attribute", f"{path}: an attribute built from a macro argument hides its lint level")
    for name, group in macro_groups(tokens):
        if "unsafe" in group:
            found.add(path, "unsafe-in-macro", f"{path}: unsafe inside {name}! covers every expansion with one count")
    return tokens


def subtree(root):
    if os.path.basename(root) in ("lib.rs", "main.rs", "mod.rs"):
        return os.path.dirname(root) + "/"
    return root[: -len(".rs")] + "/"


def check_roots(found, roots, tokens, unsafe):
    for root in roots:
        if root not in tokens:
            continue
        tree = subtree(root)
        fenced = unsafe[root] > 0 or any(unsafe[p] > 0 for p in unsafe if p.startswith(tree))
        allowed = {"forbid", "deny"} if fenced else {"forbid"}
        with open(root, encoding="utf-8") as f:
            first = f.readline().rstrip("\n")
        level = tokens[root][3] if len(tokens[root]) > 3 else None
        parsed = tokens[root][:8] == ["#", "!", "[", level, "(", "unsafe_code", ")", "]"]
        if not (parsed and level in allowed and first == ROOT_ATTRIBUTES[level]):
            want = " or ".join(ROOT_ATTRIBUTES[a] for a in sorted(allowed, reverse=True))
            found.add(root, "root-attribute", f"{root}: line 1 must be {want}")


def metadata():
    # outside the tree, so neither a toolchain file nor a .cargo/config in it picks the cargo that runs here
    manifest = os.path.join(os.getcwd(), "Cargo.toml")
    with tempfile.TemporaryDirectory() as neutral:
        result = run(["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1", "--manifest-path", manifest], cwd=neutral)
    if result.returncode != 0:
        die(f"cargo metadata: {result.stderr.strip()}")
    return json.loads(result.stdout)


def check_targets(found, files, here):
    roots = []
    for package in metadata()["packages"]:
        manifest = os.path.relpath(package["manifest_path"], here)
        if not manifest.startswith(CRATES):
            found.add(manifest, "target-outside-crates", f"{manifest}: workspace members live under {CRATES}")
        with open(manifest, "rb") as f:
            lints = tomllib.load(f).get("lints")
        if lints != {"workspace": True}:
            found.add(manifest, "crate-lints", f"{manifest}: [lints] must be exactly workspace = true")
        for target in package["targets"]:
            source = os.path.relpath(target["src_path"], here)
            kinds = set(target["kind"])
            if "custom-build" in kinds:
                found.add(source, "build-script", f"{source}: build scripts generate code the fence cannot see")
                continue
            if "proc-macro" in kinds:
                found.add(source, "proc-macro", f"{source}: proc macros generate code the fence cannot see")
            if not source.startswith(CRATES) or source not in files:
                found.add(source, "target-outside-crates", f"{source}: every target must be a listed file under {CRATES}")
                continue
            roots.append(source)
    return roots


def check_workspace(found):
    with open("Cargo.toml", "rb") as f:
        lints = tomllib.load(f).get("workspace", {}).get("lints", {})
    for tool, table in lints.items():
        for name, setting in table.items():
            level = setting.get("level") if isinstance(setting, dict) else setting
            if level not in ("deny", "forbid"):
                found.add("Cargo.toml", "workspace-lints", f"Cargo.toml: [workspace.lints.{tool}] {name} is {level}; only deny or forbid")
    for tool, names in REQUIRED_LINTS.items():
        for name in names:
            if name not in lints.get(tool, {}):
                found.add("Cargo.toml", "workspace-lints", f"Cargo.toml: [workspace.lints.{tool}] lost {name}")
    try:
        with open("clippy.toml", "rb") as f:
            banned = tomllib.load(f).get("disallowed-methods", [])
    except (OSError, tomllib.TOMLDecodeError) as e:
        found.add("clippy.toml", "clippy-config", f"clippy.toml: {e}")
        return
    paths = [entry if isinstance(entry, str) else entry.get("path") for entry in banned]
    if EXPOSE_BAN not in paths:
        found.add("clippy.toml", "clippy-config", f"clippy.toml: disallowed-methods no longer lists {EXPOSE_BAN}")


def gate_fence(found, _args):
    files = listed()
    if not files:
        die("git lists no files; run the gates from a checkout")
    here = os.getcwd()
    for path in files:
        parts = path.split("/")
        if parts[-1] in LINT_CONFIG_NAMES and path not in LINT_CONFIG_ALLOWED or ".cargo" in parts[:-1]:
            found.add(path, "lint-config-file", f"{path}: lint and build configuration lives only in the root clippy.toml and Cargo.toml")
        if parts[-1] == "build.rs":
            found.add(path, "build-script", f"{path}: build scripts generate code the fence cannot see")
    unsafe, tokens = collections.Counter(), {}
    for path in files:
        if not path.startswith(CRATES):
            continue
        if os.path.islink(path):
            found.add(path, "symlink", f"{path}: a symlink can point the build outside the fence")
            continue
        raw = read_bytes(path)
        lexed = scan_rust(found, path, raw) if path.endswith(".rs") else None
        if lexed is not None:
            tokens[path] = lexed
        measure(found, path, raw, lexed)
        unsafe[path] = len(WORDS["unsafe"].findall(raw))
    check_roots(found, check_targets(found, set(files), here), tokens, unsafe)
    check_workspace(found)


def days_between(start, end):
    return (end - start).days


def gate_advisories(found, _args):
    today = datetime.date.fromisoformat(os.environ.get("GATE_TODAY") or datetime.datetime.now(datetime.UTC).date().isoformat())
    try:
        with open("deny.toml", "rb") as f:
            config = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError) as e:
        found.add("deny.toml", "advisory-ignore", f"deny.toml: {e}")
        return
    ignores = config.get("advisories", {}).get("ignore", [])
    if not isinstance(ignores, list):
        found.add("deny.toml", "advisory-ignore", "deny.toml: advisories.ignore must be a list")
        return
    for entry in ignores:
        if not isinstance(entry, dict) or set(entry) != {"id", "reason"}:
            found.add("deny.toml", "advisory-ignore", f"deny.toml: {entry!r} must be a table with exactly id and reason")
            continue
        name, reason = entry["id"], entry["reason"]
        dated = isinstance(reason, str) and ADVISORY_REASON.match(reason)
        if not (isinstance(name, str) and ADVISORY_ID.fullmatch(name)) or not dated:
            found.add("deny.toml", "advisory-ignore", f"deny.toml: {name}: an ignore needs an advisory id and reason = \"expires YYYY-MM-DD: why\"")
            continue
        try:
            expires = datetime.date.fromisoformat(dated.group(1))
        except ValueError:
            found.add("deny.toml", "advisory-ignore", f"deny.toml: {name}: {dated.group(1)} is not a date")
            continue
        left = days_between(today, expires)
        if left < 0:
            found.add("deny.toml", "advisory-ignore", f"deny.toml: {name}: expired on {expires}; fix the dependency or re-justify it with a new date")
        elif left > HORIZON_DAYS:
            found.add("deny.toml", "advisory-ignore", f"deny.toml: {name}: expiry {expires} is more than {HORIZON_DAYS} days out")
    print(f"advisories: {len(ignores)} ignores read as of {today}")


def gate_canary(found, _args):
    result = run(CANARY)
    seen = set()
    for line in result.stdout.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        diagnostic = message.get("message") or {}
        code = (diagnostic.get("code") or {}).get("code")
        if message.get("reason") == "compiler-message" and diagnostic.get("level") == "error" and code:
            seen.add(code)
    if result.returncode == 0:
        found.add("crates/vigil-secret/examples/lint_canary.rs", "canary", "the canary built cleanly, so the lint configuration no longer rejects it")
    for lint in CANARY_LINTS:
        if lint not in seen:
            found.add("crates/vigil-secret/examples/lint_canary.rs", "canary", f"clippy no longer rejects the canary's {lint}: {result.stderr.strip()[-400:]}")


def parse_baseline(text):
    entries = {}
    for line in text.splitlines():
        gate, path, rule, count = line.split("\t")
        entries[gate, path, rule] = int(count)
    return entries


def git(*args):
    result = run(["git", *args])
    if result.returncode != 0:
        die(f"git {' '.join(args)}: {result.stderr.strip()}")
    return result.stdout


def at(revision, path):
    if revision is None:
        return read_bytes(path).decode("utf-8", "surrogateescape") if os.path.isfile(path) else None
    shown = run(["git", "show", f"{revision}:{path}"])
    return shown.stdout if shown.returncode == 0 else None


def changed(base, head):
    fields = git("diff", "--name-status", "-z", "--find-renames", base, *([head] if head else [])).split("\0")
    while len(fields) > 1:
        status = fields.pop(0)
        old = fields.pop(0)
        yield old, fields.pop(0) if status[0] in "RC" else old
    if head is None:
        for path in git("ls-files", "--others", "--exclude-standard", "-z").split("\0")[:-1]:
            yield path, path


def needs_approval(base, head):
    reasons, renamed = [], {}
    for old, new in changed(base, head):
        renamed[new] = old
        for path in dict.fromkeys((old, new)):
            if GUARDED.fullmatch(path):
                reasons.append((path, "a gate, lint or build configuration, or the Secret API changed"))
    before = parse_baseline(at(base, BASELINE) or "")
    for (gate, path, rule), count in parse_baseline(at(head, BASELINE) or "").items():
        was = before.get((gate, renamed.get(path, path), rule), 0)
        if count > was:
            reasons.append((BASELINE, f"{path} {rule} grew from {was} to {count}"))
    return reasons


def api(path, *options):
    if not shutil.which("gh"):
        die("gh is required to read a pull request's approval")
    result = run(["gh", "api", path, *options])
    if result.returncode != 0:
        die(f"gh api {path}: {result.stderr.strip()}")
    return result.stdout


def refusal(pull_request):
    owner = api("repos/{owner}/{repo}", "--jq", ".owner.login").strip()
    pull = json.loads(api(f"repos/{{owner}}/{{repo}}/pulls/{pull_request}"))
    if LABEL not in [label["name"] for label in pull["labels"]]:
        return f"the pull request does not carry the label {LABEL}"
    query = f'.[] | select(.label.name == "{LABEL}") | [.event, .actor.login, .created_at] | @tsv'
    events = api(f"repos/{{owner}}/{{repo}}/issues/{pull_request}/events", "--paginate", "--jq", query).splitlines()
    event, actor, labelled = events[-1].split("\t") if events else ("", "", "")
    if event != "labeled" or actor != owner:
        return f"the label {LABEL} was last {event or 'set'} by '{actor}', and only the repository owner can approve"
    if not pull["head"]["repo"]:
        return "the pull request's branch is gone"
    branch = urllib.parse.quote(pull["head"]["ref"], safe="")
    pushed = api(f"repos/{pull['head']['repo']['full_name']}/activity?ref={branch}&per_page=1", "--jq", ".[0].timestamp").strip()
    if not pushed or labelled <= pushed:
        return f"the label {LABEL} was added at {labelled}, before the last push at {pushed}; the owner removes it and adds it again"
    return None


def gate_guard(found, args):
    if run(["git", "rev-parse", "--verify", "--quiet", f"{args.base}^{{commit}}"]).returncode != 0:
        die(f"base revision {args.base} is not in this clone; fetch it, or pass --base")
    base = git("merge-base", args.base, args.head or "HEAD").strip()
    if at(base, "tests/gates/gate.py") is None:
        print(f"guard: {args.base} has no gates, so this change introduces them")
        return
    reasons = needs_approval(base, args.head)
    if not reasons:
        return
    print(f"guard: {len(reasons)} changes need the repository owner's approval (the label {LABEL}, added by the owner):")
    for path, why in reasons:
        print(f"  {path}: {why}")
    if not args.pr:
        print("guard: no pull request given (--pr), so this is a report and not a failure")
        return
    refused = refusal(args.pr)
    if refused is None:
        print(f"guard: approved, the repository owner added {LABEL} after the last push")
        return
    for path, why in reasons:
        found.add(path, "needs-owner-approval", f"{path}: {why}; {refused}")


GATES = {"fence": gate_fence, "advisories": gate_advisories, "canary": gate_canary, "guard": gate_guard}
LOCAL = ["fence", "advisories", "canary"]


def judge(gate, found, baseline, args):
    allowed = {key[1:]: count for key, count in baseline.items() if key[0] == gate and key[2] in COUNTED}
    failures = 0
    for key in sorted(set(found.counts) | set(allowed)):
        have, limit = found.counts[key], allowed.get(key, 0)
        if have == limit:
            continue
        counted = key[1] in COUNTED
        if args.update and counted and (have < limit or args.allow_growth):
            baseline[gate, *key] = have
            continue
        failures += 1
        if have > limit:
            print("\n".join(found.details[key]))
            if counted:
                print(f"FAIL {gate}: {key[0]} {key[1]}: {have} found, the baseline allows {limit}")
            else:
                print(f"FAIL {gate}: {key[0]} {key[1]}: never allowed")
        else:
            print(f"FAIL {gate}: {key[0]} {key[1]}: the baseline allows {limit} but only {have} remain; lower it with --update")
    if not failures and gate == "fence":
        totals = collections.Counter()
        for (path, rule), count in found.counts.items():
            totals[rule] += count
        print(f"ok   fence holds: {totals['unsafe']} unsafe, {totals['lint-allow']} lint allows, {totals['expose']} expose, per file in {BASELINE}")
    elif not failures:
        print(f"ok   {gate}")
    return failures


def main():
    parser = argparse.ArgumentParser(description="Safety gates with a per-file baseline that only shrinks.")
    parser.add_argument("gates", nargs="+", choices=[*GATES, "all"])
    parser.add_argument("--root", default=os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), help="checkout to judge; a base-branch copy of this script judges a pull request this way")
    parser.add_argument("--update", action="store_true", help="lower the baseline to what is found now")
    parser.add_argument("--allow-growth", action="store_true", help="with --update, also record growth; the guard then asks for the owner's approval")
    parser.add_argument("--base", default=os.environ.get("GATE_BASE", "origin/main"), help="revision the guard compares against")
    parser.add_argument("--head", default=os.environ.get("GATE_HEAD"), help="revision the guard judges, the working tree by default")
    parser.add_argument("--pr", default=os.environ.get("GATE_PR"), help="pull request whose approval the guard reads")
    args = parser.parse_args()
    os.chdir(args.root)
    baseline = parse_baseline(read_bytes(BASELINE).decode()) if os.path.exists(BASELINE) else {}
    failures = 0
    for gate in LOCAL if "all" in args.gates else args.gates:
        found = Findings()
        GATES[gate](found, args)
        failures += judge(gate, found, baseline, args)
    if args.update and not failures:
        rows = sorted((*key, count) for key, count in baseline.items() if count)
        with open(BASELINE, "w", encoding="utf-8") as f:
            f.writelines("\t".join(map(str, row)) + "\n" for row in rows)
        print(f"wrote {BASELINE}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()

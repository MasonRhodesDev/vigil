import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
GATE = os.path.join(HERE, "gate.py")
GIT = ["git", "-c", "user.name=gate", "-c", "user.email=gate@localhost", "-c", "commit.gpgsign=false"]

FAKE_GH = """\
import json, os, sys
scenario = json.load(open(os.environ["FAKE_GH_SCENARIO"]))
path = sys.argv[2]
for key, answer in scenario.items():
    if path.startswith(key):
        sys.stdout.write(answer)
        sys.exit(0)
sys.exit(f"fake gh: no answer for {path}")
"""


def run(argv, cwd, env=None):
    return subprocess.run(argv, cwd=cwd, capture_output=True, text=True, check=False, env=env)


class Scratch(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="gate-test-")
        cls.repo = os.path.join(cls.tmp, "repo")
        listed = run(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], ROOT).stdout
        for path in listed.split("\0")[:-1]:
            source = os.path.join(ROOT, path)
            if not os.path.lexists(source):
                continue
            copy = os.path.join(cls.repo, path)
            os.makedirs(os.path.dirname(copy), exist_ok=True)
            shutil.copy2(source, copy, follow_symlinks=False)
        for argv in (["init", "-q", "-b", "main"], ["add", "-A"], ["commit", "-qm", "base"]):
            subprocess.run([*GIT, *argv], cwd=cls.repo, check=True, capture_output=True)
        cls.base = run(["git", "rev-parse", "HEAD"], cls.repo).stdout.strip()

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def setUp(self):
        subprocess.run([*GIT, "reset", "-q", "--hard", self.base], cwd=self.repo, check=True)
        subprocess.run([*GIT, "clean", "-qfdx"], cwd=self.repo, check=True)

    def path(self, relative):
        return os.path.join(self.repo, relative)

    def read(self, relative):
        with open(self.path(relative), encoding="utf-8") as f:
            return f.read()

    def write(self, relative, text):
        os.makedirs(os.path.dirname(self.path(relative)), exist_ok=True)
        with open(self.path(relative), "w", encoding="utf-8") as f:
            f.write(text)

    def append(self, relative, text):
        self.write(relative, self.read(relative) + textwrap.dedent(text))

    def replace(self, relative, old, new):
        text = self.read(relative)
        self.assertIn(old, text)
        self.write(relative, text.replace(old, new, 1))

    def gate(self, *args, env=None):
        result = run([sys.executable, "-I", GATE, "--root", self.repo, *args], self.repo, env)
        return result.returncode, result.stdout + result.stderr

    def assertRefused(self, gate, *rules):
        code, out = self.gate(gate)
        self.assertNotEqual(code, 0, out)
        for rule in rules:
            self.assertIn(rule, out)
        return out

    def commit(self, message):
        subprocess.run([*GIT, "add", "-A"], cwd=self.repo, check=True)
        subprocess.run([*GIT, "commit", "-qm", message], cwd=self.repo, check=True)
        return run(["git", "rev-parse", "HEAD"], self.repo).stdout.strip()


LEAK = """
pub fn leak(secret: &vigil_secret::Secret) -> String {
    secret.expose().to_owned()
}
"""


class Fence(Scratch):
    def test_the_unchanged_tree_passes(self):
        code, out = self.gate("fence")
        self.assertEqual(code, 0, out)
        self.assertIn("fence holds", out)

    def test_cfg_not_clippy_is_refused(self):
        self.append("crates/vigil-lock/src/main.rs", "\n#[cfg(not(clippy))]" + LEAK)
        self.assertRefused("fence", "cfg-clippy", "crates/vigil-lock/src/main.rs expose")

    def test_cfg_attr_clippy_is_refused(self):
        self.append("crates/vigil-ui/src/lib.rs", "\n#[cfg_attr(clippy, allow(dead_code))]\nfn idle() {}\n")
        self.assertRefused("fence", "cfg-clippy")

    def test_a_clippy_group_allow_on_an_item_is_counted(self):
        self.append("crates/vigil-lock/src/main.rs", "\n#[allow(clippy::style)]" + LEAK)
        self.assertRefused("fence", "crates/vigil-lock/src/main.rs lint-allow", "crates/vigil-lock/src/main.rs expose")

    def test_a_crate_wide_clippy_all_is_counted(self):
        self.replace("crates/vigil-lock/src/main.rs", "#![deny(unsafe_code)]\n", "#![deny(unsafe_code)]\n#![allow(clippy::all)]\n")
        self.assertRefused("fence", "crates/vigil-lock/src/main.rs lint-allow")

    def test_include_is_refused_and_its_file_is_measured(self):
        self.write("crates/vigil-lock/src/glue.inc", "pub unsafe fn glue() {}\n")
        self.append("crates/vigil-lock/src/main.rs", '\ninclude!("glue.inc");\n')
        self.assertRefused("fence", "include-macro", "crates/vigil-lock/src/glue.inc unsafe")

    def test_a_renamed_include_is_refused(self):
        self.append("crates/vigil-lock/src/main.rs", '\nuse std::include as glue;\nglue!("glue.inc");\n')
        self.assertRefused("fence", "include-macro")

    def test_a_path_attribute_is_refused(self):
        self.write("crates/vigil/src/leak.inc", LEAK)
        self.append("crates/vigil/src/main.rs", '\n#[path = "leak.inc"]\nmod leak;\n')
        self.assertRefused("fence", "path-attribute", "crates/vigil/src/leak.inc expose")

    def test_a_path_attribute_behind_cfg_attr_is_refused(self):
        self.append("crates/vigil/src/main.rs", '\n#[cfg_attr(all(), path = "leak.inc")]\nmod leak;\n')
        self.assertRefused("fence", "path-attribute")

    def test_a_build_script_is_refused(self):
        self.write(
            "crates/vigil-input/build.rs",
            'fn main() {\n    let word = ["un", "safe"].concat();\n    let _ = word;\n}\n',
        )
        self.assertRefused("fence", "build-script")

    def test_a_named_build_script_is_refused(self):
        self.write("crates/vigil-input/gen.rs", "fn main() {}\n")
        self.replace("crates/vigil-input/Cargo.toml", "[package]\n", '[package]\nbuild = "gen.rs"\n')
        self.assertRefused("fence", "build-script")

    def test_a_proc_macro_crate_is_refused(self):
        self.replace("crates/vigil-input/Cargo.toml", "[lints]", "[lib]\nproc-macro = true\n\n[lints]")
        self.assertRefused("fence", "proc-macro")

    def test_live_code_after_a_line_comment_inside_a_block_comment_is_counted(self):
        self.append(
            "crates/vigil-lock/src/main.rs",
            """
            #[rustfmt::skip]
            #[expect(unsafe_code)]
            fn hidden(p: *const u8) -> u8 {
                /* SAFETY: hidden
                // */ unsafe { *p }
            }
            """,
        )
        self.assertRefused("fence", "crates/vigil-lock/src/main.rs unsafe", "crates/vigil-lock/src/main.rs lint-allow")

    def test_unsafe_comments_are_counted(self):
        self.append("crates/vigil-ui/src/lib.rs", "\n// unsafe\n")
        self.assertRefused("fence", "crates/vigil-ui/src/lib.rs unsafe")

    def test_unsafe_inside_macro_rules_is_refused(self):
        self.replace(
            "crates/vigil/src/hold.rs",
            "    let r = unsafe { libc::ioctl(fd, DROP_MASTER) };",
            "    let r = sys!(libc::ioctl(fd, DROP_MASTER));",
        )
        self.append(
            "crates/vigil/src/hold.rs",
            """
            macro_rules! sys {
                ($e:expr) => {
                    unsafe { $e }
                };
            }
            """,
        )
        self.assertRefused("fence", "unsafe-in-macro")

    def test_a_comment_between_the_level_and_its_paren_is_counted(self):
        self.append("crates/vigil-lock/src/main.rs", "\n#[allow/**/(clippy::multiple_unsafe_ops_per_block)]\nfn quiet() {}\n")
        self.assertRefused("fence", "crates/vigil-lock/src/main.rs lint-allow")

    def test_a_lint_level_passed_through_a_macro_is_refused(self):
        self.append("crates/vigil-ui/src/lib.rs", "\nmacro_rules! level {\n    ($a:ident) => {\n        #[$a(clippy::all)]\n        fn quiet() {}\n    };\n}\nlevel!(allow);\n")
        self.assertRefused("fence", "macro-attribute")

    def test_unsafe_passed_into_a_macro_is_refused(self):
        self.replace(
            "crates/vigil/src/hold.rs",
            "    let r = unsafe { libc::ioctl(fd, DROP_MASTER) };",
            "    let r = sys!(unsafe libc::ioctl(fd, DROP_MASTER), libc::sync(), libc::getpid());",
        )
        self.append(
            "crates/vigil/src/hold.rs",
            """
            macro_rules! sys {
                ($kw:tt $first:expr $(, $rest:expr)*) => {
                    $kw { $first $(; $rest)* }
                };
            }
            """,
        )
        self.assertRefused("fence", "unsafe-in-macro")

    def test_a_commented_forbid_is_refused(self):
        self.replace("crates/vigil-pam/src/lib.rs", "#![forbid(unsafe_code)]", "/* #![forbid(unsafe_code)] */")
        self.assertRefused("fence", "crates/vigil-pam/src/lib.rs root-attribute")

    def test_a_forbid_below_line_one_is_refused(self):
        self.replace("crates/vigil-pam/src/lib.rs", "#![forbid(unsafe_code)]\n", "\n#![forbid(unsafe_code)]\n")
        self.assertRefused("fence", "crates/vigil-pam/src/lib.rs root-attribute")

    def test_deny_needs_fenced_unsafe_in_the_tree(self):
        self.replace("crates/vigil-pam/src/lib.rs", "#![forbid(unsafe_code)]", "#![deny(unsafe_code)]")
        self.assertRefused("fence", "crates/vigil-pam/src/lib.rs root-attribute")

    def test_an_example_root_needs_the_attribute(self):
        self.replace("crates/vigil-gl/examples/gl_smoke.rs", "#![deny(unsafe_code)]\n", "")
        self.assertRefused("fence", "crates/vigil-gl/examples/gl_smoke.rs root-attribute")

    def test_a_target_outside_crates_is_refused(self):
        self.write("tests/leak.rs", "#![forbid(unsafe_code)]\nfn main() {}\n")
        self.replace("crates/vigil-input/Cargo.toml", "[lints]", '[[example]]\nname = "leak"\npath = "../../tests/leak.rs"\n\n[lints]')
        self.assertRefused("fence", "target-outside-crates")

    def test_a_crate_local_clippy_toml_is_refused(self):
        self.write("crates/vigil-lock/clippy.toml", "disallowed-methods = []\n")
        self.append("crates/vigil-lock/src/main.rs", LEAK)
        self.assertRefused("fence", "lint-config-file", "crates/vigil-lock/src/main.rs expose")

    def test_cargo_config_is_refused(self):
        self.write(".cargo/config.toml", '[build]\nrustflags = ["--cap-lints=allow"]\n')
        self.assertRefused("fence", "lint-config-file")

    def test_a_nested_cargo_config_is_refused(self):
        self.write("crates/vigil-lock/.cargo/config", '[build]\nrustflags = ["--cap-lints=allow"]\n')
        self.assertRefused("fence", "lint-config-file")

    def test_a_toolchain_file_is_refused(self):
        self.write("rust-toolchain.toml", '[toolchain]\nchannel = "1.70"\n')
        self.assertRefused("fence", "lint-config-file")

    def test_a_workspace_lint_set_to_allow_is_refused(self):
        self.replace("Cargo.toml", 'disallowed_methods = "deny"', 'disallowed_methods = "allow"')
        self.assertRefused("fence", "workspace-lints")

    def test_a_lowered_group_in_the_workspace_lints_is_refused(self):
        self.replace("Cargo.toml", 'disallowed_types = "deny"', 'disallowed_types = "deny"\nstyle = { level = "allow", priority = 1 }')
        self.assertRefused("fence", "workspace-lints")

    def test_pinned_lint_lines_moved_to_another_table_are_refused(self):
        self.replace("Cargo.toml", 'unsafe_code = "deny"\nunsafe_op_in_unsafe_fn = "deny"\n', "")
        self.append("Cargo.toml", '\n[workspace.metadata.parked]\nunsafe_code = "deny"\nunsafe_op_in_unsafe_fn = "deny"\n')
        self.assertRefused("fence", "workspace-lints")

    def test_a_crate_overriding_the_workspace_lints_is_refused(self):
        self.replace("crates/vigil-lock/Cargo.toml", "[lints]\nworkspace = true", "[lints.clippy]\ndisallowed_methods = \"allow\"")
        self.assertRefused("fence", "crate-lints")

    def test_a_commented_out_expose_ban_is_refused(self):
        text = self.read("clippy.toml")
        self.write("clippy.toml", "".join("# " + line for line in text.splitlines(keepends=True)))
        self.assertRefused("fence", "clippy-config")

    def test_a_symlink_under_crates_is_refused(self):
        os.symlink("/dev/null", self.path("crates/vigil-lock/src/link.rs"))
        self.assertRefused("fence", "symlink")

    def test_an_unterminated_comment_is_refused(self):
        self.append("crates/vigil-ui/src/lib.rs", "\n/* never closed\n")
        self.assertRefused("fence", "unlexable")

    def test_a_shrink_must_lower_the_baseline(self):
        self.replace("crates/vigil-gl/examples/gl_modeset.rs", "let bo = unsafe {", "let bo = {")
        out = self.assertRefused("fence", "crates/vigil-gl/examples/gl_modeset.rs unsafe")
        self.assertIn("lower it with --update", out)
        code, out = self.gate("fence", "--update")
        self.assertEqual(code, 0, out)
        code, out = self.gate("fence")
        self.assertEqual(code, 0, out)

    def test_update_never_records_growth_without_allow_growth(self):
        self.append("crates/vigil-ui/src/lib.rs", "\n// unsafe\n")
        code, out = self.gate("fence", "--update")
        self.assertNotEqual(code, 0, out)


class Advisories(Scratch):
    def deny(self, ignores):
        text = self.read("deny.toml")
        start = text.index("ignore = [")
        end = text.index("]\n", start) + 2
        self.write("deny.toml", text[:start] + ignores + text[end:])

    def test_the_unchanged_file_passes(self):
        code, out = self.gate("advisories", env={**os.environ, "GATE_TODAY": "2026-11-19"})
        self.assertEqual(code, 0, out)

    def check(self, *rules):
        code, out = self.gate("advisories", env={**os.environ, "GATE_TODAY": "2026-11-19"})
        self.assertNotEqual(code, 0, out)
        for rule in rules:
            self.assertIn(rule, out)

    def test_array_of_tables_ignores_are_read(self):
        text = self.read("deny.toml")
        start = text.index("ignore = [")
        end = text.index("]\n", start) + 2
        tables = '\n[[advisories.ignore]]\nid = "RUSTSEC-2026-0192"\n'
        self.write("deny.toml", text[:start] + text[end:] + tables)
        self.check("RUSTSEC-2026-0192")

    def test_a_spaced_section_header_is_read(self):
        self.replace("deny.toml", "[advisories]", "[ advisories ]")
        self.deny('ignore = ["RUSTSEC-2026-0192"]\n')
        self.check("RUSTSEC-2026-0192")

    def test_a_dotted_key_is_read(self):
        self.replace("deny.toml", "[advisories]\n", "")
        text = self.read("deny.toml")
        start = text.index("ignore = [")
        end = text.index("]\n", start) + 2
        self.write("deny.toml", 'advisories.ignore = ["RUSTSEC-2026-0192"]\n' + text[:start] + text[end:])
        self.check("RUSTSEC-2026-0192")

    def test_a_bare_id_is_refused(self):
        self.deny('ignore = [\n    "RUSTSEC-2026-0192",\n]\n')
        self.check("RUSTSEC-2026-0192")

    def test_two_entries_on_one_line_are_both_read(self):
        self.deny(
            'ignore = [{ id = "RUSTSEC-2026-0192", reason = "expires 2027-02-19: fine" }, '
            '{ id = "RUSTSEC-2026-0196", reason = "no date" }]\n'
        )
        self.check("RUSTSEC-2026-0196")

    def test_an_expired_entry_is_refused(self):
        self.deny('ignore = [{ id = "RUSTSEC-2026-0192", reason = "expires 2026-11-18: gone" }]\n')
        self.check("expired")

    def test_an_entry_past_the_horizon_is_refused(self):
        self.deny('ignore = [{ id = "RUSTSEC-2026-0192", reason = "expires 2027-06-30: too far" }]\n')
        self.check("more than 183 days")

    def test_an_invalid_date_is_refused(self):
        self.deny('ignore = [{ id = "RUSTSEC-2026-0192", reason = "expires 2027-02-30: no such day" }]\n')
        self.check("RUSTSEC-2026-0192")

    def test_an_unknown_key_is_refused(self):
        self.deny('ignore = [{ id = "RUSTSEC-2026-0192", reason = "expires 2027-02-19: x", crate = "x" }]\n')
        self.check("RUSTSEC-2026-0192")


class Guard(Scratch):
    OWNER = "repo-owner"

    def guard(self, head, events, pushed="2026-11-19T10:00:00Z", labels=("owner-approved",)):
        bin_dir = os.path.join(self.tmp, "bin")
        os.makedirs(bin_dir, exist_ok=True)
        with open(os.path.join(bin_dir, "gh"), "w", encoding="utf-8") as f:
            f.write(f"#!{sys.executable}\n{FAKE_GH}")
        os.chmod(os.path.join(bin_dir, "gh"), 0o755)
        scenario = os.path.join(self.tmp, "scenario.json")
        pull = {"labels": [{"name": n} for n in labels], "head": {"ref": "topic", "repo": {"full_name": "someone/vigil"}}}
        with open(scenario, "w", encoding="utf-8") as f:
            json.dump(
                {
                    "repos/{owner}/{repo}/pulls/": json.dumps(pull),
                    "repos/{owner}/{repo}/issues/": "".join("\t".join(e) + "\n" for e in events),
                    "repos/someone/vigil/activity": pushed + "\n",
                    "repos/{owner}/{repo}": self.OWNER + "\n",
                },
                f,
            )
        env = {**os.environ, "PATH": bin_dir + os.pathsep + os.environ["PATH"], "FAKE_GH_SCENARIO": scenario}
        return self.gate("guard", "--base", self.base, "--head", head, "--pr", "7", env=env)

    def totals_trick(self):
        self.replace("crates/vigil-gl/examples/gl_modeset.rs", "let bo = unsafe {", "let bo = {")
        self.append("crates/vigil-lock/src/main.rs", "\n// unsafe\n")
        code, out = self.gate("fence", "--update", "--allow-growth")
        self.assertEqual(code, 0, out)
        return self.commit("trade an example's unsafe for the lock's")

    def test_an_unguarded_change_passes(self):
        self.append("crates/vigil-ui/src/lib.rs", "\nfn idle() {}\n")
        code, out = self.guard(self.commit("plain"), [])
        self.assertEqual(code, 0, out)

    def test_the_totals_trick_needs_the_owner(self):
        was = next(line for line in self.read("tests/gates/baseline.tsv").splitlines() if "vigil-lock/src/main.rs\tunsafe" in line)
        was = int(was.split("\t")[-1])
        code, out = self.guard(self.totals_trick(), [], labels=())
        self.assertNotEqual(code, 0, out)
        self.assertIn(f"crates/vigil-lock/src/main.rs unsafe grew from {was} to {was + 1}", out)
        self.assertIn("does not carry the label", out)

    def test_a_label_from_someone_else_is_refused(self):
        head = self.totals_trick()
        code, out = self.guard(head, [("labeled", "an-agent", "2026-11-19T11:00:00Z")])
        self.assertNotEqual(code, 0, out)
        self.assertIn("only the repository owner can approve", out)

    def test_an_owner_label_before_the_last_push_is_refused(self):
        head = self.totals_trick()
        code, out = self.guard(head, [("labeled", self.OWNER, "2026-11-19T09:00:00Z")])
        self.assertNotEqual(code, 0, out)
        self.assertIn("before the last push", out)

    def test_a_removed_label_is_refused(self):
        head = self.totals_trick()
        events = [("labeled", self.OWNER, "2026-11-19T11:00:00Z"), ("unlabeled", self.OWNER, "2026-11-19T11:05:00Z")]
        code, out = self.guard(head, events)
        self.assertNotEqual(code, 0, out)

    def test_an_owner_label_after_the_last_push_approves(self):
        head = self.totals_trick()
        code, out = self.guard(head, [("labeled", self.OWNER, "2026-11-19T11:00:00Z")])
        self.assertEqual(code, 0, out)
        self.assertIn("approved", out)

    def test_guarded_paths_need_the_owner(self):
        for path, text in [
            ("tests/gates/gate.py", "\n"),
            ("tests/check.sh", "\n"),
            (".github/workflows/ci.yml", "\n"),
            ("Cargo.toml", "\n"),
            ("clippy.toml", "\n"),
            ("deny.toml", "\n"),
            ("crates/vigil-secret/src/lib.rs", "\nimpl std::ops::Deref for Secret {\n    type Target = str;\n    fn deref(&self) -> &str {\n        &self.0\n    }\n}\n"),
        ]:
            with self.subTest(path=path):
                self.setUp()
                self.append(path, text)
                code, out = self.guard(self.commit(path), [], labels=())
                self.assertNotEqual(code, 0, out)
                self.assertIn(path, out)

    def test_a_base_without_gates_is_reported(self):
        subprocess.run([*GIT, "rm", "-rq", "tests/gates"], cwd=self.repo, check=True)
        bare = self.commit("no gates")
        self.append("crates/vigil-ui/src/lib.rs", "\nfn idle() {}\n")
        head = self.commit("on top of a base without gates")
        code, out = self.gate("guard", "--base", bare, "--head", head)
        self.assertEqual(code, 0, out)
        self.assertIn("has no gates", out)


if __name__ == "__main__":
    unittest.main()

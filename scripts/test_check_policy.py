import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "check_policy", Path(__file__).with_name("check-policy.py")
)
check_policy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check_policy)


def run_policy(files):
    with tempfile.TemporaryDirectory() as temp_dir:
        root = Path(temp_dir)
        for filename, source in files.items():
            path = root / filename
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(source, encoding="utf-8")
        stdout = io.StringIO()
        stderr = io.StringIO()
        with (
            patch("sys.argv", ["check-policy.py", str(root)]),
            contextlib.redirect_stdout(stdout),
            contextlib.redirect_stderr(stderr),
        ):
            status = check_policy.main()
        return status, stdout.getvalue(), stderr.getvalue()


class PolicyTests(unittest.TestCase):
    def test_direct_inner_and_outer_suppressions_report_source_lines(self):
        status, stdout, stderr = run_policy({
            "src/lib.rs": "// header\n#![allow(dead_code)]\n\n#[expect(unused)]\nfn f() {}\n",
        })
        self.assertEqual(status, 1)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr.splitlines(), [
            "src/lib.rs:2: inline #[allow(...)]/#[expect(...)] is forbidden",
            "src/lib.rs:4: inline #[allow(...)]/#[expect(...)] is forbidden",
            "Set exceptions centrally in Cargo.toml; do not suppress lints in Rust source.",
        ])

    def test_conditional_suppressions_are_rejected_regardless_of_predicate(self):
        attributes = (
            "#[cfg_attr(any(), allow(dead_code))]",
            "#![cfg_attr(feature = \"disabled\", expect(unused))]",
            "#[cfg_attr(all(unix, feature = \"x\"), derive(Debug), allow(unused))]",
            "#[cfg_attr(any(), cfg_attr(all(), expect(unused)), derive(Debug))]",
            "#[cfg_attr(unix, cfg_attr(any(), derive(Debug), cfg_attr(all(), allow(unused))),)]",
        )
        for attribute in attributes:
            with self.subTest(attribute=attribute):
                status, _, stderr = run_policy({"src/lib.rs": attribute + "\nfn f() {}\n"})
                self.assertEqual(status, 1)
                self.assertIn("src/lib.rs:1: inline", stderr)

    def test_comments_can_separate_attribute_tokens(self):
        status, _, stderr = run_policy({
            "src/lib.rs": "\n# /* outer /* nested */ comment */ ! [\n"
            "cfg_attr /* comment */ (all(),\n"
            "  expect /* comment */ (unused))]\nfn f() {}\n",
        })
        self.assertEqual(status, 1)
        self.assertIn("src/lib.rs:2: inline", stderr)

    def test_raw_identifier_spellings_do_not_bypass_policy(self):
        for attribute in ("#[r#allow(unused)]", "#[r#cfg_attr(unix, r#expect(unused))]"):
            with self.subTest(attribute=attribute):
                status, _, stderr = run_policy({"src/lib.rs": attribute + "\nfn f() {}\n"})
                self.assertEqual(status, 1)
                self.assertIn("src/lib.rs:1: inline", stderr)

    def test_attribute_looking_comments_and_literals_are_ignored(self):
        source = r'''// #[allow(dead_code)]
/// #[expect(unused)]
/* #[allow(unused)] /* nested #[expect(unused)] */ still #[allow(unused)] */
const TEXT: &str = "#[allow(unused)] and \"#[expect(unused)]\"";
const MULTILINE: &str = "start
#[allow(unused)]
end";
const BYTES: &[u8] = b"#[expect(unused)]";
const RAW: &str = r"#[allow(unused)]";
const HASHED: &str = r###"quote "# #[expect(unused)] /* not a comment */"###;
const RAW_BYTES: &[u8] = br##"#[allow(unused)]"##;
const C_TEXT: &std::ffi::CStr = c"#[expect(unused)]";
const RAW_C: &std::ffi::CStr = cr#"#[allow(unused)]"#;
const CHARS: [char; 5] = ['#', '\'', '\\', '\u{23}', '😀'];
const BYTE: u8 = b'#';
fn borrow<'a, 'b>(value: &'a str, other: &'b str) -> &'a str { value }
#[doc = "#[allow(unused)]"]
#[cfg_attr(any(), doc = "#[expect(unused)]")]
fn documented() {}
'''
        status, stdout, stderr = run_policy({"src/lib.rs": source})
        self.assertEqual(status, 0, stderr)
        self.assertEqual(stdout, "Policy check passed: no inline lint suppressions.\n")
        self.assertEqual(stderr, "")

    def test_lifetimes_literals_and_nested_comments_do_not_hide_later_attributes(self):
        source = r'''fn borrow<'a>(value: &'a str) -> &'a str { value }
const QUOTE: char = '\'';
const RAW: &str = r##"\" /* #[allow(unused)] */"##;
/* before /* #[expect(unused)] */ after */
#[allow(unused)]
fn prohibited() {}
'''
        status, _, stderr = run_policy({"src/lib.rs": source})
        self.assertEqual(status, 1)
        self.assertEqual(stderr.splitlines(), [
            "src/lib.rs:5: inline #[allow(...)]/#[expect(...)] is forbidden",
            "Set exceptions centrally in Cargo.toml; do not suppress lints in Rust source.",
        ])

    def test_only_suppression_attribute_positions_are_prohibited(self):
        source = '''#[cfg_attr(allow(feature = "x"), derive(Debug))]
#[cfg_attr(any(), cfg_attr(expect(feature = "x"), derive(Clone))) ]
#[custom(allow(unused), expect(unused), cfg_attr(unix, allow(unused)))]
#[tool::allow(unused)]
struct Example;
'''
        status, _, stderr = run_policy({"src/lib.rs": source})
        self.assertEqual(status, 0, stderr)
        self.assertEqual(stderr, "")

    def test_every_source_directory_and_root_build_script_are_checked(self):
        paths = (
            "src/nested/module.rs",
            "tests/nested/integration.rs",
            "benches/nested/benchmark.rs",
            "examples/nested/example.rs",
            "build.rs",
        )
        for path in paths:
            with self.subTest(path=path):
                status, _, stderr = run_policy({
                    path: "// first line\n#[cfg_attr(any(), expect(unused))]\nfn f() {}\n",
                })
                self.assertEqual(status, 1)
                self.assertIn(f"{path}:2: inline", stderr)

    def test_build_script_uses_the_same_comment_and_literal_rules(self):
        status, _, stderr = run_policy({
            "build.rs": '// #[allow(unused)]\nfn main() { println!("#[expect(unused)]"); }\n',
            "target/generated.rs": "#[allow(unused)]\nfn generated() {}\n",
        })
        self.assertEqual(status, 0, stderr)
        self.assertEqual(stderr, "")


if __name__ == "__main__":
    unittest.main()

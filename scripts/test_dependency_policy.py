import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("policy", Path(__file__).with_name("check-dependencies.py"))
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


class PolicyTests(unittest.TestCase):
    def test_exact_names_and_duplicates(self):
        tree = "app v1.0.0 (/workspace)\nring v0.17.0\nring v0.17.0 (*)\nspring v1.0.0\n"
        self.assertEqual(policy.forbidden(tree), ["ring"])

    def test_git_and_build_helpers_are_not_native_crypto(self):
        tree = "client v0.1.0 (https://example.invalid/git#123)\ncc v1.0.0\n"
        self.assertEqual(policy.forbidden(tree), [])

    def test_missing_or_unexpected_output_fails_closed(self):
        for tree in ["", "not a cargo tree", "app v1.0.0\n??"]:
            with self.assertRaises(ValueError):
                policy.forbidden(tree)

    def test_target_features_and_workspace_are_explicit(self):
        command = policy.tree_command("custom/Cargo.toml", "host")
        self.assertIn("--all-features", command)
        self.assertIn("--workspace", command)
        self.assertEqual(command[command.index("--target") + 1], "host")
        self.assertIn("normal,build,dev", command)
        selected = policy.tree_command("Cargo.toml", "host", "client")
        self.assertNotIn("--workspace", selected)
        self.assertEqual(selected[-2:], ["--package", "client"])

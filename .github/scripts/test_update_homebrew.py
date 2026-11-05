import unittest

from update_homebrew import update_formula, version


class FormulaTests(unittest.TestCase):
    def setUp(self):
        self.checksum = "a" * 64
        self.formula = (
            'class Sqtop < Formula\n'
            '  url "https://github.com/skim0119/sqtop/archive/refs/tags/v0.1.0.tar.gz"\n'
            f'  sha256 "{"b" * 64}"\n'
            '  depends_on "rust" => :build\n'
            'end\n'
        )

    def test_updates_only_source_and_checksum_and_can_be_repeated(self):
        updated = update_formula(self.formula, "v0.2.0", self.checksum)
        expected = self.formula.replace("v0.1.0", "v0.2.0").replace("b" * 64, self.checksum)
        self.assertEqual(updated, expected)
        self.assertEqual(update_formula(updated, "v0.2.0", self.checksum), updated)

    def test_refuses_downgrades_and_unrecognized_formula(self):
        for formula, tag in [
            (self.formula, "v0.0.9"),
            (self.formula.replace('  url "', '  url "unexpected/'), "v0.2.0"),
            (self.formula.replace("b" * 64, "missing"), "v0.2.0"),
        ]:
            with self.assertRaises(ValueError):
                update_formula(formula, tag, self.checksum)

    def test_compares_versions_numerically(self):
        formula = self.formula.replace("v0.1.0", "v0.9.0")
        self.assertIn("v0.10.0", update_formula(formula, "v0.10.0", self.checksum))

    def test_rejects_prereleases_and_invalid_tags(self):
        for tag in ["", "main", "v0.2.0-rc.1", "v1.0.0/other", "$(echo bad)"]:
            with self.assertRaises(ValueError):
                version(tag)


if __name__ == "__main__":
    unittest.main()

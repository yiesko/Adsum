import importlib.util
import tempfile
import unittest
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "release", Path(__file__).resolve().parents[1] / "release.py"
)
release = importlib.util.module_from_spec(_SPEC)
import sys

sys.modules["release"] = release
_SPEC.loader.exec_module(release)


class ReleaseTagTests(unittest.TestCase):
    def test_classification(self):
        cases = {
            "v0.41.0-alpha.1": True,
            "v0.41.0-beta.1": True,
            "v0.41.0-beta.2": True,
            "v0.41.0-rc.1": True,
            "v0.41.0": False,
            "v0.41.0+build.5": False,
            "v0.41.0-beta.1+build.5": True,
            "v0.41.0-anything.valid": True,
        }
        for tag, expected in cases.items():
            with self.subTest(tag=tag):
                self.assertEqual(release.parse_tag(tag).is_prerelease, expected)

    def test_invalid_tags_rejected(self):
        for tag in ["0.41.0", "v0.41", "v0.41.0-", "v0.41.0-beta..1", "v0.41.0+"]:
            with self.subTest(tag=tag):
                with self.assertRaises(ValueError):
                    release.parse_tag(tag)

    def test_previous_tag_ignores_build_metadata_and_invalid_tags(self):
        tags = [
            "v0.39.0",
            "not-semver",
            "v0.40.0",
            "v0.41.0-beta.1",
            "v0.41.0-beta.1+build.4",
            "v0.41.0-beta.2",
        ]
        self.assertEqual(release.previous_tag("v0.41.0-beta.2", tags), "v0.41.0-beta.1")
        self.assertEqual(release.previous_tag("v0.41.0-beta.1", tags), "v0.40.0")
        self.assertIsNone(release.previous_tag("v1.0.0", []))

    def test_changelog_extraction_boundaries_and_empty_section(self):
        body = """# Changelog

## [Unreleased]

### Added
- Later

## [0.41.0] - 2026-01-01

### Added
- Current

## [0.40.0] - 2025-01-01

### Added
- Previous
"""
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "CHANGELOG.md"
            path.write_text(body)
            extracted = release.extract_changelog(path, "v0.41.0")
            self.assertEqual(extracted, "### Added\n- Current\n")
            missing = release.extract_changelog
            with self.assertRaises(ValueError):
                missing(path, "v9.9.9")
            empty = body.replace("### Added\n- Current", " ")
            path.write_text(empty)
            with self.assertRaises(ValueError):
                release.extract_changelog(path, "v0.41.0")

    def test_real_changelog_extraction(self):
        path = Path(__file__).resolve().parents[2] / "CHANGELOG.md"
        body = release.extract_changelog(path, "v0.40.0")
        self.assertIn("Added", body)
        self.assertNotIn("0.39.0", body)

    def test_package_version_stamp_and_stable_consistency(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo_toml = Path(directory) / "Cargo.toml"
            cargo_toml.write_text('[workspace.package]\nversion = "0.40.0"\n\n[package]\nname="x"\n')
            release.stamp_package_version(cargo_toml, "v0.41.0-beta.1")
            self.assertIn('version = "0.41.0-beta.1"', cargo_toml.read_text())
            cargo_toml.write_text('[workspace.package]\nversion = "0.40.0"\n')
            with self.assertRaises(ValueError):
                release.stamp_package_version(cargo_toml, "v0.41.0")


if __name__ == "__main__":
    unittest.main()

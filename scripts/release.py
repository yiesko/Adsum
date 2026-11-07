#!/usr/bin/env python3
"""Classify tagged releases and prepare GitHub release notes."""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

SEMVER_RE = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)"
    r"(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?"
    r"(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$"
)


@dataclass(frozen=True)
class SemVer:
    """A validated SemVer version and its prerelease classification."""

    tag: str
    version: str
    core: tuple[int, int, int]
    prerelease: tuple[str, ...] | None

    @property
    def is_prerelease(self) -> bool:
        """Whether this version has a SemVer prerelease identifier."""
        return self.prerelease is not None


def _parse_semver(version: str, source: str) -> SemVer:
    match = SEMVER_RE.fullmatch(version)
    if not match:
        raise ValueError(f"invalid SemVer in {source}: {version!r}")
    prerelease = tuple(match.group(4).split(".")) if match.group(4) else None
    return SemVer(
        tag=source,
        version=version,
        core=(int(match.group(1)), int(match.group(2)), int(match.group(3))),
        prerelease=prerelease,
    )


def parse_tag(tag: str) -> SemVer:
    """Return a validated SemVer for a git tag such as `v0.41.0-beta.1`."""
    raw = tag.strip()
    if not raw.startswith("v") or len(raw) == 1:
        raise ValueError(f"invalid release tag: {tag!r}")
    return _parse_semver(raw[1:], raw)


def _sort_key(version: SemVer) -> tuple:
    """Precedence comparison per SemVer: build metadata is ignored."""
    identifiers = ()
    if version.prerelease is not None:
        identifiers = tuple(
            (0, int(part), "") if part.isdigit() else (1, 0, part)
            for part in version.prerelease
        )
    return (
        *version.core,
        1 if version.prerelease is None else 0,
        identifiers,
    )


def _extract_section(path: Path, heading: str) -> str:
    """Return the non-empty body under one `## [..]` changelog heading."""
    lines: list[str] = []
    found = False
    for line in path.read_text().splitlines():
        if line.startswith("## ["):
            if found:
                break
            found = line.split("]", 1)[0] + "]" == heading
            continue
        if found:
            lines.append(line)
    if not found:
        raise ValueError(f"CHANGELOG.md has no '{heading}' section")
    body = "\n".join(lines).strip()
    if not body:
        raise ValueError(f"CHANGELOG.md '{heading}' section is empty")
    return body + "\n"


def extract_changelog(path: Path, tag: str) -> str:
    """Extract the non-empty section for a stable tag's exact SemVer version."""
    parsed = parse_tag(tag)
    return _extract_section(path, f"## [{parsed.version}]")


def extract_unreleased(path: Path) -> str:
    """Extract the `[Unreleased]` section used as pre-release notes."""
    return _extract_section(path, "## [Unreleased]")


def stamp_package_version(path: Path, tag: str) -> None:
    """Stamp exact SemVer in `[workspace.package]`; stable tags must agree."""
    parsed = parse_tag(tag)
    text = path.read_text()
    lines = text.splitlines(keepends=True)
    in_section = False
    found = False
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped == "[workspace.package]":
            in_section = True
            continue
        if in_section and stripped.startswith("["):
            in_section = False
        if not in_section:
            continue
        match = re.match(r"^(\s*version\s*=\s*\")([^\"]*)(\".*)$", line)
        if not match:
            continue
        found = True
        current = _parse_semver(match.group(2), "workspace package version")
        if not parsed.is_prerelease and _sort_key(current) != _sort_key(parsed):
            raise ValueError(
                "stable tag does not match workspace version: "
                f"{match.group(2)!r} != {parsed.version!r}"
            )
        ending = "\r\n" if line.endswith("\r\n") else "\n" if line.endswith("\n") else ""
        lines[index] = match.group(1) + parsed.version + match.group(3) + ending
        break
    if not found:
        raise ValueError("Cargo.toml has no version in [workspace.package]")
    path.write_text("".join(lines))


def cmd_classify(args: argparse.Namespace) -> int:
    parsed = parse_tag(args.tag)
    print(f"version={parsed.version}")
    print(f"prerelease={'true' if parsed.is_prerelease else 'false'}")
    return 0


def cmd_extract_changelog(args: argparse.Namespace) -> int:
    body = extract_changelog(Path(args.changelog), args.tag)
    Path(args.output).write_text(body)
    return 0


def cmd_extract_unreleased(args: argparse.Namespace) -> int:
    body = extract_unreleased(Path(args.changelog))
    Path(args.output).write_text(body)
    return 0


def cmd_stamp_version(args: argparse.Namespace) -> int:
    stamp_package_version(Path(args.cargo_toml), args.tag)
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)

    classify = subcommands.add_parser("classify")
    classify.add_argument("tag")
    classify.set_defaults(func=cmd_classify)

    extract = subcommands.add_parser("extract-changelog")
    extract.add_argument("changelog")
    extract.add_argument("tag")
    extract.add_argument("output")
    extract.set_defaults(func=cmd_extract_changelog)

    unreleased = subcommands.add_parser("extract-unreleased")
    unreleased.add_argument("changelog")
    unreleased.add_argument("output")
    unreleased.set_defaults(func=cmd_extract_unreleased)

    stamp = subcommands.add_parser("stamp-version")
    stamp.add_argument("cargo_toml")
    stamp.add_argument("tag")
    stamp.set_defaults(func=cmd_stamp_version)

    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except (ValueError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

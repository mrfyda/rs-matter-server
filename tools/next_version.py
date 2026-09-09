"""Derive the version of the next release from the manifest and the tag history.

Every commit that lands on main and passes CI is released, so nothing hand-edits
a patch number. This computes it:

  * The highest `vMAJOR.MINOR.PATCH` tag in the repository is the last release.
    The next one is that with the patch incremented.
  * `server/Cargo.toml`'s version is the *floor* of the series, not the released
    version. When it names something higher than the last tag — because a minor
    or major bump was committed by hand — that version is released as-is, and
    the automatic patches resume from there.
  * With no tags at all, the floor is the first release.

Usage:

    # What the next release would be, as `key=value` lines for $GITHUB_OUTPUT.
    python3 tools/next_version.py

    # The same, plus the fully qualified image tags to push.
    python3 tools/next_version.py \
        --image ghcr.io/mrfyda/rs-matter-server --sha "$GITHUB_SHA" --branch main

    # The rules above, checked against a table of cases.
    python3 tools/next_version.py --self-test

Tags that are not exactly `vX.Y.Z` are ignored, so a pre-release tag pushed by
hand (`v0.3.0-rc.1`) never becomes the base for the next patch.
"""
import argparse
import re
import subprocess
from pathlib import Path

# Deliberately strict: a release tag is `v` and three numbers, nothing else.
TAG_RE = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")
# The `version` of the `[package]` table specifically. The search stops at the
# next table header, so a dependency's own `version` can never be mistaken for
# it, and three numbers are required: anything else — a `0.2` or a pre-release
# suffix — has no place in a scheme where the patch is derived.
MANIFEST_RE = re.compile(
    r"^\[package\]\n(?:(?!\[)[^\n]*\n)*?version = \"(\d+)\.(\d+)\.(\d+)\"$",
    re.M,
)


def parse_manifest_text(text):
    """The `[package] version` of a Cargo manifest, as a (major, minor, patch)."""
    match = MANIFEST_RE.search(text)
    if not match:
        return None
    return tuple(int(part) for part in match.groups())


def parse_manifest(path):
    """The `[package] version` of the manifest at `path`."""
    version = parse_manifest_text(Path(path).read_text())
    if version is None:
        raise SystemExit(f"{path}: no [package] version of the form X.Y.Z")
    return version


def parse_tags(names):
    """The release versions among `names`, ignoring everything else."""
    matches = (TAG_RE.match(name.strip()) for name in names)
    return [tuple(int(part) for part in m.groups()) for m in matches if m]


def next_version(floor, releases):
    """The version to release, given the manifest floor and the released versions."""
    if not releases:
        return floor
    last = max(releases)
    # A hand-committed minor or major bump wins; otherwise carry on patching.
    return floor if floor > last else (last[0], last[1], last[2] + 1)


def image_tags(version, image, sha=None, branch=None):
    """The tags to push for `version`, most specific first.

    `MAJOR.MINOR` and `latest` move; the full version never does. No bare
    `MAJOR` tag is published while the major is 0 — under semver a 0.x minor
    bump may break the API, so a `0` tag would promise a compatibility that
    does not exist.
    """
    major, minor, patch = version
    names = [f"{major}.{minor}.{patch}", f"{major}.{minor}"]
    if major > 0:
        names.append(str(major))
    names.append("latest")
    # Not a version at all: a way back from a running container to its commit.
    if sha:
        names.append(f"sha-{sha[:7]}")
    # Kept for anyone already pulling `:main`. Identical to `:latest`, since a
    # release is cut from every commit that lands there.
    if branch:
        names.append(branch)
    return [f"{image}:{name}" for name in names]


def _self_test():
    cases = [
        # (floor, tags, expected next)
        # Nothing released yet: the manifest names the first release.
        ((0, 1, 0), [], (0, 1, 0)),
        # Steady state: the patch comes from the last tag, not the manifest.
        ((0, 1, 0), ["v0.1.0"], (0, 1, 1)),
        ((0, 1, 0), ["v0.1.0", "v0.1.7"], (0, 1, 8)),
        # Tag order in the list must not matter.
        ((0, 1, 0), ["v0.1.7", "v0.1.0"], (0, 1, 8)),
        # 10 sorts above 9 numerically, not lexically.
        ((0, 1, 0), ["v0.1.9", "v0.1.10"], (0, 1, 11)),
        # A hand-committed minor or major bump is released as it stands.
        ((0, 2, 0), ["v0.1.7"], (0, 2, 0)),
        ((1, 0, 0), ["v0.9.3"], (1, 0, 0)),
        # ...and the automatic patches resume from it.
        ((0, 2, 0), ["v0.1.7", "v0.2.0"], (0, 2, 1)),
        # A floor left behind by the tags is simply ignored.
        ((0, 1, 0), ["v0.4.2"], (0, 4, 3)),
        # Pre-release and unrelated tags are not releases.
        ((0, 1, 0), ["v0.1.0", "v0.2.0-rc.1", "nightly", "v1.2"], (0, 1, 1)),
    ]
    failures = 0
    for floor, tags, expected in cases:
        got = next_version(floor, parse_tags(tags))
        if got != expected:
            failures += 1
            print(f"FAIL floor={floor} tags={tags}: expected {expected}, got {got}")

    tag_cases = [
        # (version, expected names)
        ((0, 1, 0), ["0.1.0", "0.1", "latest"]),
        ((1, 2, 3), ["1.2.3", "1.2", "1", "latest"]),
    ]
    for version, expected in tag_cases:
        got = [t.split(":")[-1] for t in image_tags(version, "img")]
        if got != expected:
            failures += 1
            print(f"FAIL tags for {version}: expected {expected}, got {got}")

    got = [t.split(":")[-1] for t in image_tags((0, 1, 0), "img", sha="abcdef1234", branch="main")]
    if got != ["0.1.0", "0.1", "latest", "sha-abcdef1", "main"]:
        failures += 1
        print(f"FAIL sha and branch tags: got {got}")

    manifest_cases = [
        # (manifest, expected version or None)
        ('[package]\nname = "x"\nversion = "1.2.3"\n', (1, 2, 3)),
        # Comments and other keys sit between the two in the real manifest.
        ('[package]\nname = "x"\n# a comment\nversion = "0.1.0"\nedition = "2021"\n', (0, 1, 0)),
        # A dependency's version is not the package's.
        ('[package]\nname = "x"\n\n[dependencies]\nversion = "9.9.9"\n', None),
        # Neither is one in a target table further down.
        ('[package]\nname = "x"\n\n[dependencies.serde]\nversion = "1.0.0"\n', None),
        # A scheme that derives the patch has nowhere to put these.
        ('[package]\nversion = "0.2"\n', None),
        ('[package]\nversion = "0.2.0-rc.1"\n', None),
    ]
    for manifest, expected in manifest_cases:
        got = parse_manifest_text(manifest)
        if got != expected:
            failures += 1
            print(f"FAIL manifest {manifest!r}: expected {expected}, got {got}")

    if failures:
        raise SystemExit(f"{failures} failure(s)")
    print(f"{len(cases) + len(tag_cases) + len(manifest_cases) + 1} cases pass")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest", default="server/Cargo.toml")
    parser.add_argument("--image", help="registry path, to emit fully qualified image tags")
    parser.add_argument("--sha", help="commit being released, for the sha- image tag")
    parser.add_argument("--branch", help="branch being released, for the branch image tag")
    parser.add_argument("--self-test", action="store_true", help="check the rules and exit")
    args = parser.parse_args(argv)

    if args.self_test:
        return _self_test()

    # `git tag` rather than the API: the checkout already has the tags, and the
    # only thing that must be true is that it was fetched with them.
    tags = subprocess.run(
        ["git", "tag", "--list"], check=True, capture_output=True, text=True
    ).stdout.splitlines()

    floor = parse_manifest(args.manifest)
    version = next_version(floor, parse_tags(tags))
    major, minor, patch = version
    print(f"version={major}.{minor}.{patch}")
    print(f"tag=v{major}.{minor}.{patch}")
    if args.image:
        # A multiline value, in the form $GITHUB_OUTPUT accepts.
        print("image_tags<<IMAGE_TAGS")
        for tag in image_tags(version, args.image, args.sha, args.branch):
            print(tag)
        print("IMAGE_TAGS")


if __name__ == "__main__":
    main()

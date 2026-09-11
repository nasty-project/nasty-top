"""Validate release versions and crates.io availability before publishing."""

import argparse
import json
import os
from pathlib import Path
import tomllib

CRATE = "nasty-top"


def validate_version(tag, manifest, lockfile):
    package = manifest["package"]
    version = package["version"]
    if package["name"] != CRATE:
        raise ValueError(f"Expected package {CRATE}, got {package['name']}")
    if tag != f"v{version}":
        raise ValueError(f"Tag {tag!r} does not match Cargo.toml version {version}")
    locked = [
        p["version"]
        for p in lockfile["package"]
        if p["name"] == CRATE and "source" not in p
    ]
    if locked != [version]:
        raise ValueError(f"Cargo.lock package version {locked} does not match {version}")
    return version


def should_publish(status, response, version):
    if status == 404:
        return True
    if status != 200:
        raise ValueError(f"crates.io availability check returned HTTP {status}")
    record = json.loads(response)["version"]
    if record["crate"] != CRATE or record["num"] != version:
        raise ValueError("crates.io returned unexpected crate/version metadata")
    # Existing versions are immutable, including yanked ones. Reruns must not
    # attempt to overwrite them or request publishing credentials unnecessarily.
    return False


def output(name, value):
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        print(f"{name}={value}", file=stream)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("validate")
    availability = commands.add_parser("availability")
    availability.add_argument("status", type=int)
    availability.add_argument("response", type=Path)
    availability.add_argument("version")
    args = parser.parse_args()
    try:
        if args.command == "validate":
            version = validate_version(
                os.environ.get("GITHUB_REF_NAME", ""),
                tomllib.loads(Path("Cargo.toml").read_text()),
                tomllib.loads(Path("Cargo.lock").read_text()),
            )
            output("version", version)
            print(f"Validated {CRATE} {version}")
        else:
            publish = should_publish(args.status, args.response.read_text(), args.version)
            output("publish", str(publish).lower())
            print(f"{CRATE} {args.version}: {'ready to publish' if publish else 'already published; skipping'}")
    except (ValueError, KeyError, OSError) as error:
        raise SystemExit(str(error)) from error


if __name__ == "__main__":
    main()

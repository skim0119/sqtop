"""Update the tap's source URL and checksum for a stable sqtop release."""

import hashlib
from pathlib import Path
import re
import sys

ARCHIVE_URL = "https://github.com/skim0119/sqtop/archive/refs/tags/"


def version(tag):
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError("Expected a stable release tag such as v0.1.0")
    return tuple(int(part) for part in tag[1:].split("."))


def update_formula(text, tag, checksum):
    target = version(tag)
    source = re.search(r'^  url "' + re.escape(ARCHIVE_URL) + r'(v\d+\.\d+\.\d+)\.tar\.gz"$', text, re.MULTILINE)
    if source is None:
        raise ValueError("Cannot find sqtop's stable source URL in the formula")
    if target < version(source[1]):
        raise ValueError("Refusing to replace a newer Homebrew version")
    text = text[:source.start()] + f'  url "{ARCHIVE_URL}{tag}.tar.gz"' + text[source.end():]
    text, replacements = re.subn(r'^  sha256 "[0-9a-f]{64}"$', f'  sha256 "{checksum}"', text, count=1, flags=re.MULTILINE)
    if replacements != 1:
        raise ValueError("Cannot find the source checksum in the formula")
    return text


if __name__ == "__main__":
    formula = Path(sys.argv[1])
    tag = sys.argv[2]
    checksum = hashlib.sha256(Path(sys.argv[3]).read_bytes()).hexdigest()
    formula.write_text(update_formula(formula.read_text(), tag, checksum))

"""Validate exactly one SDK wheel and sdist; optionally test isolated installs."""

from __future__ import annotations

import argparse
from email.parser import BytesParser
from importlib.metadata import version
from pathlib import Path
import subprocess
import sys
import tarfile
import zipfile

from packaging.requirements import Requirement


def check(directory: Path, install: bool) -> None:
    expected = version("vectors-sdk")
    wheel = directory / f"vectors_sdk-{expected}-py3-none-any.whl"
    sdist = directory / f"vectors_sdk-{expected}.tar.gz"
    artifacts = set(directory.glob("*.whl")) | set(directory.glob("*.tar.gz"))
    if artifacts != {wheel, sdist}:
        raise SystemExit(f"expected only a wheel and sdist for vectors-sdk {expected}")

    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        required = {
            "vectors_sdk/__init__.py",
            "vectors_sdk/_version.py",
            "vectors_sdk/_client.py",
            "vectors_sdk/_async_client.py",
            "vectors_sdk/_common.py",
            "vectors_sdk/models.py",
            "vectors_sdk/errors.py",
            "vectors_sdk/py.typed",
        }
        if not required.issubset(names):
            raise SystemExit("wheel is missing SDK modules or the typing marker")
        if any(
            not (
                name.startswith("vectors_sdk/")
                or name.startswith(f"vectors_sdk-{expected}.dist-info/")
            )
            for name in names
        ):
            raise SystemExit("wheel includes unexpected files")
        metadata = BytesParser().parsebytes(
            archive.read(f"vectors_sdk-{expected}.dist-info/METADATA")
        )
        if metadata["Name"] != "vectors-sdk" or metadata["Version"] != expected:
            raise SystemExit("wheel metadata does not match the installed SDK")
        runtime = {
            Requirement(item).name for item in metadata.get_all("Requires-Dist", [])
        }
        if runtime != {"httpx"} or metadata["Requires-Python"] != ">=3.10":
            raise SystemExit("unexpected runtime dependencies or Python requirement")

    with tarfile.open(sdist) as archive:
        root = f"vectors_sdk-{expected}/"
        names = {name.removeprefix(root) for name in archive.getnames()}
        required = {
            "pyproject.toml",
            "README.md",
            "PUBLISHING.md",
            "uv.lock",
            "MANIFEST.in",
            ".python-version",
            "src/vectors_sdk/py.typed",
            "src/vectors_sdk/_version.py",
            "tests/test_sdk.py",
            "tests/test_live.py",
            "tools/generate_async.py",
            "tools/check_dist.py",
            "examples/quickstart.py",
        }
        if not required.issubset(names):
            raise SystemExit(f"sdist is missing: {sorted(required - names)}")
        if any(
            "__pycache__" in name or name.endswith(".pyc") or name.startswith(".venv/")
            for name in names
        ):
            raise SystemExit("sdist includes local environment or cache files")

    subprocess.run(
        [sys.executable, "-m", "twine", "check", "--strict", str(wheel), str(sdist)],
        check=True,
    )
    if install:
        tests = Path(__file__).resolve().parents[1] / "tests"
        smoke = "from importlib.metadata import version; import vectors_sdk; assert vectors_sdk.__version__ == version('vectors-sdk'); print(vectors_sdk.__version__, vectors_sdk.__file__)"
        for artifact in (wheel, sdist):
            command = [
                "uv",
                "run",
                "--isolated",
                "--no-project",
                "--python",
                sys.executable,
                "--with",
                str(artifact.resolve()),
                "python",
            ]
            subprocess.run([*command, "-c", smoke], check=True)
            subprocess.run(
                [*command, "-m", "unittest", "discover", "-s", str(tests), "-v"],
                check=True,
            )
    print(f"Verified vectors-sdk {expected}: wheel, sdist, metadata, and typing marker")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument(
        "--install",
        action="store_true",
        help="run SDK tests from isolated wheel and sdist installs",
    )
    arguments = parser.parse_args()
    check(arguments.directory, arguments.install)

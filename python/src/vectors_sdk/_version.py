"""The installed distribution's version is defined by pyproject.toml."""

from importlib.metadata import PackageNotFoundError, version

try:
    __version__ = version("vectors-sdk")
except PackageNotFoundError:
    # Allow source-only development without claiming an installed release.
    __version__ = "0+uninstalled"

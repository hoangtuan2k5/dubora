"""Compatibility entry point for the standalone Google Drive helper."""

import sys

from python_reference.dubora_reference import drive_helper as _implementation

sys.modules[__name__] = _implementation

if __name__ == "__main__":
    _implementation.main()

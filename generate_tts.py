"""Compatibility entry point for the Python reference pipeline."""

import sys

from python_reference.dubora_reference import generate_tts as _implementation

sys.modules[__name__] = _implementation

if __name__ == "__main__":
    _implementation.main()

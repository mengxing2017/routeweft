#!/usr/bin/env python3
"""Compatibility entry point; use release.py for public and private registries."""
import runpy
from pathlib import Path

if __name__ == '__main__':
    runpy.run_path(str(Path(__file__).with_name('release.py')), run_name='__main__')

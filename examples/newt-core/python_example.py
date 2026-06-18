# Example: Using newt-core from Python
# This demonstrates importing the Rust crate as a Python module via pyo3.

import sys
import os

# Add the workspace root to the path so we can import the Python bindings
sys.path.insert(0, os.path.abspath('.'))
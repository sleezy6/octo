"""The shim's lookup cache goes to a temporary file in tests, never to the deploy path."""
import atexit
import os
import shutil
import tempfile

_DIR = tempfile.mkdtemp(prefix="ytshim-test-")
atexit.register(shutil.rmtree, _DIR, True)
os.environ.setdefault("META_DB_PATH", os.path.join(_DIR, "meta.db"))

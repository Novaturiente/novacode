"""Runs every test_* function in test_*.py files; exits 1 on any failure."""
import glob, importlib, sys, traceback

failed = 0
for f in sorted(glob.glob("test_*.py")):
    mod = importlib.import_module(f[:-3])
    for name in sorted(n for n in dir(mod) if n.startswith("test_")):
        try:
            getattr(mod, name)()
            print(f"ok   {f}::{name}")
        except Exception:
            failed += 1
            print(f"FAIL {f}::{name}")
            traceback.print_exc()
print(f"{'FAILED' if failed else 'passed'} ({failed} failing)")
sys.exit(1 if failed else 0)

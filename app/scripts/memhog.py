#!/usr/bin/env python3
"""Test-only memory hog: allocates and touches N GB of incompressible data, then holds it.

  python3 app/scripts/memhog.py <GB> [seconds]

It exits by itself after `seconds` (default 60, at most 600) or on Ctrl-C / kill, so a forgotten
hog never leaves the Mac under pressure. At most 8 GB. Random bytes, so macOS cannot compress the
pages away: the pressure is real. Prints "holding" once every page is touched.
"""
import os
import sys
import time

gb = float(sys.argv[1])
secs = min(float(sys.argv[2]) if len(sys.argv) > 2 else 60, 600)
if not 0 < gb <= 8:
    sys.exit("memhog: GB must be more than 0 and at most 8")
CHUNK = 64 << 20
held = [os.urandom(CHUNK) for _ in range(int(gb * (1 << 30)) // CHUNK)]
print(f"holding {len(held) * CHUNK >> 20} MB for {secs:.0f} s (pid {os.getpid()})", flush=True)
end = time.time() + secs
while time.time() < end:
    time.sleep(0.5)

# tools/e2e_smoke.py — pty-driven end-to-end test of tools/shell.lua.
# Run from anywhere:  python3 tools/e2e_smoke.py   (drives the real shell on a
# pseudo-terminal, raw bytes in/out, asserts the DoD behaviors).
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from e2e_shell import Shell, check, gdb_orphans, FAILURES

print("=== scenario: smoke streams through the shell ===", flush=True)
s = Shell()
ok = s.expect("glm> ", 20)
check("smoke shell starts", ok)
s.buf = b""
s.send("smoke\r")
t0 = time.time()
ok = s.expect("smoke: all green", 550, echo=True)
print(flush=True)
check("DoD4 smoke streams and ends all green", ok, f"after {time.time()-t0:.0f}s; tail: {s.tail(300)!r}")
s.send("exit\r")
ok = s.wait_exit(20)
check("smoke shell exits", ok)
sane, why = s.tty_sane()
check("smoke shell terminal sane", sane, why)
s.close()
time.sleep(0.5)
n = gdb_orphans()
check("smoke shell no orphaned rust-gdb", n == 0, f"{n} rust-gdb process(es)")
print("\n" + ("ALL GREEN" if not FAILURES else f"FAILURES: {FAILURES}"))
sys.exit(0 if not FAILURES else 1)

# tools/e2e_extra.py — pty-driven end-to-end test of tools/shell.lua.
# Run from anywhere:  python3 tools/e2e_extra.py   (drives the real shell on a
# pseudo-terminal, raw bytes in/out, asserts the DoD behaviors).
import sys, time, os, select
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import e2e_shell as E
from e2e_shell import Shell, check, FAILURES

print("=== scenario: face=human, keys rebind, unknown key, b hotkey ===", flush=True)
# 1. startup option picks the human face (now also the default; the
#    explicit face=human spelling stays pinned)
s = Shell(args=["face=human"])

ok = s.expect(["glm> ", "face: human"], 20)
check("face=human startup option shown in banner", ok and b"face: human" in s.buf, s.tail(300))

# 2. human-face batch run actually loads the human shim (dashboard rows)
s.buf = b""
s.send("run cases/explore_join_phi.lua\r")
ok = s.expect(["landed at the glm_exec boundary"], 120)
import re as _re
clean = _re.sub(r"\x1b\[[0-9;]*[a-zA-Z]", "", s.buf.decode(errors="replace"))
ok = ok and "EFL: 0x" in clean
check("face human: batch run carries the dashboard", ok, s.tail(300))

# 3. switch back to agent for the remaining checks
s.send("face agent\r")
ok = s.expect("[shell] face = agent", 5)
check("`face` command switches faces", ok)

# 4. keys listing + rebind
s.buf = b""
s.send("keys\r")
ok = s.expect("[hotkeys] keymap", 5)
check("`keys` lists the keymap", ok)
s.buf = b""
s.send("keys X parg arg\r")
ok = s.expect("[keys] X = parg arg", 5)
check("`keys KEY cmd` rebinds", ok)

# 5. hotkey mode: unknown key prints the keymap; X uses the rebind; b prompts
s.buf = b""
s.send("debug cases/21_interop_alloc_arg_header.lua 9\r")
ok = s.expect("Breakpoint 1, glm_exec (arg=", 90)
check("debug lands (agent face again)", ok, s.tail(200))
s.buf = b""
s.send("z")
ok = s.expect(["[hotkeys] unbound key: z", "[hotkeys] keymap"], 10)
check("unknown key prints the keymap", ok, s.tail(200))
s.buf = b""
s.send("X")
ok = s.expect("{tbl:0x", 10)
check("rebound X runs `parg arg`", ok, s.tail(200))
s.buf = b""
s.send("b")                       # prompt for a break argument
ok = s.expect("break> ", 10)
check("b hotkey prompts for a break arg", ok)
s.send("21_interop_alloc_arg_header.lua:14\r")
ok = s.expect("Breakpoint 2", 15)
check("break argument sets a breakpoint at the Lua line", ok, s.tail(200))

s.send("\x1b")
ok = s.expect(["[debug] gdb closed", "glm> "], 20)
check("ESC back to line mode", ok)
s.send("exit\r")
ok = s.wait_exit(20)
sane, why = s.tty_sane()
check("exit + terminal sane", ok and sane, why)
s.close()
time.sleep(0.5)
n = E.gdb_orphans()
check("no orphaned rust-gdb", n == 0, f"{n}")
print("\n" + ("ALL GREEN" if not FAILURES else f"FAILURES: {FAILURES}"))
sys.exit(0 if not FAILURES else 1)

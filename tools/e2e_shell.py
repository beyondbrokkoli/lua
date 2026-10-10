# tools/e2e_shell.py — pty-driven end-to-end test of tools/shell.lua.
# Run from anywhere:  python3 tools/e2e_shell.py   (drives the real shell on a
# pseudo-terminal, raw bytes in/out, asserts the DoD behaviors).
#!/usr/bin/env python3
"""End-to-end pty driver for tools/shell.lua — drives the real shell on a
pseudo-terminal the way a user would (raw bytes in, raw bytes out) and
asserts the definition-of-done behaviors."""
import os, pty, select, termios, time, fcntl, struct, sys, subprocess, re

# Resolved from this script's home: tools/e2e_*.py -> repo root.
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FAILURES = []

# gdb on a tty (both live faces) styles its OWN message lines — the stop
# line's function name and even the `arg` parameter name ride in escapes
# (`Breakpoint 1, \x1b[33mglm_exec\x1b[m (\x1b[36marg\x1b[m=0x...`), gated
# on the driving terminal's TERM — so a raw-byte marker splits mid-pattern
# under one TERM and not another. Markers therefore match against an
# ANSI-stripped mirror of the buffer: the same convention smoke.sh's
# check() applies to its transcripts.
ESC_RE = re.compile(rb"\x1b\[[0-9;]*[a-zA-Z]")

def check(name, ok, detail=""):
    tag = "PASS" if ok else "FAIL"
    line = f"{tag}  {name}"
    if detail and not ok:
        line += f"  — {detail}"
    print(line, flush=True)
    if not ok:
        FAILURES.append(name)

class Shell:
    def __init__(self, args=None):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 110, 0, 0))
        self.pid = os.fork()
        if self.pid == 0:
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
            os.dup2(slave, 0); os.dup2(slave, 1); os.dup2(slave, 2)
            if slave > 2: os.close(slave)
            os.chdir(REPO)
            os.execvp("luajit", ["luajit", "tools/shell.lua"] + (args or []))
            os._exit(127)
        os.close(slave)
        self.buf = b""

    def send(self, data):
        os.write(self.master, data if isinstance(data, bytes) else data.encode())

    def expect(self, patterns, timeout, echo=False):
        if isinstance(patterns, (str, bytes)):
            patterns = [patterns]
        pats = [p.encode() if isinstance(p, str) else p for p in patterns]
        deadline = time.time() + timeout
        while time.time() < deadline:
            if all(p in ESC_RE.sub(b"", self.buf) for p in pats):
                return True
            r, _, _ = select.select([self.master], [], [], 0.2)
            if self.master in r:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                if echo:
                    sys.stdout.write(chunk.decode(errors="replace")); sys.stdout.flush()
                self.buf += chunk
        return all(p in ESC_RE.sub(b"", self.buf) for p in pats)

    def mark(self):
        return len(self.buf)

    def tail(self, n=200):
        return self.buf[-n:].decode(errors="replace")

    def wait_exit(self, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            r, _, _ = select.select([self.master], [], [], 0.2)
            if self.master in r:
                try: chunk = os.read(self.master, 65536)
                except OSError: return True
                if chunk: self.buf += chunk
            done, _ = os.waitpid(self.pid, os.WNOHANG)
            if done == self.pid:
                # drain whatever remains
                while True:
                    r, _, _ = select.select([self.master], [], [], 0.3)
                    if self.master not in r: break
                    try:
                        c = os.read(self.master, 65536)
                    except OSError: break
                    if not c: break
                    self.buf += c
                return True
        return False

    def tty_sane(self):
        try:
            l = termios.tcgetattr(self.master)
        except termios.error:
            return False, "tcgetattr failed"
        lflag, iflag = l[3], l[1]
        ok = bool(lflag & termios.ICANON) and bool(lflag & termios.ECHO) \
             and bool(iflag & termios.ISIG)
        return ok, f"lflag=0x{lflag:x} iflag=0x{iflag:x}"

    def close(self):
        try: os.close(self.master)
        except OSError: pass

def gdb_orphans():
    out = subprocess.run(["ps", "-eo", "comm"], capture_output=True, text=True).stdout
    return sum(1 for line in out.splitlines() if line.strip() == "rust-gdb")

CASE = "cases/21_interop_alloc_arg_header.lua"

def scenario_basics():
    print("\n=== scenario: banner / TAB / run / debug / ESC / exit ===", flush=True)
    s = Shell()
    ok = s.expect("glm> ", 20)
    check("DoD1 shell starts, banner + prompt", ok, s.tail())
    check("banner shows root + face", b"GLM GDB steering shell" in s.buf and b"face: human" in s.buf)

    # DoD 1: TAB completes `case 21_` to the full case name
    s.buf = b""
    s.send("case 21_"); time.sleep(0.3); s.send("\t")
    ok = s.expect("case 21_interop_alloc_arg_header.lua ", 5)
    check("DoD1 `case 21_<TAB>` completes to full name", ok, s.tail())

    # (natural follow-up) Enter runs `case` with the completed name: ARGS pin
    s.send("\r")
    ok = s.expect("ARGS: 9", 5)
    check("case <name> shows the ARGS pin", ok, s.tail())

    # DoD 2: batch run prints a transcript containing the glm_exec stop
    s.buf = b""
    s.send(f"run {CASE} 9\r")
    ok = s.expect(["Breakpoint 1, glm_exec", "landed at the glm_exec boundary"], 120, echo=False)
    check("DoD2 `run <case> 9` transcript has the glm_exec stop", ok, s.tail(400))

    # case-name normalization at the gdb boundary: bare stem, prefixed form
    s.buf = b""
    s.send("run 21_interop_alloc_arg_header 9\r")
    ok = s.expect("landed at the glm_exec boundary", 120)
    check("run accepts a bare stem (no dir, no .lua)", ok, s.tail(300))
    s.buf = b""
    s.send("run cases/explore_join_phi.lua\r")
    ok = s.expect("landed at the glm_exec boundary", 120)
    check("run accepts the cases/-prefixed form", ok, s.tail(300))

    # DoD 3: hotkey mode, BARE case name (no dir, no .lua) — wait for the
    # REAL stop ("Breakpoint 1, glm_exec") not the pending-breakpoint
    # notice ("Breakpoint 1 (glm_exec) pending.")
    s.buf = b""
    s.send("debug 21_interop_alloc_arg_header 9\r")
    ok = s.expect("Breakpoint 1, glm_exec (arg=", 90)
    check("DoD3 debug lands at the script boundary", ok, s.tail(400))

    # Default face is HUMAN: hook-stop's register dashboard must fire at
    # the FIRST live stop already (the registration landmine's pin). The
    # [Bytes] opcode row is contiguous ASCII inside the styled output.
    ok = s.expect(b"[Bytes]", 10)
    check("human default: per-stop dashboard fires live ([Bytes] row)", ok, s.tail(300))

    def wait_growth(min_bytes, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            r, _, _ = select.select([s.master], [], [], 0.2)
            if s.master in r:
                try: chunk = os.read(s.master, 65536)
                except OSError: return False
                if not chunk: return False
                s.buf += chunk
                if len(s.buf) - mark >= min_bytes:
                    return True
        return len(s.buf) - mark >= min_bytes

    mark = s.mark()
    s.send("\x1b[C")            # RIGHT -> si
    ok = wait_growth(20, 15)
    check("DoD3 RIGHT steps, new output arrives", ok, f"grew {len(s.buf)-mark}B; tail: {s.tail(200)!r}")

    s.buf = b""
    s.send("s")                 # pstack — human face: annotated rows
    ok = s.expect("=== Stack window:", 15)
    check("DoD3 `s` prints the human stack window", ok, s.tail(300))

    s.send("\x1b")              # ESC -> line mode
    ok = s.expect(["[debug] gdb closed", "glm> "], 20)
    check("DoD3 ESC returns to line mode", ok, s.tail(200))

    # DoD 5: exit restores the tty and leaves no orphans
    s.send("exit\r")
    ok = s.wait_exit(20)
    check("DoD5 shell exits on `exit`", ok)
    sane, why = s.tty_sane()
    check("DoD5 terminal sane after exit (ICANON|ECHO|ISIG)", sane, why)
    s.close()
    time.sleep(0.5)
    n = gdb_orphans()
    check("DoD5 no orphaned rust-gdb", n == 0, f"{n} rust-gdb process(es)")

def scenario_agent_face():
    print("\n=== scenario: --face agent — terse live session ===", flush=True)
    s = Shell(args=["--face", "agent"])   # two-token spelling of the flag
    ok = s.expect(["glm> ", "face: agent"], 20)
    check("--face agent (two tokens) picks the agent face", ok, s.tail())
    s.buf = b""
    s.send("debug 21_interop_alloc_arg_header 9\r")
    ok = s.expect("Breakpoint 1, glm_exec (arg=", 90)
    check("agent debug lands at the boundary", ok, s.tail(400))
    time.sleep(1.0)            # let any (unwanted) dashboard output land
    check("agent stops stay silent (no dashboard)",
          b"[Bytes]" not in s.buf and b"[Ret/Stack]" not in s.buf)
    s.buf = b""
    s.send("s")                # pstack — agent face: one dense line
    ok = s.expect("stk{rsp:0x", 15)
    check("agent `s` prints the one-line stk{...} window", ok, s.tail(200))
    s.send("\x1b")             # ESC -> line mode
    ok = s.expect(["[debug] gdb closed", "glm> "], 20)
    check("agent ESC returns to line mode", ok, s.tail(200))
    s.send("exit\r")
    ok = s.wait_exit(20)
    sane, why = s.tty_sane()
    check("agent session exit restores tty", ok and sane, why)
    s.close()
    time.sleep(0.5)
    n = gdb_orphans()
    check("agent session: no orphaned rust-gdb", n == 0, f"{n} rust-gdb process(es)")

def scenario_ctrlc():
    print("\n=== scenario: Ctrl+C restores and exits ===", flush=True)
    s = Shell()
    ok = s.expect("glm> ", 20)
    check("ctrl-c shell starts", ok)
    s.send("\x03")
    ok = s.wait_exit(15)
    check("ctrl-c exits the shell", ok)
    sane, why = s.tty_sane()
    check("ctrl-c terminal sane", sane, why)
    s.close()
    time.sleep(0.5)
    n = gdb_orphans()
    check("ctrl-c no orphaned rust-gdb", n == 0, f"{n} rust-gdb process(es)")

if __name__ == "__main__":
    scenario_basics()
    scenario_agent_face()
    scenario_ctrlc()
    print("\n" + ("ALL GREEN" if not FAILURES else f"FAILURES: {FAILURES}"))
    sys.exit(0 if not FAILURES else 1)

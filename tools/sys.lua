-- tools/sys.lua — the terminal + process discipline of the steering shell.
--
-- Harvested from legacy/tools/cli_sys.lua (the old demo_engine launcher):
--   * sleep_ms (ffi usleep)
--   * set_raw_mode / run_shell_cmd — the stty sandwich is kept in shape
--     (sane -> run command -> raw again), MANDATORY around every
--     os.execute that can touch the tty. One deliberate edit, recorded
--     here: the raw-mode enable adds `-isig` to the harvested
--     `stty cbreak -echo`. With ISIG off, Ctrl+C reaches this process as
--     the byte 0x03 — the CTRLC key the legacy editor already decoded —
--     instead of killing the shell via the default SIGINT disposition
--     with the terminal left raw. The shell additionally SIG_IGNs SIGINT
--     (pure C, no Lua callback) so the `stty sane` legs of the sandwich,
--     which re-enable ISIG, are safe for the shell too.
--   Dropped per the harvest brief: the Windows/_getch branch, the
--   platform gate that hard-exited, and the demo_engine orphan scans.
--   (The legacy's check_orphans io.popen ran inside raw mode without a
--   sandwich; that is fine for PIPE-wired children like io.popen("ls"),
--   which never see the tty. Every os.execute child goes through the
--   sandwich in run_shell_cmd.)
-- Plus the pty/poll ffi surface the live gdb side needs: forkpty from
-- <pty.h>, poll(2), waitpid, kill, ioctl(TIOCGWINSZ). Linux + LuaJIT only.
local ffi = require("ffi")
local bit = require("bit")

ffi.cdef[[
  typedef int pid_t;
  typedef long ssize_t;
  struct pollfd { int fd; short events; short revents; };
  int poll(struct pollfd *fds, unsigned long nfds, int timeout);
  ssize_t read(int fd, char *buf, size_t count);
  ssize_t write(int fd, const char *buf, size_t count);
  int close(int fd);
  int isatty(int fd);
  pid_t forkpty(int *amaster, char *name, const void *termp, const void *winp);
  int execvp(const char *file, const char *const argv[]);
  void _exit(int status);
  pid_t waitpid(pid_t pid, int *status, int options);
  int kill(pid_t pid, int sig);
  int ioctl(int fd, unsigned long request, ...);
  int chdir(const char *path);
  unsigned int usleep(unsigned int usec);
  struct timespec { long tv_sec; long tv_nsec; };
  int clock_gettime(int clk, struct timespec *tp);
  struct winsize { unsigned short ws_row; unsigned short ws_col;
                   unsigned short ws_xpixel; unsigned short ws_ypixel; };
  typedef void (*sighandler_t)(int);
  sighandler_t signal(int signum, sighandler_t handler);
]]

local C = ffi.C
local Sys = {}

-- poll(2) flags
Sys.POLLIN  = 0x001
Sys.POLLERR = 0x008
Sys.POLLHUP = 0x010
-- waitpid(2)
Sys.WNOHANG = 1
-- x86-64 Linux
Sys.TIOCGWINSZ = 0x5413
Sys.CLOCK_MONOTONIC = 1
Sys.EINTR = 4

local function stty(spec)
  os.execute("stty " .. spec .. " 2>/dev/null")
end

-- The one raw-mode definition (harvested cbreak -echo, plus -isig — see
-- the header). Both set_raw_mode(true) and the sandwich's re-entry leg
-- go through here so the discipline lives in exactly one place.
local function raw_on()
  stty("sane")
  stty("cbreak -echo -isig")
end

function Sys.set_raw_mode(enable)
  if enable then raw_on() else stty("sane") end
end

-- Harvested run_shell_cmd: children get a cooked tty (stty sane first),
-- the shell goes back to raw after. Mandatory around every os.execute
-- issued from the raw-mode session.
function Sys.run_shell_cmd(cmd)
  stty("sane")
  os.execute(cmd)
  raw_on()
end

function Sys.sleep_ms(ms)
  C.usleep(ms * 1000)
end

-- Pure-C SIGINT ignore (SIG_IGN == (void*)1): no Lua callback runs in
-- signal context. Ctrl+C is handled as an ordinary 0x03 byte instead,
-- which only reaches us while ISIG is off (the raw mode above).
function Sys.ignore_sigint()
  C.signal(2, ffi.cast("sighandler_t", 1))
end

-- Harvested from legacy/tools/cli_lobby.lua await_id (~27-42): poll a
-- file every 100 ms for a sentinel, bounded attempts. Generalized per
-- the brief: caller supplies the Lua pattern, file, and tries.
-- Returns the pattern's first match, or nil on timeout.
function Sys.wait_for(pattern, file, tries)
  tries = tries or 50
  for _ = 1, tries do
    local f = io.open(file, "r")
    if f then
      local content = f:read("*a")
      f:close()
      if content then
        -- Human-face transcripts carry ANSI (the dashboard's printf
        -- escapes, the core's rebuilt styling): strip before matching so
        -- a marker can never straddle an escape sequence.
        content = content:gsub("\27%[[0-9;]*[a-zA-Z]", "")
        local m = content:match(pattern)
        if m then return m end
      end
    end
    Sys.sleep_ms(100)
  end
  return nil
end

function Sys.shquote(s)
  return "'" .. (s:gsub("'", "'\\''")) .. "'"
end

function Sys.mono()
  local ts = ffi.new("struct timespec")
  C.clock_gettime(Sys.CLOCK_MONOTONIC, ts)
  -- tonumber: tv_sec is an ffi long — raw arithmetic on it stays cdata,
  -- which math.min and friends reject.
  return tonumber(ts.tv_sec) + tonumber(ts.tv_nsec) * 1e-9
end

-- poll one fd; returns "in" (readable), "hup" (HUP/ERR), false (timeout),
-- or "err" (poll itself failed).
function Sys.poll_fd(fd, timeout_ms)
  local fds = ffi.new("struct pollfd[1]")
  fds[0].fd = fd
  fds[0].events = Sys.POLLIN
  local r = C.poll(fds, 1, timeout_ms or -1)
  if r < 0 then return "err" end
  if r == 0 then return false end
  local rev = fds[0].revents
  if bit.band(rev, Sys.POLLIN) ~= 0 then return "in" end
  if bit.band(rev, bit.bor(Sys.POLLHUP, Sys.POLLERR)) ~= 0 then return "hup" end
  return false
end

-- One byte from fd (call after poll_fd said "in"); nil on EOF/error.
function Sys.read_byte(fd)
  local b = ffi.new("char[1]")
  local n = C.read(fd, b, 1)
  if n <= 0 then return nil end
  return bit.band(b[0], 0xFF)
end

-- A chunk from fd; "" on EINTR (retry-worthy), nil on EOF/error.
function Sys.read_chunk(fd, max)
  max = max or 8192
  local buf = ffi.new("char[?]", max)
  local n = C.read(fd, buf, max)
  if n > 0 then return ffi.string(buf, n) end
  if n < 0 and ffi.errno() == Sys.EINTR then return "" end
  return nil
end

function Sys.write_fd(fd, s)
  return tonumber(C.write(fd, s, #s))
end

function Sys.close_fd(fd)
  C.close(fd)
end

-- Real terminal size for the pty child, 24x80 when stdin is not a tty.
function Sys.tty_size(fd)
  local ws = ffi.new("struct winsize")
  if C.ioctl(fd or 0, Sys.TIOCGWINSZ, ws) == 0 and ws.ws_col > 0 then
    return ws.ws_row, ws.ws_col
  end
  return 24, 80
end

-- Spawn argv on a fresh pty (forkpty from <pty.h>). The child becomes a
-- session leader with the pty as controlling terminal, so it is its own
-- process group and kill(-pid) reaches it and its children. Returns
-- pid, master_fd.
function Sys.pty_spawn(argv, rows, cols)
  local ws = ffi.new("struct winsize")
  ws.ws_row = rows or 24
  ws.ws_col = cols or 80
  local master = ffi.new("int[1]")
  io.stdout:flush()
  io.stderr:flush()
  local pid = C.forkpty(master, nil, nil, ws)
  if pid < 0 then
    return nil, "forkpty: errno " .. tostring(ffi.errno())
  end
  if pid == 0 then
    -- const char*: LuaJIT only converts Lua strings to const char* —
    -- a plain char* array element assignment dies in the forked child.
    local cargv = ffi.new("const char *[?]", #argv + 1)
    for i = 0, #argv - 1 do cargv[i] = argv[i + 1] end
    C.execvp(argv[1], cargv)
    C._exit(127)
  end
  return pid, master[0]
end

-- WNOHANG reap; true + raw status when the pid is gone.
function Sys.reap(pid, block)
  local st = ffi.new("int[1]")
  local r = C.waitpid(pid, st, block and 0 or Sys.WNOHANG)
  if r == pid then return true, st[0] end
  return false
end

-- The forkpty child leads its own group: signal it, fall back to the
-- single pid if the group is somehow gone.
function Sys.kill_group(pid, sig)
  if C.kill(-pid, sig) == 0 then return true end
  return C.kill(pid, sig) == 0
end

function Sys.chdir(path)
  return C.chdir(path) == 0
end

return Sys

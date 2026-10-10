-- tools/gdb.lua — the gdb driver, both paths.
--
-- batch() copies the canonical batch form verbatim from gdb/smoke.sh's
-- gdb_run() (~line 163-172):
--   GLM_GDB_FACE=<face> rust-gdb -q -nx -x <shim> --batch -x <script> \
--     --args ./target/debug/glm --debug <case> [args...]
--   (run from the repo root, transcript captured to a file, path returned)
-- The LIVE side keeps ONE gdb child on a pseudo-terminal: forkpty makes
-- the child a session leader with the pty as its stdio, so the shell owns
-- the real keyboard, gdb reads its stdin from the pty, and its output is
-- drained off the master and echoed as it arrives. spawn()/send()/
-- drain()/alive()/kill() are the whole surface; kill() takes down the
-- child's whole process group, not just gdb itself.
local sys = require("tools.sys")

-- face default mirrors the shell's: HUMAN (the dashboard face). Callers
-- pass their own face everywhere; this only keeps direct module use in
-- sync with the shell's default.
local G = { _pid = nil, _fd = nil, _reaped = false, _n = 0, face = "human" }

local FACE_SHIM = { agent = ".gdbinit.agent", human = ".gdbinit" }

-- The case-name interface consensus: listings and TAB completion speak
-- BARE case names (what `ls cases` yields), while this boundary accepts
-- every spelling a human might reach for — bare with or without `.lua`,
-- `cases/`-prefixed with or without `.lua`, or an absolute path. One
-- normalizer, used by both spawn() and batch(): nothing that ever worked
-- stops working, and no caller munges paths itself.
function G.case_path(name)
  if name:match("^/") then return name end            -- absolute: trust it
  if not name:match("%.lua$") then name = name .. ".lua" end
  if not name:match("/") then return "cases/" .. name end
  return name
end

-- batch ------------------------------------------------------------------

-- One-shot batch run. script_lines default to `blua` (pending breakpoint
-- on glm_exec + run: land on the script boundary). Returns the
-- transcript path; the transcript is complete when this returns.
function G.batch(case, args, script_lines, face)
  face = face or G.face
  case = G.case_path(case)
  local script = os.tmpname()
  local f = io.open(script, "w")
  f:write(table.concat(script_lines or { "blua" }, "\n"), "\n")
  f:close()
  os.execute("mkdir -p target/shell 2>/dev/null")
  G._n = G._n + 1
  local tr = ("target/shell/batch_%d_%d.log"):format(os.time(), G._n)
  local quoted = {}
  for _, a in ipairs(args or {}) do quoted[#quoted + 1] = sys.shquote(a) end
  -- The canonical batch form, verbatim from gdb/smoke.sh gdb_run().
  local cmd = string.format(
    "GLM_GDB_FACE=%s rust-gdb -q -nx -x %s --batch -x %s" ..
    " --args ./target/debug/glm --debug %s %s > %s 2>&1",
    face, sys.shquote(FACE_SHIM[face] or ".gdbinit.agent"), sys.shquote(script),
    sys.shquote(case), table.concat(quoted, " "), sys.shquote(tr))
  sys.run_shell_cmd(cmd)
  os.remove(script)
  return tr
end

-- live -------------------------------------------------------------------

-- Spawn the live gdb child on a pty. Kills any previous one first.
function G.spawn(case, args, face)
  if G.alive() then G.kill() end
  local rows, cols = sys.tty_size(0)
  -- gdb's own options end at "--args": everything after is the debuggee's
  -- argv (the case's ARGS). -q silences the greeting the live pty would
  -- otherwise print; both flags must stay on this side of the border.
  local argv = {
    "rust-gdb", "-q", "-nx", "-x", FACE_SHIM[face or G.face] or ".gdbinit.agent",
    "--args", "./target/debug/glm", "--debug", G.case_path(case),
  }
  for _, a in ipairs(args or {}) do argv[#argv + 1] = a end
  local pid, fd = sys.pty_spawn(argv, rows, cols)
  if not pid then return nil, fd end
  G._pid, G._fd, G._reaped = pid, fd, false
  return true
end

function G.send(cmd)
  if not G.alive() then return false end
  return sys.write_fd(G._fd, cmd .. "\n") == #cmd + 1
end

-- Read the master until timeout_ms elapses overall or the output has
-- been quiet for quiet_ms (default 120), echoing every chunk as it
-- arrives so stop output lands live between keypresses. Returns the
-- newly arrived text.
function G.drain(timeout_ms, quiet_ms)
  local out = {}
  local deadline = sys.mono() + (timeout_ms or 200) / 1000
  local quiet = quiet_ms or 120                  -- ms; deadline is seconds
  while true do
    local left_ms = (deadline - sys.mono()) * 1000
    if left_ms <= 0 then break end
    local ev = sys.poll_fd(G._fd, math.min(left_ms, quiet))
    if ev == "in" then
      local chunk = sys.read_chunk(G._fd)
      if chunk == "" then
        -- EINTR: no data, keep polling
      elseif chunk then
        io.write(chunk)
        io.flush()
        out[#out + 1] = chunk
      else
        G._reaped = true            -- master EOF: gdb is gone
        break
      end
    elseif ev == "hup" then
      G._reaped = true
      break
    elseif ev ~= "in" then          -- quiet window elapsed, or poll error
      break
    end
  end
  return table.concat(out)
end

function G.alive()
  if not G._pid or G._reaped then return false end
  if sys.reap(G._pid, false) then
    G._reaped = true
    return false
  end
  return true
end

-- Kill the child's whole process group (the forkpty child is a session
-- leader), escalate to SIGKILL, reap, close the master. Safe to call
-- twice; called from the shell's every-exit-path restore.
function G.kill()
  local pid, fd = G._pid, G._fd
  if not pid then return end
  G._pid, G._fd = nil, nil
  if not G._reaped and not sys.reap(pid, false) then
    sys.kill_group(pid, 15)                       -- SIGTERM
    local deadline = sys.mono() + 1.0
    while sys.mono() < deadline do
      if sys.reap(pid, false) then break end
      sys.sleep_ms(25)
    end
    if not sys.reap(pid, false) then
      sys.kill_group(pid, 9)                      -- SIGKILL stragglers
      sys.reap(pid, true)
    end
  end
  -- Echo whatever the dying child still had in flight, then drop the pty.
  if fd then
    local until_ = sys.mono() + 0.2
    while sys.mono() < until_ do
      local ev = sys.poll_fd(fd, 50)
      if ev ~= "in" then break end
      local chunk = sys.read_chunk(fd)
      if not chunk then break end
      io.write(chunk)
    end
    io.flush()
    sys.close_fd(fd)
  end
end

-- Orderly exit: ask gdb to quit (confirm off in the core config makes it
-- immediate even with a live inferior), drain its goodbye, then make
-- sure it is dead.
function G.quit_gdb()
  if G.alive() then
    G.send("quit")
    G.drain(800, 200)
  end
  G.kill()
end

return G

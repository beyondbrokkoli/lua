-- tools/shell.lua — entry point: luajit tools/shell.lua [--face agent|human]
--
-- Skeleton harvested from legacy/launch.lua (~51-57): raw mode on,
-- pcall(main_loop), raw mode off, error report — the terminal is restored
-- on EVERY exit path (normal exit, error, Ctrl+C, EOF), and the same
-- restore path kills the live gdb child: no orphaned rust-gdb.
-- The legacy's if/elseif dispatch with its hand-synced duplicate
-- command-name array is NOT ported — tools/commands.lua's registry is the
-- single source of truth for both dispatch and TAB completion.
--
-- The repo root is resolved from arg[0] (the Lua equivalent of the
-- legacy/launch.sh line-4 `cd "$(dirname "$0")"`) and chdir'd to first:
-- the gdb shims, ./target/debug/glm and cases/ are all root-relative.

-- Resolve the repo root before any require: package.path must find
-- tools/*.lua no matter the invocation cwd.
local function repo_root()
  if not arg or not arg[0] then
    error("run as a script: luajit tools/shell.lua (arg[0] missing)")
  end
  local p = io.popen("readlink -f " ..
    "'" .. arg[0]:gsub("'", "'\\''") .. "'")
  local abs = p and p:read("*l") or ""
  if p then p:close() end
  return abs:match("^(.*)/tools/") or abs:match("^(.*)/")
end

local root = repo_root()
if not root or root == "" then
  error("cannot resolve the repo root from arg[0]=" .. tostring(arg and arg[0]))
end
package.path = root .. "/?.lua;" .. package.path

local sys = require("tools.sys")
if not sys.chdir(root) then
  error("chdir " .. root .. " failed")
end
local rl = require("tools.readline")
local gdb = require("tools.gdb")
local commands = require("tools.commands")

-- Shell option: face=agent (or --face agent / --face=agent) overrides the
-- HUMAN default. The shell is the human steering surface: the human shim
-- brings the per-stop register dashboard, colors, and annotated output
-- free (same core, other face). The agent face stays one flag away for
-- terse sessions — and the pure-gdb batch flow agents actually use never
-- needed the shell at all.
local face = "human"
for i, a in ipairs(arg) do
  local f = a:match("^face=(%a+)$") or a:match("^%-%-face=(%a+)$")
  if not f and a == "--face" then f = arg[i + 1] end
  if f == "human" or f == "agent" then face = f end
end

-- Generic completion: word 1 = command names from the registry; word >= 2
-- = the named command's own complete().
local function completer(word_index, prefix, words)
  if word_index == 1 then
    local names = {}
    for n in pairs(commands.registry) do
      if n:sub(1, #prefix) == prefix then names[#names + 1] = n end
    end
    table.sort(names)
    return names
  end
  local entry = commands.registry[words[1] or ""]
  if entry and entry.complete then return entry.complete(word_index, prefix) end
  return nil
end

local function banner(ctx)
  local names = {}
  for n in pairs(commands.registry) do names[#names + 1] = n end
  table.sort(names)
  print("=======================================================")
  print(" GLM GDB steering shell  (live pty + batch, face: " .. ctx.face .. ")")
  print(" root: " .. ctx.root)
  print(" commands: " .. table.concat(names, " "))
  print(" TAB completes; Ctrl+C leaves the current mode")
  print("=======================================================")
end

local function main_loop(ctx)
  banner(ctx)
  while true do
    local line = rl.read("glm> ", completer)
    if not line then
      print("\n[shell] EOF/Ctrl+C — bye")
      return
    end
    local words = {}
    for w in line:gmatch("%S+") do words[#words + 1] = w end
    local name = words[1]
    if name and name ~= "" then
      local entry = commands.registry[name]
      if not entry then
        print("[shell] unknown command: " .. name .. "   (try 'help')")
      else
        local args = {}
        for i = 2, #words do args[#args + 1] = words[i] end
        local ok, res = pcall(entry.fn, ctx, args)
        if not ok then
          -- One command's failure must not take the shell (or the tty
          -- state) down with it: report, re-assert raw mode, continue.
          print("[shell] command error: " .. tostring(res))
          sys.set_raw_mode(true)
        elseif res == "exit" then
          return
        end
      end
    end
  end
end

-- Pure-C SIGINT ignore: Ctrl+C arrives as the 0x03 byte the editor maps
-- to CTRLC (see tools/sys.lua) instead of killing the process with the
-- tty left raw.
sys.ignore_sigint()

local ctx = { face = face, root = root, gdb = gdb, last_transcript = nil }

sys.set_raw_mode(true)
local ok, err = pcall(main_loop, ctx)
-- Every exit path lands here: normal exit, command error, Ctrl+C, EOF.
sys.set_raw_mode(false)
gdb.kill()                    -- no orphaned rust-gdb (non-negotiable)

if not ok then
  io.write("\n[shell] FATAL: " .. tostring(err) .. "\n")
  os.exit(1)
end
print("[shell] Exit")

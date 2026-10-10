-- tools/commands.lua — THE command registry, the single source of truth:
-- dispatch (shell.lua) and TAB completion (readline's injected completer,
-- shell.lua) both read this one table; no other file hardcodes a command
-- list. This replaces the harvested legacy/launch.lua if/elseif dispatch
-- with its hand-synced duplicate command-name array — that design flaw is
-- exactly what the registry exists to kill.
--
-- Entry shape: registry[name] = { fn = function(ctx, args) end,
--                 help = "...", complete = function(word_index, prefix) end }
-- complete() is consulted for word_index >= 2 (arguments); word 1 — the
-- command name itself — is completed generically over the registry keys.
local sys = require("tools.sys")
local rl = require("tools.readline")
local gdb = require("tools.gdb")

local M = { registry = {} }

-- Hotkey mode's REBINDABLE keymap: read_key name -> gdb command string.
-- The "@break" sentinel prompts for the break argument first. ESC/q and
-- CTRLC are mode-level (handled by the loop below), not map entries.
M.keymap = {
  RIGHT = "si",
  LEFT  = "finish",
  UP    = "up",
  DOWN  = "down",
  [" "] = "continue",
  b     = "@break",
  c     = "cregs",
  s     = "pstack",
  h     = "here",
  r     = "run",       -- re-run the inferior; the pending glm_exec bp lands again
}

function M.register(name, def)
  M.registry[name] = def
end

-- ---------------------------------------------------------------- helpers

local function case_names()
  local names = {}
  local p = io.popen("ls cases 2>/dev/null")
  if p then
    for name in p:lines() do
      if name:sub(-4) == ".lua" then names[#names + 1] = name end
    end
    p:close()
  end
  table.sort(names)
  return names
end

-- Harvested from conf.lua's corpus pins (~62): the one ARGS regex, so a
-- case listing says which argv the case wants (e.g. `9` for the interop
-- header case). Accepts any case spelling; gdb.case_path normalizes.
-- Returns pin (or nil) + whether the case file exists at all.
local function case_args_pin(user_name)
  local f = io.open(gdb.case_path(user_name), "r")
  if not f then return nil, false end
  local content = f:read("*a")
  f:close()
  return content:match("%-%-%s*ARGS:%s*([^\r\n]+)"), true
end

local function complete_case(word_index)
  if word_index ~= 2 then return nil end
  return case_names()
end

local function print_keymap()
  local ks = {}
  for k in pairs(M.keymap) do ks[#ks + 1] = k end
  table.sort(ks)
  print("[hotkeys] keymap (rebind: keys KEY gdb-cmd...):")
  for _, k in ipairs(ks) do
    local what = M.keymap[k] == "@break" and "prompt for a break arg" or M.keymap[k]
    io.write(("[hotkeys]   %-6s %s\n"):format(k, what))
  end
  print("[hotkeys]   q/ESC  back to line mode;  Ctrl+C  same")
end

-- ------------------------------------------------------------- hotkey mode

local function hotkey_mode(ctx, args)
  if not args[1] then
    print("usage: debug <case> [args...]   (TAB completes case names)")
    return
  end
  local rest = {}
  for i = 2, #args do rest[#rest + 1] = args[i] end

  local ok, err = gdb.spawn(args[1], rest, ctx.face)
  if not ok then
    print("[debug] gdb failed to start: " .. tostring(err))
    return
  end

  print("[debug] live gdb (pid " .. gdb._pid .. ") — single keys steer it")
  print_keymap()
  -- Land on the script boundary: blua = pending breakpoint on glm_exec +
  -- run. (pagination/confirm/height are already set by the core config;
  -- the agent face additionally runs style-free.)
  if ctx.face == "agent" then gdb.send("set style enabled off") end
  gdb.send("blua")
  gdb.drain(10000, 400)          -- gdb startup is DWARF-heavy; wait it out

  while true do
    gdb.drain(40, 40)            -- echo whatever arrived between keypresses
    if not gdb.alive() then
      print("\n[debug] gdb exited")
      gdb.kill()
      return
    end
    local key = rl.read_key(40)
    if key == "CTRLC" or key == "EOF" or key == "ESC" or key == "q" then
      break
    end
    if key then
      local cmd = M.keymap[key]
      if not cmd then
        -- Unknown key prints the keymap (the mode's only help surface).
        print("\n[hotkeys] unbound key: " .. key)
        print_keymap()
      elseif cmd == "@break" then
        local line = rl.read("break> ")
        if line and line ~= "" then gdb.send("break " .. line) end
      else
        gdb.send(cmd)
      end
    end
  end

  gdb.quit_gdb()
  print("[debug] gdb closed — back to line mode")
end

-- ---------------------------------------------------------------- registry

M.register("case", {
  help = "list the corpus; `case <name>` shows its ARGS pin",
  complete = complete_case,
  fn = function(_, args)
    if args[1] then
      local pin, exists = case_args_pin(args[1])
      if not exists then
        print("no such case: " .. args[1])
        return
      end
      print(args[1] .. (pin and ("   ARGS: " .. pin) or "   (no ARGS pin)"))
      return
    end
    for _, name in ipairs(case_names()) do
      local pin = case_args_pin(name)
      print("  " .. name .. (pin and ("   ARGS: " .. pin) or ""))
    end
  end,
})

M.register("run", {
  help = "run <case> [args...] — one-shot batch gdb run; prints the transcript",
  complete = complete_case,
  fn = function(ctx, args)
    if not args[1] then
      print("usage: run <case> [args...]   (TAB completes case names)")
      return
    end
    -- Normalization lives at the gdb boundary (gdb.case_path): any
    -- spelling — bare, cases/-prefixed, with or without .lua — works.
    local rest = {}
    for i = 2, #args do rest[#rest + 1] = args[i] end
    local tr = gdb.batch(args[1], rest, nil, ctx.face)
    local f = io.open(tr, "r")
    if f then
      io.write(f:read("*a") or "")
      f:close()
    end
    -- Harvested wait_for: await the transcript's boundary marker (the
    -- blua stop) before pronouncing the run landed.
    local m = sys.wait_for("glm_exec", tr, 20)
    ctx.last_transcript = tr
    if m then
      print("[run] landed at the glm_exec boundary  ✓  " .. tr)
    else
      print("[run] NO glm_exec stop in the transcript  ✗  " .. tr)
    end
  end,
})

M.register("debug", {
  help = "debug <case> [args...] — live gdb on a pty; single-key hotkeys",
  complete = complete_case,
  fn = hotkey_mode,
})

M.register("smoke", {
  help = "smoke [agent|human|both] — ./gdb/smoke.sh streamed through the stty sandwich",
  complete = function(wi) if wi == 2 then return { "agent", "human", "both" } end end,
  fn = function(_, args)
    local face = args[1] or "agent"
    if face ~= "agent" and face ~= "human" and face ~= "both" then
      print("smoke face is agent|human|both")
      return
    end
    sys.run_shell_cmd("./gdb/smoke.sh --face " .. face .. " 2>&1")
  end,
})

M.register("face", {
  help = "face [agent|human] — which gdbinit shim subsequent spawns load",
  complete = function(wi) if wi == 2 then return { "agent", "human" } end end,
  fn = function(ctx, args)
    if args[1] == "agent" or args[1] == "human" then
      ctx.face = args[1]
      gdb.face = args[1]
      print("[shell] face = " .. ctx.face)
    else
      print("[shell] face = " .. tostring(ctx.face) .. "   (agent|human)")
    end
  end,
})

M.register("keys", {
  help = "keys [KEY [gdb command...]] — list or rebind the hotkey keymap",
  complete = function(wi)
    if wi ~= 2 then return nil end
    local ks = {}
    for k in pairs(M.keymap) do ks[#ks + 1] = k end
    table.sort(ks)
    return ks
  end,
  fn = function(_, args)
    if not args[1] then print_keymap() return end
    if not args[2] then
      print("[keys] " .. args[1] .. " = " .. (M.keymap[args[1]] or "(unbound)"))
      return
    end
    local cmd = table.concat(args, " ", 2)
    M.keymap[args[1]] = cmd
    print("[keys] " .. args[1] .. " = " .. cmd)
  end,
})

M.register("help", {
  help = "help [cmd] — the registry, or one entry",
  complete = function(wi)
    if wi ~= 2 then return nil end
    local names = {}
    for n in pairs(M.registry) do names[#names + 1] = n end
    table.sort(names)
    return names
  end,
  fn = function(_, args)
    if args[1] and M.registry[args[1]] then
      print(args[1] .. " — " .. (M.registry[args[1]].help or ""))
      return
    end
    local names = {}
    for n in pairs(M.registry) do names[#names + 1] = n end
    table.sort(names)
    for _, n in ipairs(names) do
      print("  " .. n .. " — " .. (M.registry[n].help or ""))
    end
  end,
})

M.register("exit", {
  help = "exit — restore the terminal, kill the gdb child, leave",
  fn = function() return "exit" end,
})

return M

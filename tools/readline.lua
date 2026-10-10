-- tools/readline.lua — the harvested line editor + a symbolic read_key.
--
-- Harvested from legacy/tools/cli_readline.lua:
--   * the escape-sequence decoder (\27 [ A/B/C/D -> UP/DOWN/RIGHT/LEFT,
--     every other escape IGNORE) — the old demo_engine demo's Windows
--     _getch branch is dropped, per the brief;
--   * the cursor redraw (\r\27[K + prompt + column restore);
--   * history with UP/DOWN navigation and adjacent-duplicate dedup.
-- REFACTOR per the brief: read_key() returns the SYMBOLIC key name, so
-- hotkey mode can bind single keys, and the line editor is built on top
-- of it instead of carrying its own decoder.
-- Two behavior fixes over the original, both needed here:
--   * the decoder's follow-up bytes are read with a short poll timeout,
--     so a lone ESC press is "ESC" instead of hanging the original's
--     blocking io.read(1);
--   * TAB with several candidates lists them (the original silently did
--     nothing) — `case <TAB>` needs that to browse the corpus.
-- The lobby-id completion is NOT ported: completion is delegated to an
-- injected completer(word_index, prefix[, words]) -> candidates.
local sys = require("tools.sys")

local R = { history = {}, prompt_str = "glm> " }

-- One keypress as a symbolic name: printable keys as their 1-char
-- string, control keys as upper-case names. timeout_ms nil blocks.
-- Returns nil on timeout; "EOF" / "CTRLC" on those.
function R.read_key(timeout_ms)
  local ev = sys.poll_fd(0, timeout_ms)
  if ev == false then return nil end
  if ev ~= "in" then return "EOF" end
  local b = sys.read_byte(0)
  if not b then return "EOF" end
  if b == 3 then return "CTRLC" end
  if b == 4 then return "EOF" end           -- Ctrl+D: EOF on a raw tty
  if b == 127 or b == 8 then return "BACKSPACE" end
  if b == 9 then return "TAB" end
  if b == 13 or b == 10 then return "ENTER" end
  if b == 27 then
    -- Harvested escape decode (\27 [ A/B/C/D), with a 25 ms poll on the
    -- follow-up bytes so a lone ESC stays a lone ESC.
    if sys.poll_fd(0, 25) ~= "in" then return "ESC" end
    local b2 = sys.read_byte(0)
    if b2 == 91 or b2 == 79 then            -- '[' or 'O' (cursor/app keys)
      if sys.poll_fd(0, 25) ~= "in" then return "IGNORE" end
      local b3 = sys.read_byte(0)
      if b3 == 65 then return "UP" end
      if b3 == 66 then return "DOWN" end
      if b3 == 67 then return "RIGHT" end
      if b3 == 68 then return "LEFT" end
    end
    return "IGNORE"
  end
  if b >= 32 and b < 127 then return string.char(b) end
  return "IGNORE"
end

local function split_words(buf)
  local words = {}
  for w in buf:gmatch("%S+") do words[#words + 1] = w end
  return words
end

-- Which word TAB would complete: 1-based word index + its prefix.
-- Word 1 is the command name; a trailing space starts a new empty word.
function R.completion_point(buf)
  local words = split_words(buf)
  if buf:match("%s$") then return #words + 1, "" end
  return #words, words[#words] or ""
end

-- The harvested editor. completer(word_index, prefix[, words]) returns
-- the candidate list for the word being completed (nil = no ideas).
-- Returns the entered line, or nil on CTRLC/EOF.
function R.read(prompt, completer)
  local buf = ""
  local cursor = 0
  local hist_idx = #R.history + 1
  prompt = prompt or R.prompt_str
  local prompt_len = #prompt

  -- Harvested redraw: clear the line, re-emit prompt+buffer, restore the
  -- cursor column when it is not at the end.
  local function redraw()
    io.write("\r\27[K" .. prompt .. buf)
    if cursor < #buf then
      io.write("\r\27[" .. (prompt_len + cursor) .. "C")
    end
    io.flush()
  end

  io.write(prompt)
  io.flush()

  while true do
    local c = R.read_key(nil)
    if c == "EOF" or c == "CTRLC" then
      return nil
    elseif c == "ENTER" then
      io.write("\n")
      io.flush()
      buf = buf:gsub("\r", "")
      if buf ~= "" and R.history[#R.history] ~= buf then
        table.insert(R.history, buf)          -- harvested adjacent-dedup
      end
      return buf
    elseif c == "BACKSPACE" then
      if cursor > 0 then
        buf = buf:sub(1, cursor - 1) .. buf:sub(cursor + 1)
        cursor = cursor - 1
        redraw()
      end
    elseif c == "LEFT" then
      if cursor > 0 then cursor = cursor - 1; redraw() end
    elseif c == "RIGHT" then
      if cursor < #buf then cursor = cursor + 1; redraw() end
    elseif c == "UP" or c == "DOWN" then
      -- Harvested history walk.
      if c == "UP" and hist_idx > 1 then hist_idx = hist_idx - 1
      elseif c == "DOWN" and hist_idx <= #R.history then hist_idx = hist_idx + 1 end
      buf = R.history[hist_idx] or ""
      cursor = #buf
      redraw()
    elseif c == "TAB" then
      local idx, prefix = R.completion_point(buf)
      local matches = {}
      if completer then
        local cands = completer(idx, prefix, split_words(buf))
        for _, cand in ipairs(cands or {}) do
          if cand:sub(1, #prefix) == prefix then matches[#matches + 1] = cand end
        end
      end
      if #matches == 1 then
        buf = buf:sub(1, #buf - #prefix) .. matches[1] .. " "
        cursor = #buf
        redraw()
      elseif #matches > 1 then
        io.write("\n" .. table.concat(matches, "  ") .. "\n")
        io.flush()
        redraw()
      end
    elseif c ~= "IGNORE" and c ~= "ESC" then
      buf = buf:sub(1, cursor) .. c .. buf:sub(cursor + 1)
      cursor = cursor + 1
      redraw()
    end
  end
end

return R

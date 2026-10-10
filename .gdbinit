# /home/halim/lua/.gdbinit — thin shim: the HUMAN face of gdb/glm.gdbinit.
#
# Auto-loaded whenever gdb starts in the repo root (plain `rust-gdb` from
# ~/lua), or explicitly: rust-gdb -q -nx -x .gdbinit --args ...
#
# The human face of the one core (see gdb/glm.gdbinit): the same toolset as the
# agent face PLUS the per-stop register dashboard, ANSI colors everywhere,
# annotated parg/pcells/xq/xqv output, renumbered cbt frames, highlighted
# igrep matches. All landmines documented in AGENTS.md apply in full.
#
# No default breakpoints: an automatic `b glm::main` hijacks every `run` (the
# host's main stops before the pending Lua-line breakpoint fires), so scripted
# faces and `blua`/`to_script` lose their landing spot. Break where you want.
#
# Smoke: ./gdb/smoke.sh --face human [config]   (a --config argument tests a
# candidate shim — or a bare candidate core, via GLM_GDB_FACE — in place)

set $glm_face_human = 1
source gdb/glm.gdbinit

# --- human-only display settings (the agent shim sets the terse forms) ----
set print pretty on
set print array on
set print array-indexes on
set print symbol off
set disassemble-next-line off
set history save on

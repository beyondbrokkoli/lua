# ~/lua/gdb/glm.gdbinit — the ONE core GDB config for the GLM compiler & runtime.
#
# Every debugging face is this file plus a two-line shim:
#   .gdbinit        — human face (shim sets $glm_face_human = 1)
#   .gdbinit.agent  — agent face (shim sets $glm_face_human = 0)
#
# Face selection happens at load time, first match wins:
#   1. GLM_GDB_FACE=agent|human in the environment — lets a draft core be
#      driven directly: GLM_GDB_FACE=human rust-gdb -q -nx -x gdb/glm.gdbinit ...
#   2. the $glm_face_human convenience variable, set by a shim;
#   3. default: agent — silence is the safer fallback for an unspecified face.
# The core re-publishes the verdict as $glm_face_human (ALWAYS initialized
# here, before any define body can run), so command bodies branch on it
# without the void-convenience-variable landmine.
#
# Documented variant points (everything else is shared byte-for-byte):
#   * hook-stop dashboard — human only (gated body; agent stops stay
#     silent). Its tail keeps the native x/1i line verbatim (gdb's own
#     disassembler styling — colored %rip-style registers, mnemonics —
#     and tab placement), preceded by _glm_opcodes_line: the instruction's
#     raw opcode bytes on their own [Bytes] row — x/1i cannot show bytes,
#     so that row is the Python API (architecture().disassemble +
#     inferior read_memory).
#   * _cell / parg / pcells / xq / xqv formatting — agent: compact one-line
#     output, human: labels, annotation, and color. parg prints `border`
#     in BOTH faces (one source, no raw +48 reads needed to see it).
#   * pkind — one wording (the agent's), green in the human face
#   * cbt numbering — agent: gdb-native frame numbers (`frame N` works);
#     human: renumbered over the PRINTED frames (walks 64 deep so elided
#     std frames don't consume the budget)
#   * igrep — agent: plain lines + `igrep: N match(es) in M instructions`;
#     human: lines wear gdb's own disassembler styling (rebuilt from
#     `show style` — to_string capture strips gdb's escapes), matches pop
#     bold-green on top + cyan summary line
#   * here — human colors the basename:line anchor
#   * `set print pretty/array/...` and history settings — belong to the
#     shims, never the core
#
# The landmines documented in AGENTS.md apply to this file in full: language
# sandwiches go through python to_string (never bare `set language` lines),
# _cell sets C WITHOUT restore, no convenience names collide with segment
# registers, $eflags is never cast, `breakpoint pending on` stays, Python
# command classes register at top level only, and $bufp is initialized
# before its first comparison.

# --- face selection -----------------------------------------------------
python
import os
_face = os.environ.get("GLM_GDB_FACE", "").strip().lower()
if _face in ("agent", "human"):
    GLM_HUMAN = _face == "human"
else:
    if _face:
        print("glm.gdbinit: GLM_GDB_FACE=%r is not agent|human; "
              "using the shim's variable" % _face)
    try:
        GLM_HUMAN = int(gdb.parse_and_eval("$glm_face_human")) != 0
    except Exception:
        GLM_HUMAN = False
gdb.execute("set $glm_face_human = %d" % (1 if GLM_HUMAN else 0))
# Always-initialized toggles the define bodies branch on (the void-var
# landmine): $glm_stk_watch gates pstack's per-stop printing in hook-stop.
gdb.execute("set $glm_stk_watch = 0", to_string=True)
end

# --- common non-interactive & output discipline -------------------------
# (face-divergent print/history settings live in the shims)
set debuginfod enabled off
set pagination off
set confirm off
set height 0
set width 0
set breakpoint pending on
set unwindonsignal on
set print inferior-events off
set print demangle on
set print asm-demangle on

# --- rustc sysroot source substitution ----------------------------------
set substitute-path /rustc/8bab26f4f68e0e26f0bb7960be334d5b520ea452 /usr/lib/rustlib/src/rust

# --- skip rules: dynamic loader, glibc, vdso, Rust std -------------------
# File globs keep si/ni out of ld.so/glibc objects; the -rfu regexes match
# demangled names so `step` never descends into std/core/alloc internals
# (the .* prefix catches trait impls like <T as core::...>). Issued through
# python to_string so the 18 "will be skipped" confirmation lines stay off
# the transcript — a must for the agent face, harmless for the human one.
python
for _s in [
    "skip -gfi /lib/**/*.so",
    "skip -gfi /lib64/**/*.so",
    "skip -gfi /usr/lib/**/*.so",
    "skip -gfi /usr/lib64/**/*.so",
    "skip -rfu .*std::.*",
    "skip -rfu .*core::.*",
    "skip -rfu .*alloc::.*",
    "skip -rfu .*panic_unwind::.*",
    "skip -gfi /rustc/*",
    "skip -gfi /usr/lib/rustlib/*",
    # Kernel/loader/libc faces nobody wants under their feet.
    "skip -rfu ^__libc_",
    "skip -rfu ^_dl_",
    "skip -rfu ^__vdso",
    "skip -rfu ^pthread_",
    "skip -rfu ^malloc$",
    "skip -rfu ^calloc$",
    "skip -rfu ^realloc$",
    "skip -rfu ^free$",
]:
    gdb.execute(_s, to_string=True)
end

# --- cregs: every GP register, one dense line (python: $eflags is a flags
# struct in modern gdb, so it renders as the set-flag names only) ---------
python
class _Cregs(gdb.Command):
    """cregs — all GP registers on one line; efl lists set flags only."""

    def __init__(self):
        super().__init__("cregs", gdb.COMMAND_NONE)

    def invoke(self, args, from_tty):
        parts = []
        for r in "rax rbx rcx rdx rsi rdi rbp rsp r8 r9 r10 r11 r12 r13 r14 r15 rip".split():
            try:
                parts.append("%s=0x%x" % (r, int(gdb.parse_and_eval("$" + r)) & 0xFFFFFFFFFFFFFFFF))
            except Exception:
                parts.append("%s=?" % r)
        try:
            # $eflags is an opaque flags type (no fields, no casts) — parse
            # the one-line info output: eflags 0x202 [ IF ]
            out = gdb.execute("info registers eflags", to_string=True).split()
            if len(out) >= 4 and out[2] == "[":
                parts.append("efl=%s[%s]" % (out[1], " ".join(out[3:-1])))
            else:
                parts.append("efl=%s" % out[1])
        except Exception:
            pass
        print(" ".join(parts))


_Cregs()
end

# --- _glm_opcodes_line (internal): the dashboard's [Bytes] row ------------
# The dashboard's instruction line is native x/1i, verbatim — gdb's own
# disassembler styling (colored %rip-style registers, mnemonics) and tab
# placement are the reference the human face wants, and the Python API's
# asm strings carry none of it. But x/1i cannot show the raw opcode bytes,
# so this companion row adds them on their own line:
# architecture().disassemble(pc, pc+1)[0] yields the exact instruction at
# pc (its `length` field sizes the byte read — never a fixed guess) and
# inferior read_memory pulls the opcodes out. Pure API end to end: it
# parses no C, hence no language sandwich even at Rust-DWARF frames, and
# it creates no convenience names (segment-register collisions stay
# impossible). Defined at TOP LEVEL so the hook's FIRST stop already finds
# the symbol (the registration landmine); every failure path prints
# nothing — the native x/1i line below is the anchor and must never be
# upstaged by an error (a raising hook-stop prints a traceback at every
# stop).
python
def _glm_opcodes_line():
    try:
        pc = int(gdb.selected_frame().pc()) & 0xFFFFFFFFFFFFFFFF
        length = int(gdb.selected_frame().architecture()
                     .disassemble(pc, pc + 1)[0].get("length") or 0)
        if not length:
            return
        code = bytes(gdb.selected_inferior().read_memory(pc, length))
    except Exception:
        return
    # [Bytes] padded to the dashboard's label column ([Ret/Stack],
    # [Scratch], [Saved] ...); dim cyan keeps the row present without
    # competing with the styled instruction line above it.
    print("\033[35m[Bytes]    \033[0m  \033[2;36m%s\033[0m"
          % " ".join("%02X" % b for b in code))


end

# --- hook-stop: the human face's per-stop register dashboard --------------
# Gated on the face: the agent face keeps fully silent stops (gdb's own
# file:line + source echo is the only per-stop context it wants). No
# language sandwich in the body: the register printf, the native x/1i,
# and the python [Bytes] row are language-independent (printf plus
# pure-API reads — proven at both Rust-DWARF and Lua frames), and a
# `set language c` here would print the "current language does not match
# this frame" warning at every Rust-frame stop. $eflags goes straight to
# printf — the struct converts to its integral value; NEVER cast it (any
# cast of it dies).
define hook-stop
  if $glm_face_human == 1
    printf "\n\e[35m[Ret/Stack]\e[0m  \e[1;36mRAX:\e[0m 0x%016lx  \e[1;36mRSP:\e[0m 0x%016lx  \e[1;36mRBP:\e[0m 0x%016lx\n", $rax, $rsp, $rbp

    printf "\e[35m[Args 1-3] \e[0m  \e[1;36mRDI:\e[0m 0x%016lx  \e[1;36mRSI:\e[0m 0x%016lx  \e[1;36mRDX:\e[0m 0x%016lx\n", $rdi, $rsi, $rdx

    printf "\e[35m[Args 4-6] \e[0m  \e[1;36mRCX:\e[0m 0x%016lx  \e[1;36mR8 :\e[0m 0x%016lx  \e[1;36mR9 :\e[0m 0x%016lx\n", $rcx, $r8, $r9

    printf "\e[35m[Scratch]  \e[0m  \e[1;36mR10:\e[0m 0x%016lx  \e[1;36mR11:\e[0m 0x%016lx  \e[1;32mRIP:\e[0m 0x%016lx\n", $r10, $r11, $rip

    printf "\e[35m[Saved]    \e[0m  \e[1;36mRBX:\e[0m 0x%016lx  \e[1;36mR12:\e[0m 0x%016lx  \e[1;36mR13:\e[0m 0x%016lx\n", $rbx, $r12, $r13
    printf "\e[35m[Saved]    \e[0m  \e[1;36mR14:\e[0m 0x%016lx  \e[1;36mR15:\e[0m 0x%016lx  \e[1;33mEFL:\e[0m 0x%016lx\n", $r14, $r15, $eflags

    # The raw opcode bytes first, on the [Bytes] row (x/1i cannot show
    # them; the row stays silent on any failure, never a traceback), then
    # exactly the next instruction, natively — gdb's own styling and
    # placement, verbatim, as the dashboard's last line.
    python _glm_opcodes_line()
    x/1i $pc
    echo \n
  end
  # The opt-in stack watch: `stkwatch on` breaks the agent face's silent
  # stops deliberately (one dense line) and appends to the human dashboard.
  if $glm_stk_watch != 0
    pstack
  end
end

# --- xd / regs: gdb-native memory & register views ------------------------
define xd
  if $argc == 0
    help xd
  else
    x/4gx $arg0
  end
end
document xd
  Dumps 4 consecutive 64-bit words (32 bytes) at the given memory address.
  Usage: xd <address>
end

define regs
  info registers rax rbx rcx rdx rsi rdi rbp rsp r8 r9 r10 r11 r12 r13 r14 r15 rip eflags
end
document regs
  Displays all 64-bit general-purpose registers and CPU flags, gdb-native.
end

# --- to_script / blua: run straight into the script boundary -------------
define to_script
  b glm_exec
  run
end
document to_script
  Pending breakpoint on @glm_exec, then run (lands at the script's first
  statement).
end

define blua
  to_script
end
document blua
  The agent-face name of to_script (kept for muscle memory).
end

# --- _cell (internal): decode one GlmTable cell at a known address --------
# Arg 1: cell address (char*). Arg 2: esize (1 bool / 8 scalar / 16 tagged
# Any). Arg 3: contains_tables flag (switches the 8-byte view to row
# pointers). Sets language c without restoring: the casts below die in
# Rust frames ("No symbol 'unsigned'"), and a restoring sandwich here
# would flip the language back mid-body of the public caller. Public
# commands (parg / pcells / xq / pkind) wrap themselves and restore; _cell
# left alone. Never strip the sandwiches.
define _cell
  python gdb.execute("set language c", to_string=True)
  if (unsigned long)$arg1 == 16
    set $pl = *(unsigned long*)$arg0
    set $tg = *(long*)((char*)$arg0+8)
    if $tg == 0
      if $glm_face_human == 1
        printf "\e[1;32m[INT]\e[0m    i:%ld u:0x%lx", (long)$pl, $pl
      else
        printf "int i:%ld u:0x%lx", (long)$pl, $pl
      end
    else
      if $tg == 1
        if $glm_face_human == 1
          printf "\e[1;34m[FLOAT]\e[0m  f:%g u:0x%lx", *(double*)((char*)$arg0), $pl
        else
          printf "float u:0x%lx f:%g", $pl, *(double*)((char*)$arg0)
        end
      else
        if $tg == 2
          if $glm_face_human == 1
            printf "\e[1;33m[BOOL]\e[0m   b:%d u:0x%lx", (int)(*(unsigned char*)$arg0), $pl
          else
            printf "bool u:0x%lx b:%d", $pl, (int)(*(unsigned char*)$arg0)
          end
        else
          if $tg == 3
            if $glm_face_human == 1
              printf "\e[1;35m[STR]\e[0m    \"%s\" u:0x%lx", (char*)$pl, $pl
            else
              printf "str u:0x%lx s:\"%s\"", $pl, (char*)$pl
            end
          else
            if $glm_face_human == 1
              printf "\e[1;31m[TAG:%ld]\e[0m 0x%lx", $tg, $pl
            else
              printf "tag%ld u:0x%lx", $tg, $pl
            end
          end
        end
      end
    end
  else
    if (unsigned long)$arg1 == 8
      if (unsigned char)$arg2 != 0
        if $glm_face_human == 1
          printf "\e[1;36m[ROW]\e[0m    0x%lx", *(unsigned long*)$arg0
        else
          printf "row 0x%lx", *(unsigned long*)$arg0
        end
      else
        set $v = *(unsigned long*)$arg0
        if $glm_face_human == 1
          printf "\e[1;32m[SCALAR]\e[0m i:%ld u:0x%lx f:%g", (long)$v, $v, *(double*)((char*)$arg0)
        else
          printf "i:%ld u:0x%lx f:%g", (long)$v, $v, *(double*)((char*)$arg0)
        end
      end
    else
      if $glm_face_human == 1
        printf "\e[1;33m[BOOL]\e[0m   b:%u", *(unsigned char*)$arg0
      else
        printf "b:%u", *(unsigned char*)$arg0
      end
    end
  end
end
document _cell
  Internal helper: one decoded cell, no newline. _cell <addr> <esize> <ct>
end

# --- parg: the GlmTable header -------------------------------------------
# GlmTable #[repr(C)]: data+0 len+8 reserve+16 esize+24 mode(u8)+32
# contains_tables(u8)+33 sparse_map+40 border(i64)+48. No arg: tries $rdi
# then $rbx (the boundary pointer at the glm_exec entry face). Inside a
# Lua frame prefer passing the variable directly: parg arg / parg t.
# Both faces print border (the agent pseudo-JSON closes with it; the
# human face annotates it as #t's answer).
define parg
  # Casts parse in the current frame's language: inside rt.rs frames
  # (DWARF Rust) `(char*)` dies with "unexpected token". Run the body
  # in C, restore the frame's language at the end. See AGENTS.md.
  python gdb.execute("set language c", to_string=True)
  if $argc == 1
    set $t = (char*)$arg0
  else
    if $rdi != 0
      set $t = (char*)$rdi
    else
      set $t = (char*)$rbx
    end
  end
  if (unsigned long)$t == 0
    if $glm_face_human == 1
      printf "\e[1;31m=== GlmTable is NULL ===\e[0m\n"
    else
      printf "{tbl:null}\n"
    end
  else
    if $glm_face_human == 1
      printf "\n\e[1;36m=== GlmTable at %p ===\e[0m\n", $t
      printf "  data    :  %p\n", *(void**)$t
      printf "  len     :  %ld\n", *(long*)($t+8)
      printf "  reserve :  %lu\n", *(unsigned long*)($t+16)
      printf "  esize   :  %lu", *(unsigned long*)($t+24)
      if *(unsigned long*)($t+24) == 16
        printf "  (tagged Any cells)\n"
      else
        printf "  (typed)\n"
      end
      printf "  mode    :  %u", *(unsigned char*)($t+32)
      if *(unsigned char*)($t+32) == 0
        printf "  (dense)\n"
      else
        printf "  (sparse)\n"
      end
      printf "  ct      :  %u\n", *(unsigned char*)($t+33)
      printf "  sparse  :  %p\n", *(void**)($t+40)
      printf "  border  :  %ld  (#t's answer — one past the highest store)\n", *(long*)($t+48)
      printf "\e[1;36m========================\e[0m\n\n"
    else
      printf "{tbl:%p data:%p len:%ld reserve:%lu esize:%lu mode:", $t, *(void**)$t, *(long*)($t+8), *(unsigned long*)($t+16), *(unsigned long*)($t+24)
      if *(unsigned char*)($t+32) == 0
        printf "dense"
      else
        printf "sparse"
      end
      printf " ct:%u sparse:%p border:%ld}\n", *(unsigned char*)($t+33), *(void**)($t+40), *(long*)($t+48)
    end
  end
  python gdb.execute("set language auto", to_string=True)
end
document parg
  GlmTable header (incl. border — #t's answer), one line in the agent
  face, annotated in the human one. Usage: parg [addr|lua-var]
end

# --- pcells: first N cells decoded ----------------------------------------
# Reads the dense buffer directly; a sparse table (or a dense buffer that
# was never allocated) falls back to calling the .so's own glm_tbl_get,
# which handles the far-key overflow map. Usage: pcells <tbl> [count]
# (count defaults to min(len, 4); an explicit count overrides len because
# a born-sparse table has len 0 and its live cells live in the map).
define pcells
  # Language sandwich (see parg) plus the $bufp init: a convenience
  # var compared before its first assignment is void, and the
  # `$bufp == 0` test died with "Invalid type combination in equality
  # test" — on the dense face the infcall branch never runs, so only
  # sparse tables ever saw it. The reset costs one 16-byte inferior
  # malloc per sparse invocation.
  python gdb.execute("set language c", to_string=True)
  set $bufp = 0
  if $argc == 0
    if $glm_face_human == 1
      help pcells
    else
      echo usage: pcells <GlmTable*|lua-var> [count]\n
    end
  else
    set $t = (char*)$arg0
    if (unsigned long)$t == 0
      if $glm_face_human == 1
        printf "\e[1;31m=== GlmTable is NULL ===\e[0m\n"
      else
        printf "{tbl:null}\n"
      end
    else
      set $n = *(long*)($t+8)
      if $n > 4
        set $n = 4
      end
      if $argc == 2
        set $n = (long)$arg1
      end
      set $esz = *(unsigned long*)($t+24)
      set $md = *(unsigned char*)($t+32)
      set $ct = *(unsigned char*)($t+33)
      set $data = *(char**)$t
      if $glm_face_human == 1
        printf "\n\e[1;36m=== Dumping %ld Cell(s) from GlmTable %p ===\e[0m\n", $n, $t
      else
        printf "cells{tbl:%p esize:%lu mode:", $t, $esz
        if $md == 0
          printf "dense"
        else
          printf "sparse"
        end
        printf " n:%ld} [", $n
      end
      set $i = 0
      while $i < $n
        if $glm_face_human == 1
          printf "  [%ld] ", $i
        else
          if $i > 0
            printf " "
          end
          printf "%ld:", $i
        end
        if $md == 0 && (unsigned long)$data != 0
          if $esz == 16
            set $cp = $data + $i*16
          else
            if $esz == 8
              set $cp = $data + $i*8
            else
              set $cp = $data + $i
            end
          end
        else
          # Sparse (or never-allocated dense) fallback: call the module's
          # own glm_tbl_get, which handles the far-key overflow map.
          if $bufp == 0
            set $bufp = (char*)((void*(*)(unsigned long))malloc)(16)
          end
          call ((void(*)(void*,long,void*,unsigned long))glm_tbl_get)((void*)$t, $i, (void*)$bufp, $esz)
          set $cp = $bufp
        end
        _cell $cp $esz $ct
        if $glm_face_human == 1
          printf "\n"
        end
        set $i = $i + 1
      end
      if $glm_face_human == 1
        printf "\e[1;36m==========================================\e[0m\n\n"
      else
        printf "]\n"
      end
    end
  end
  python gdb.execute("set language auto", to_string=True)
end
document pcells
  Decoded cell dump from a GlmTable (dense buffers and sparse overflow
  maps). Usage: pcells <GlmTable*|lua-var> [count]
end

# --- pstack / stkwatch: the live stack window -----------------------------
# pstack [count] [addr] reads `count` words (default 8, max 64) UPWARD from
# $rsp (or addr) — the direction that matters when watching call/push/pop:
# the return address of a call and the saved rbp of a push land above the
# moving $rsp. Anchoring at the frame base shows the classic pair:
#   pstack 4 $rbp     →  [saved rbp][return address][caller stack...]
# Annotations (sparse, high-signal): code pointers decode to symbol+offset
# (via info symbol), values equal to $rbp mark the saved-frame-pointer
# chain (^rbp), and the word AT $rbp is flagged (#rbp) when in window.
# Pure Python end to end — read_memory and parse_and_eval parse no C, so
# there is no language sandwich to maintain; a read crossing into unmapped
# memory falls back word-by-word and keeps the mapped prefix (igrep's
# bounded-loss rule).
# stkwatch on|off opts into printing the window at EVERY stop (hook-stop):
# the agent face's silent-stop contract is broken deliberately then, one
# dense line per stop — `stkwatch on` + `si` through a prologue is the
# real-time view.
python
class PStack(gdb.Command):
    """pstack [count] [addr] — stack window: count words (default 8) read
    upward from $rsp (or addr), annotated for prologue watching."""

    def __init__(self):
        super().__init__("pstack", gdb.COMMAND_STACK)

    def invoke(self, args, from_tty):
        argv = args.split()
        count, base_expr = 8, None
        for a in argv:
            if a.lstrip("+-").isdigit():
                count = int(a)
            else:
                base_expr = a
        if count < 1:
            print("usage: pstack [count] [addr] (count >= 1, max 64)")
            return
        count = min(count, 64)
        try:
            sp = int(gdb.parse_and_eval(base_expr or "$rsp")) & 0xFFFFFFFFFFFFFFFF
        except Exception as e:
            print("pstack: bad address %r: %s" % (base_expr, e))
            return
        try:
            rbp = int(gdb.parse_and_eval("$rbp")) & 0xFFFFFFFFFFFFFFFF
        except Exception:
            rbp = 0
        try:
            buf = bytes(gdb.selected_inferior().read_memory(sp, count * 8))
        except Exception:
            # Bounded loss: read word-by-word and keep the mapped prefix.
            buf = b""
            for i in range(count):
                try:
                    buf += bytes(gdb.selected_inferior().read_memory(sp + i * 8, 8))
                except Exception:
                    break
            if not buf:
                print("pstack: cannot read stack at 0x%x" % sp)
                return
        n = len(buf) // 8
        gap = rbp - sp
        anchored = base_expr is not None and base_expr != "$rsp"
        base_label = "base" if anchored else "rsp"

        def sym_of(v):
            # Decode a plausible userspace pointer to symbol+offset;
            # "No symbol matches ..." means data (or stripped), not code.
            try:
                out = gdb.execute("info symbol 0x%x" % v, to_string=True).strip()
                if out and not out.startswith("No symbol"):
                    return out.split(" in section")[0].replace(" + ", "+").strip()
            except Exception:
                pass
            return None

        rows = []
        for i in range(n):
            addr = sp + i * 8
            v = int.from_bytes(buf[i * 8:(i + 1) * 8], "little")
            kind = None
            sym = None
            if addr == rbp:
                kind = "rbp_row"
            if v == rbp and rbp != 0:
                kind = kind or "chain"
            if v != 0 and v < 0x0000800000000000:
                sym = sym_of(v)
                if sym and kind is None:
                    kind = "code"
            rows.append((addr, v, kind, sym))

        if GLM_HUMAN:
            print("\033[1;36m=== Stack window: %d word(s) up from %s 0x%016x "
                  "(rbp 0x%016x, gap %+dB) ===\033[0m" % (n, base_label, sp, rbp, gap))
            for addr, v, kind, sym in rows:
                off = addr - sp
                vstr = "0x%016x" % v
                if kind == "code":
                    vstr = "\033[1;32m%s\033[0m" % vstr
                elif kind == "chain":
                    vstr = "\033[1;35m%s\033[0m" % vstr
                line = "  +0x%03x  0x%016x  %s" % (off, addr, vstr)
                if kind == "rbp_row":
                    line += "  \033[1;36m← $rbp\033[0m"
                if v == rbp and rbp != 0:
                    line += "  \033[1;35m← saved rbp (chain)\033[0m"
                if sym:
                    line += "  \033[1;32m← %s (code)\033[0m" % sym
                print(line)
            print("\033[1;36m%s\033[0m" % ("=" * 66))
        else:
            parts = []
            for addr, v, kind, sym in rows:
                p = "+%d=0x%x" % (addr - sp, v)
                if addr == rbp:
                    p += "#rbp"
                if v == rbp and rbp != 0:
                    p += "^rbp"
                if sym:
                    p += "(%s)" % sym
                parts.append(p)
            print("stk{%s:0x%x rbp:0x%x gap:%+dB n:%d} [%s]"
                  % (base_label, sp, rbp, gap, n, " ".join(parts)))


PStack()


class StkWatch(gdb.Command):
    """stkwatch [on|off] — print a pstack window on every stop (status
    when invoked with no argument)."""

    def __init__(self):
        super().__init__("stkwatch", gdb.COMMAND_NONE)

    def invoke(self, args, from_tty):
        a = args.strip().lower()
        if a in ("", "status"):
            val = int(gdb.parse_and_eval("$glm_stk_watch"))
        elif a in ("on", "1"):
            val = 1
        elif a in ("off", "0"):
            val = 0
        else:
            print("usage: stkwatch [on|off]")
            return
        gdb.execute("set $glm_stk_watch = %d" % val, to_string=True)
        if GLM_HUMAN:
            if val:
                print("\033[1;32mstack-watch: ON — pstack prints on every stop\033[0m")
            else:
                print("\033[1;31mstack-watch: OFF\033[0m")
        else:
            print("stack-watch:%d (%s)" % (val, "pstack on every stop" if val else "off"))


StkWatch()
end

# --- xq: one tagged 128-bit Any cell, one line ----------------------------
define xq
  # Language sandwich (see parg): `(unsigned long)` in _cell's casts
  # is unparseable in Rust frames.
  python gdb.execute("set language c", to_string=True)
  if $argc == 0
    if $glm_face_human == 1
      help xq
    else
      echo usage: xq <addr of 16-byte cell>\n
    end
  else
    if $glm_face_human == 1
      set $ptr = (unsigned long long*)$arg0
      printf "\n\e[1;35m=== 128-bit Cell at %p ===\e[0m\n", $ptr
      printf "  [High 64-bit Tag]:      0x%016llx\n", $ptr[1]
      printf "  [Low 64-bit Payload]:   0x%016llx\n", $ptr[0]
      printf "  -> Decoded: "
      _cell (char*)$arg0 16 0
      printf "\n\e[1;35m==========================================\e[0m\n\n"
    else
      _cell (char*)$arg0 16 0
      echo \n
    end
  end
  python gdb.execute("set language auto", to_string=True)
end
document xq
  Decode one 16-byte tagged cell (payload+GLM_ARG tag). Usage: xq <addr>
end

# --- xqv: one tagged 128-bit Any cell from a VALUE, one line --------------
# The register seam's decoder: at a glm_tbl_set_any stop the cell IS
# the i128 third argument (a register pair), not bytes at an address,
# so xq's dereference cannot read it. Truncating casts pick the
# halves ((unsigned long)val = payload, (unsigned long)(val >> 64) =
# tag — the explicit __int128 cast spelling dies in gdb's C parser);
# a float payload round-trips through an 8-byte inferior malloc —
# gdb cannot bit-cast a convenience value in place (the same trick
# pcells' fallback uses).
define xqv
  # Language sandwich (see parg): the casts below die in Rust frames.
  python gdb.execute("set language c", to_string=True)
  if $argc == 0
    if $glm_face_human == 1
      help xqv
    else
      echo usage: xqv <i128 value>\n
    end
  else
    set $pl = (unsigned long)$arg0
    set $tg = (unsigned long)($arg0 >> 64)
    if $glm_face_human == 1
      printf "\n\e[1;35m=== 128-bit Cell (By Value) ===\e[0m\n"
      printf "  [High 64-bit Tag]:      0x%016llx\n", $tg
      printf "  [Low 64-bit Payload]:   0x%016llx\n", $pl
      printf "  -> Decoded: "
    end
    if $tg == 0
      if $glm_face_human == 1
        printf "\e[1;32m[INT]\e[0m    i:%ld u:0x%lx", (long)$pl, $pl
      else
        printf "int i:%ld u:0x%lx", (long)$pl, $pl
      end
    else
      if $tg == 1
        set $m = (char*)((void*(*)(unsigned long))malloc)(8)
        *(unsigned long*)$m = $pl
        if $glm_face_human == 1
          printf "\e[1;34m[FLOAT]\e[0m  f:%g u:0x%lx", *(double*)$m, $pl
        else
          printf "float u:0x%lx f:%g", $pl, *(double*)$m
        end
      else
        if $tg == 2
          if $glm_face_human == 1
            printf "\e[1;33m[BOOL]\e[0m   b:%d u:0x%lx", (int)($pl & 0xff), $pl
          else
            printf "bool u:0x%lx b:%d", $pl, (int)($pl & 0xff)
          end
        else
          if $tg == 3
            if $glm_face_human == 1
              printf "\e[1;35m[STR]\e[0m    \"%s\" u:0x%lx", (char*)$pl, $pl
            else
              printf "str u:0x%lx s:\"%s\"", $pl, (char*)$pl
            end
          else
            if $glm_face_human == 1
              printf "\e[1;31m[TAG:%ld]\e[0m 0x%lx", $tg, $pl
            else
              printf "tag%ld u:0x%lx", $tg, $pl
            end
          end
        end
      end
    end
    if $glm_face_human == 1
      printf "\n\e[1;35m===============================\e[0m\n\n"
    else
      echo \n
    end
  end
  python gdb.execute("set language auto", to_string=True)
end
document xqv
  Decode one tagged 128-bit cell passed BY VALUE (glm_tbl_set_any /
  glm_tbl_get_any stops). Usage: xqv <i128 expr, e.g. val>
end

# --- pkind: the module's exported boundary contract -----------------------
define pkind
  # Language sandwich (see parg): the function-pointer cast dies in
  # Rust frames.
  python gdb.execute("set language c", to_string=True)
  set $k = ((int(*)(void))glm_arg_kind)()
  if $glm_face_human == 1
    printf "\e[1;32mglm_arg_kind=%d\e[0m (-1 none, 0 int, 1 float, 2 bool, 3 string, 4 any)\n", $k
  else
    printf "glm_arg_kind=%d (-1 none, 0 int, 1 float, 2 bool, 3 string, 4 any)\n", $k
  end
  python gdb.execute("set language auto", to_string=True)
end
document pkind
  Query glm_arg_kind() from the loaded module. Requires the .so/exe symbols.
end

# --- here / cbt (python ergonomic commands) -------------------------------
python
import gdb, os, re


class Here(gdb.Command):
    """here — current position as one line: basename:line: source text."""

    def __init__(self):
        super().__init__("here", gdb.COMMAND_STACK)

    def invoke(self, args, from_tty):
        try:
            f = gdb.selected_frame()
            sal = f.find_sal()
            if not sal or not sal.symtab or sal.line == 0:
                print("here: no source at %s" % (f.pc(),))
                return
            base = os.path.basename(sal.symtab.filename)
            text = ""
            try:
                path = sal.symtab.fullname() or sal.symtab.filename
                with open(path, "r", errors="replace") as fh:
                    lines = fh.read().splitlines()
                if sal.line - 1 < len(lines):
                    text = lines[sal.line - 1].strip()
            except Exception:
                pass
            if GLM_HUMAN:
                print("\033[1;32m%s:%d\033[0m: %s" % (base, sal.line, text))
            else:
                print("%s:%d: %s" % (base, sal.line, text))
        except Exception as e:
            print("here: %s" % e)


Here()


_SKIP = re.compile(r"std::|core::|alloc::|panic_unwind|__libc|_dl_|__vdso|rust_begin_unwind")


class Cbt(gdb.Command):
    """cbt [N] — compact backtrace; std/core/alloc/loader frames elided.
    Agent face: frame numbers stay gdb-native (usable with `frame N`).
    Human face: numbers are renumbered over the PRINTED frames, and the
    walk goes up to 64 deep so elided std frames don't consume the
    budget (use gdb-native `bt` for the native numbering)."""

    def __init__(self):
        super().__init__("cbt", gdb.COMMAND_STACK)

    def invoke(self, args, from_tty):
        argv = args.split()
        limit = int(argv[0]) if argv and argv[0].isdigit() else 12
        f = gdb.newest_frame()
        i = printed = depth = 0
        while f is not None and (not GLM_HUMAN or depth < 64):
            if (GLM_HUMAN and printed >= limit) or (not GLM_HUMAN and i >= limit):
                break
            name = f.name() or "??"
            if not _SKIP.search(name):
                loc = ""
                try:
                    sal = f.find_sal()
                    if sal and sal.symtab and sal.line:
                        base = os.path.basename(sal.symtab.filename)
                        if GLM_HUMAN:
                            loc = " \033[36m%s:%d\033[0m" % (base, sal.line)
                        else:
                            loc = " %s:%d" % (base, sal.line)
                except Exception:
                    pass
                if GLM_HUMAN:
                    print("#%d %s%s" % (printed, name, loc))
                    printed += 1
                else:
                    print("#%d %s%s" % (i, name, loc))
            f = f.older()
            i += 1
            depth += 1


Cbt()
end

# --- igrep: filtered lookahead disassembly ---------------------------------
# Unlike interactive `x/Ni`, a to_string walk that crosses into unmapped
# memory raises and loses the partial output, so the walk goes in
# 64-instruction chunks with one lookahead instruction (which names the
# true next start — x/Ni prints no end address). A failed chunk falls
# back to the mapped prefix. The agent face stays plain. The human face
# re-applies gdb's OWN disassembler styling to each printed line: gdb's
# escapes never survive a to_string capture (the capture stream is not a
# tty), so the interactive x/1i coloring — address/branch-target blue,
# symbol names yellow (+off plain), mnemonic green, %registers red,
# $immediates and displacements blue, the whole `# comment` tail dim —
# is rebuilt from `show style` (honoring the user's own restyling),
# with the regex matches popping bold-green on top of it and a cyan
# summary line. Ground truth for every one of those spans was captured
# from a styled pty session; if the classification here ever disagrees
# with a live `x/1i`, trust the capture and fix the token table.
python
import re as _re


def _glm_style_codes():
    """Parse `show style` once → {style name: escape}; {} when styling is
    globally disabled or unparsable (callers print plain lines then)."""
    try:
        raw = gdb.execute("show style", to_string=True)
    except Exception:
        return {}
    for ln in raw.splitlines():
        if ln.startswith("style enabled:") and "disabled" in ln:
            return {}
    fg = {"black": "30", "red": "31", "green": "32", "yellow": "33",
          "blue": "34", "magenta": "35", "cyan": "36", "white": "37"}
    bg = {"black": "40", "red": "41", "green": "42", "yellow": "43",
          "blue": "44", "magenta": "45", "cyan": "46", "white": "47"}
    for i, c in enumerate("black red green yellow blue magenta cyan white".split()):
        fg["light-" + c] = str(90 + i)
        bg["light-" + c] = str(100 + i)
    fields = {}
    for ln in raw.splitlines():
        m = _re.match(r'style (.+) (background|foreground|intensity): '
                      r'.* is: (.+?)\s*$', ln)
        if not m:
            continue
        name, field, val = m.groups()
        d = fields.setdefault(name, {})
        if field == "foreground":
            d["fg"] = fg.get(val, "38;5;" + val if val.isdigit() else "")
        elif field == "background":
            d["bg"] = bg.get(val, "48;5;" + val if val.isdigit() else "")
        else:
            d["int"] = {"bold": "1", "dim": "2"}.get(val, "")
    out = {}
    for name, d in fields.items():
        params = [p for p in (d.get("int", ""), d.get("fg", ""), d.get("bg", "")) if p]
        if params:
            out[name] = "\033[" + ";".join(params) + "m"
    return out


# Operand token table, classification-first (comment before target before
# the rest, so `# ... <sym>` stays comment-dim and `0x.. <sym>` splits).
_GLM_TOK = _re.compile(
    r'(?P<comment>\s+#.*$)'
    r'|(?P<target>0x[0-9a-fA-F]+\s+<.*>)'
    r'|(?P<reg>%[a-z0-9]+)'
    r'|(?P<imm>\$-?(?:0x[0-9a-fA-F]+|[0-9]+))'
    r'|(?P<disp>-?0x[0-9a-fA-F]+)'
    r'|(?P<num>[0-9]+)')


def _glm_style_disasm(line, spans, S):
    """One x/i line wearing gdb's own styles; `spans` are absolute plain-text
    match ranges overlaid bold-green. Unparsable lines return plain."""
    R = "\033[0m"
    pieces = []   # (start, end, style-name-or-None) over the whole line
    at = [0]

    def put(text, key=None):
        if text:
            pieces.append((at[0], at[0] + len(text), key))
            at[0] += len(text)

    def put_sym(txt):
        # <name+off> — the NAME wears the function style, `+off` stays plain
        sm = _re.match(r'(.*)\+(\d+)$', txt)
        if sm:
            put(sm.group(1), "function")
            put("+" + sm.group(2))
        else:
            put(txt, "function")

    head, tab, rest = line.partition(":\t")
    hm = _re.match(r'^(=> ?|   )?(0x[0-9a-fA-F]+)(?: <(.*)>)?$', head) if tab else None
    if hm is None:
        return line
    put(hm.group(1) or "")
    put(hm.group(2), "address")
    if hm.group(3) is not None:
        put(" <")
        put_sym(hm.group(3))
        put(">")
    put(":\t")
    mn = _re.match(r'\S+', rest)
    if mn:
        put(mn.group(0), "disassembler mnemonic")
    pos = mn.end() if mn else 0
    while pos < len(rest):
        t = _GLM_TOK.search(rest, pos)
        if t is None:
            put(rest[pos:])
            break
        put(rest[pos:t.start()])
        kind = t.lastgroup
        if kind == "comment":
            put(t.group(0), "disassembler comment")
        elif kind == "target":
            tm = _re.match(r'(0x[0-9a-fA-F]+)(\s+)(<.*)$', t.group(0))
            put(tm.group(1), "address")
            put(tm.group(2))
            put("<")
            put_sym(tm.group(3)[1:-1])
            put(">")
        elif kind == "reg":
            put(t.group(0), "disassembler register")
        else:   # imm / disp / num
            put(t.group(0), "disassembler immediate")
        pos = t.end()
    if at[0] != len(line):
        return line   # the walk lost sync — plain beats half-styled text
    out = []
    for a, b, key in pieces:
        cuts = [a, b]
        for s, e in spans:
            if a < s < b:
                cuts.append(s)
            if a < e < b:
                cuts.append(e)
        for x, y in zip(*[iter(sorted(set(cuts)))] * 2):
            hit = any(s < y and e > x for s, e in spans)
            esc = "\033[1;32m" if hit else (S.get(key) if key else "")
            chunk = line[x:y]
            out.append((esc + chunk + R) if esc else chunk)
    return "".join(out)


class IGrep(gdb.Command):
    """igrep <count> <pattern> [addr] — disassemble count instructions from
    addr (default $pc), print only lines matching the case-insensitive regex."""

    def __init__(self):
        super().__init__("igrep", gdb.COMMAND_RUNNING)

    def invoke(self, args, from_tty):
        argv = args.split()
        if len(argv) < 2 or not argv[0].isdigit():
            if GLM_HUMAN:
                print("\033[33musage: igrep <count> <pattern> [addr]\033[0m")
            else:
                print("usage: igrep <count> <pattern> [addr]")
            return
        want = int(argv[0])
        pat = _re.compile(argv[1], _re.IGNORECASE)
        start = argv[2] if len(argv) > 2 else "$pc"
        addr = _re.compile(r"^\s*(?:=>\s*)?(0x[0-9a-fA-F]+)")
        styles = False   # parsed lazily on the first printed human line
        shown = seen = 0
        while seen < want:
            n = min(64, want - seen)
            try:
                out = gdb.execute("x/%di %s" % (n + 1, start), to_string=True)
                lines = out.splitlines()
                tail, lines = lines[-1], lines[:-1]
            except gdb.MemoryError:
                try:
                    out = gdb.execute("x/%di %s" % (n, start), to_string=True)
                    lines, tail = out.splitlines(), None
                except gdb.MemoryError:
                    lines, tail = [], None
            for line in lines:
                if pat.search(line):
                    if GLM_HUMAN:
                        if styles is False:
                            styles = _glm_style_codes()
                        print(_glm_style_disasm(
                            line,
                            [(m.start(), m.end()) for m in pat.finditer(line)],
                            styles))
                    else:
                        print(line)
                    shown += 1
            seen += len(lines)
            m = addr.match(tail) if tail else None
            if m is None or seen >= want:
                break
            start = m.group(1)
        if GLM_HUMAN:
            print("\033[36m--- %d match(es) in the next %d instructions ---\033[0m" % (shown, seen))
        else:
            print("igrep: %d match(es) in %d instructions" % (shown, seen))


IGrep()
end

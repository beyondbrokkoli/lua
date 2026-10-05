# /home/halim/lua/.gdbinit — Elite Debugger Config for GLM Compiler & Runtime

# 1. Core Behavior & Noise Reduction
set debuginfod enabled off
set breakpoint pending on
set pagination off
set confirm off
set history save on

# Symbol demangling & output styling
set print demangle on
set print asm-demangle on
set print pretty on
set print array on
set print array-indexes on

# Disable symbol printing in disassembly to hide massive Rust v0 symbol blobs.
# This keeps the terminal clean when inspecting assembly instructions.
set print symbol off
set disassemble-next-line off

# Map rustc sysroot to local Rust standard library sources
set substitute-path /rustc/8bab26f4f68e0e26f0bb7960be334d5b520ea452 /usr/lib/rustlib/src/rust

# Prevent 'step' and 'next' from falling into dynamic linker (ld-linux) or glibc thunks.
skip -gfi /lib/**/*.so
skip -gfi /lib64/**/*.so
skip -gfi /usr/lib/**/*.so
skip -gfi /usr/lib64/**/*.so

# Prevent stepping into Rust standard library internals.
# Uses Regular Expression Function (-rfu) matching against demangled names.
# The .* prefix catches cases like `<T as core::...>` trait implementations.
skip -rfu .*std::.*
skip -rfu .*core::.*
skip -rfu .*alloc::.*
skip -rfu .*panic_unwind::.*

# Fallback file globs (using single wildcards to avoid fnmatch ** bugs)
skip -gfi /rustc/*
skip -gfi /usr/lib/rustlib/*

# 2. Ergonomic Stepping Display (Replaces 'display/i $pc')
# By using `hook-stop` instead of `display`, we fix the "Enter key repetition" quirk.
# Hitting Enter will now repeat `si` or `ni` normally, and hook-stop simply triggers
# to show a clean dashboard, rather than running `x/i` which auto-increments memory.

define hook-stop
  # Print a dark grey border

  # Group 1: Return value and Stack/Frame pointers
  printf "\e[35m[Ret/Stack]\e[0m  \e[1;36mRAX:\e[0m 0x%016lx  \e[1;36mRSP:\e[0m 0x%016lx  \e[1;36mRBP:\e[0m 0x%016lx\n", $rax, $rsp, $rbp

  # Group 2: Arguments 1-3
  printf "\e[35m[Args 1-3] \e[0m  \e[1;36mRDI:\e[0m 0x%016lx  \e[1;36mRSI:\e[0m 0x%016lx  \e[1;36mRDX:\e[0m 0x%016lx\n", $rdi, $rsi, $rdx

  # Group 3: Arguments 4-6
  printf "\e[35m[Args 4-6] \e[0m  \e[1;36mRCX:\e[0m 0x%016lx  \e[1;36mR8 :\e[0m 0x%016lx  \e[1;36mR9 :\e[0m 0x%016lx\n", $rcx, $r8, $r9

  # Group 4: Scratch registers & Instruction Pointer (Highlighted Green)
  printf "\e[35m[Scratch]  \e[0m  \e[1;36mR10:\e[0m 0x%016lx  \e[1;36mR11:\e[0m 0x%016lx  \e[1;32mRIP:\e[0m 0x%016lx\n", $r10, $r11, $rip

  # Group 5: Callee-saved registers (Rust/C must restore these if they use them)
  printf "\e[35m[Saved]    \e[0m  \e[1;36mRBX:\e[0m 0x%016lx  \e[1;36mR12:\e[0m 0x%016lx  \e[1;36mR13:\e[0m 0x%016lx\n", $rbx, $r12, $r13
  printf "\e[35m[Saved]    \e[0m  \e[1;36mR14:\e[0m 0x%016lx  \e[1;36mR15:\e[0m 0x%016lx  \e[1;33mEFL:\e[0m 0x%016lx\n", $r14, $r15, $eflags

  # Print exactly the next instruction natively
  x/1i $pc
  echo \n
end

# 3. General-Purpose Inspection Helpers

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

define xq
  if $argc == 0
    help xq
  else
    set $ptr = (unsigned long long*)$arg0
    printf "\n=== 128-bit Cell at %p ===\n", $ptr
    printf "  [High 64-bit Tag]:     0x%016llx\n", $ptr[1]
    printf "  [Low 64-bit Payload]:  0x%016llx\n", $ptr[0]
    printf "==========================================\n\n"
  end
end
document xq
  Dumps a 128-bit word (16 bytes) at the given address, splitting it into High (Tag) and Low (Payload).
  Excellent for inspecting GLM_ARG_ANY dynamically typed boundary cells.
  Usage: xq <address>
end

define regs
  info registers rax rbx rcx rdx rsi rdi rbp rsp r8 r9 r10 r11 r12 r13 r14 r15 rip eflags
end
document regs
  Displays a comprehensive overview of all 64-bit general-purpose registers and CPU flags.
end

define to_script
  b glm_exec
  r
end
document to_script
  Sets a pending breakpoint on @glm_exec and runs the host compiler straight into the Lua JIT entry point.
end

# 4. GLM Boundary Data Structure Inspectors

define parg
  if $argc == 1
    set $tbl = (unsigned long*)$arg0
  else
    # Fallback heuristics if no address is explicitly provided
    if $rdi != 0
      set $tbl = (unsigned long*)$rdi
    else
      set $tbl = (unsigned long*)$rbx
    end
  end

  printf "\n=== GlmTable at %p ===\n", $tbl
  printf "  data  (*u8):  0x%016lx\n", $tbl[0]
  printf "  len   (i64):  %ld\n",        (long)$tbl[1]
  printf "  cap   (i64):  %ld\n",        (long)$tbl[2]
  printf "  esize (i64):  %ld bytes",    (long)$tbl[3]
  if $tbl[3] == 16
    printf " (GLM_ARG_ANY)\n"
  else
    printf " (Typed)\n"
  end
  printf "  flags (i64):  0x%016lx\n", $tbl[4]
  printf "  aux   (i64):  0x%016lx\n", $tbl[5]
  printf "========================\n\n"
end
document parg
  Prints formatted 48-byte GlmTable struct fields.
  Usage: parg [address] (Defaults to checking $rdi or$rbx if omitted).
end

define pcells
  if $argc == 0
    help pcells
  else
    # $arg0 is address,$arg1 is count (optional, default 4)
    set $data = (unsigned long*)$arg0
    if $argc == 2
      set $count =$arg1
    else
      set $count = 4
    end

    printf "\n=== Dumping %d Cell(s) from Buffer: %p ===\n", $count,$data
    set $i = 0
    while $i <$count
      set $cell_ptr = (unsigned long*)((char*)$data + ($i * 16))
      set $payload =$cell_ptr[0]
      set $tag = (long)$cell_ptr[1]

      printf "  [%d] Payload: 0x%016lx | Tag: %ld", $i, $payload,$tag

      if $tag == 0
        printf " [INT: %ld]\n", (long)$payload
      else
        if $tag == 1
          # Note: Formatting float natively from hex requires python extension, keeping raw for pure GDB
          printf " [FLOAT]\n"
        else
          if $tag == 2
            printf " [BOOL]\n"
          else
            if $tag == 3
              printf " [STRING ptr]\n"
            else
              printf " [OTHER]\n"
            end
          end
        end
      end
      set $i =$i + 1
    end
    printf "====\n\n"
  end
end
document pcells
  Dumps raw tagged cells from a known data buffer pointer.
  Usage: pcells <data_ptr_address> [count] (count defaults to 4).
end

# 5. Default Breakpoints
b glm::main

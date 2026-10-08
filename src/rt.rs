#![allow(unsafe_op_in_unsafe_fn)]
// The glm runtime: C-ABI surface (print + table memory).
use std::alloc::{Layout, alloc_zeroed, dealloc, realloc};
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::trace::{
    TRACE_FAIL_ANY_LEN, TRACE_FAIL_DIV_ZERO, TRACE_FAIL_LEAK_DETECTED, TRACE_FAIL_NULL_ROW_STORE,
    TRACE_RT_ALLOC, TRACE_RT_FREE, TRACE_RT_FREE_CHILD, TRACE_RT_STR_INTERN, TRACE_RT_STR_POOL_HIT,
};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableMode {
    Dense = 0,
    Sparse = 1,
}

/// A table value is one machine word — a pointer to this stable
/// #[repr(C)] header. Field layout (offset: type), ABI-pinned by
/// test_abi_layout_offsets:
///   data@0:         *mut u8   — the element buffer (moves freely; the header stays)
///   len@8:          i64       — the zeroed, addressable span (doubling watermark)
///   reserve@16:     usize     — 0 = malloc world; nonzero = PROT_NONE VA reservation
///   esize@24:       usize     — element size: 8 for int/float/str/table,
///                              1 for bool, 16 for the tagged Any cell
///   mode@32:        TableMode — 0 = Dense (flat buffer + overflow map),
///                              1 = Sparse (map-only, born at TableNew)
///   contains_tbl@33: u8       — 1 = elements include nested tables (deep-free flag)
///   sparse_map@40:  *mut HashMap — the far-key overflow map: born-Sparse
///                              tables fill it with every store, Dense tables
///                              allocate it lazily on the first far store
///   border@48:      i64       — `#t`: one past the highest index ever
///                              stored (any lane; far keys count)
/// Total: 56 bytes (7 machine words). Every pre-border offset is
/// unchanged — the field was appended at the runtime-border milestone,
/// and every producer and consumer of headers lives in this crate
/// (both hosts build through glm_tbl_new).
///
/// THE ALIAS INVARIANT: only the data buffer moves; the header sits
/// at one address for the table's whole lifetime. Every live copy of
/// a table value points at the header, so growth may relocate the
/// buffer freely and table values stay plain SSA ptrs
/// (buffer_growth_keeps_aliased_table_values_live pins the copy).
/// This is the entire alias story.
///
/// THE DENSITY INVARIANT: a Dense table is Dense forever — the data
/// pointer stays a live zeroed span for the table's whole lifetime.
/// Far keys (past `len + SPARSE_THRESHOLD`) ride the overflow map,
/// reads overlay the map over the span, and a dense write shadows a
/// mapped entry away, so the fill-loop fast store's raw in-bounds GEP
/// is sound against every store the checked path performs, whatever
/// the key computes to at runtime.
#[repr(C)]
pub struct GlmTable {
    pub data: *mut u8,
    pub len: i64,
    pub reserve: usize,
    pub esize: usize,
    pub mode: TableMode,
    pub contains_tables: u8,
    /// The far-key overflow map: born-Sparse tables fill it with every
    /// store, Dense tables allocate it lazily on the first far store.
    /// The value lane is u128 so an Any (tagged) cell's whole 16
    /// bytes ride it; 8- and 1-byte cells sit in the low bits.
    pub sparse_map: *mut HashMap<i64, u128>,
    /// The border — `#t`'s answer: one past the highest index ever
    /// stored, any lane (far keys count). NOT the span watermark
    /// (`len`, a doubling capacity): the border is the store extent,
    /// a plain high-water mark maintained by set_core on every store
    /// and committed by glm_tbl_reserve (the compiler reserves exactly
    /// on proven full-fill loops, which is what covers the fast-store
    /// path that bypasses set_core). Zero-fill reads and absent rows
    /// never move it; `glm_tbl_len` reads it.
    pub border: i64,
}

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

// === GLM_TRACE signal array ===
// 256 one-byte slots mapped MAP_SHARED off ./.glm_rt_trace.bin — a
// runtime-owned sidecar, fully separate from the compiler's
// .glm_trace.bin chronology plate (the compiler process has already
// exited by the time this runs, so it cannot append to that ring).
// A signal is a bare store, so tracing survives crashes with no flush
// step and costs no syscall in steady state (the gauntlet's 1M
// ctor/free churn stays clean). First caller maps; later callers reuse
// the pointer.

const TRACE_SLOTS: usize = 256;
const TRACE_FILE: &str = ".glm_rt_trace.bin";

static TRACE_MAP: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());
static LEAK_HOOK: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub unsafe fn init_trace_map() {
    // Anti-sticky: the sidecar is per-run — the previous run's file is
    // removed before this run maps, so every execution starts from an
    // all-zero array (a leak in one run leaves no flag in the next).
    let _ = std::fs::remove_file(TRACE_FILE);
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(TRACE_FILE)
    else {
        return; // unwritable CWD: degrade silently, tracing not fatal
    };
    let _ = file.set_len(TRACE_SLOTS as u64);
    let ptr = unsafe {
        mmap(
            ptr::null_mut(),
            TRACE_SLOTS,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if is_map_failed(ptr) {
        return;
    }
    TRACE_MAP.store(ptr.cast::<u8>(), Ordering::Relaxed);
}

#[inline]
fn trace_map() -> *mut u8 {
    let map = TRACE_MAP.load(Ordering::Relaxed);
    if !map.is_null() {
        return map;
    }
    unsafe { init_trace_map() };
    TRACE_MAP.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_trace_set(slot: u8) {
    let map = trace_map();
    if !map.is_null() {
        unsafe { *map.add(slot as usize) = 1 };
    }
}

// Registered once by the first glm_tbl_new: at process exit, any live
// GlmTable is a leak. Runs even though the crate builds with
// panic=abort (atexit fires on the normal exit path).
unsafe extern "C" fn glm_trace_leak_check() {
    if ALLOC_COUNT.load(Ordering::Relaxed) > 0 {
        unsafe { glm_trace_set(TRACE_FAIL_LEAK_DETECTED) };
        eprintln!("GLM_TRACE: slot 90 — leak at exit (sys_alloc_count() > 0)");
    }
}

fn elem_layout(len: i64, esize: usize) -> Layout {
    let bytes = usize::try_from(len)
        .ok()
        .and_then(|l| l.checked_mul(esize))
        .unwrap_or_else(|| abort_alloc(usize::MAX));
    Layout::from_size_align(bytes, span_align(esize)).unwrap_or_else(|_| abort_alloc(bytes))
}

/// The dense span's allocation alignment for one esize: the cell
/// repr's ALIGN — 16 for the tagged Any word, 8 for every other.
/// This is the engine half of the tiered alignment law: the span is
/// memory the runtime itself allocates (through this mapping, at
/// every alloc/realloc/dealloc site), so its alignment is a fact the
/// runtime states and every span movement relies on by construction.
/// The FFI seam's 8-byte minimum guarantee is untouched — no caller
/// ever supplies a cell buffer.
const fn span_align(esize: usize) -> usize {
    if esize == u128::ESIZE {
        u128::ALIGN
    } else {
        u64::ALIGN
    }
}

fn abort_alloc(bytes: usize) -> ! {
    eprintln!("glm: table allocation of {bytes} bytes failed");
    std::process::abort();
}

/// # Safety
/// `esize` is 1, 8, or 16 (the checker-pinned element sizes). `flags` is a
/// packed byte: bit 0 = mode (0=Dense, 1=Sparse), bit 7 =
/// contains_tables.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_new(esize: usize, flags: u8) -> *mut GlmTable {
    // Unpack bit 7 → contains_tables for the deep-free path
    let contains_tables = (flags & 0x80) >> 7;
    // Unpack bit 0 → sparse mode (bits 1-6 unused)
    let is_sparse = (flags & 0x01) != 0;

    let hdr = Box::into_raw(Box::new(GlmTable {
        data: std::ptr::null_mut(),
        len: 0,
        reserve: 0,
        esize,
        mode: if is_sparse {
            TableMode::Sparse
        } else {
            TableMode::Dense
        },
        contains_tables,
        sparse_map: if is_sparse {
            Box::into_raw(Box::new(HashMap::new()))
        } else {
            std::ptr::null_mut()
        },
        border: 0,
    }));
    ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
    unsafe { glm_trace_set(TRACE_RT_ALLOC) };
    if LEAK_HOOK
        .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        unsafe { atexit(glm_trace_leak_check) };
    }
    hdr
}

/// # Safety
/// `t` live from glm_tbl_new; `idx` non-negative, non-overflowing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_grow(t: *mut GlmTable, idx: i64) {
    if !t.is_null() {
        let tbl = &mut *t;
        if tbl.mode != TableMode::Dense {
            return;
        }
    }
    if idx >= 0 && idx == i64::MAX {
        eprintln!("glm fatal: table index overflow (idx={idx})");
        std::process::abort();
    }
    unsafe { span_grow(t, idx.wrapping_add(1)) };
}

/// # Safety
/// `t` live from glm_tbl_new; on return the span covers `n` cells.
/// The call also commits the border: the compiler emits a reserve
/// exactly when it PROVED the coming fill covers 0..n (the
/// reserved-loop conversion), so the fast stores that bypass set_core
/// still leave a true border behind — n is the border that fill owes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_reserve(t: *mut GlmTable, n: i64) {
    let tbl = unsafe { &mut *t };
    if tbl.mode != TableMode::Dense {
        return;
    }
    if n > tbl.border {
        tbl.border = n;
    }
    unsafe { span_grow(t, n) };
}

/// `#t`: the array's border — one past the highest index ever stored,
/// any lane (a far key counts; a born-Sparse table answers its map's
/// high-water). A null table answers 0: `#` is a read, and reads are
/// total in this engine (an absent row has no cells, the same
/// philosophy as the zero-filling cell read).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_len(t: *const GlmTable) -> i64 {
    if t.is_null() {
        return 0;
    }
    unsafe { (*t).border }
}

// Phase 2 — the overflow map for far keys

/// The far-key overflow threshold, the one value both sides of the
/// compile-time/runtime twin read: a checked store past
/// `len + SPARSE_THRESHOLD` rides the table's overflow map instead of
/// the dense span (the span and its GEP-fast path stay untouched), and
/// the analyzer's compile-time Sparse verdict (src/shape) reuses this
/// constant, so the two contracts cannot drift.
pub const SPARSE_THRESHOLD: i64 = 100_000;

/// The overflow map of a live table — the far-key cells past the
/// sparse threshold. Born-Sparse tables allocate theirs in
/// glm_tbl_new; Dense tables allocate one here, lazily, on the first
/// far store. Reads overlay the map over the dense span (map wins),
/// and a dense write to a mapped index shadows the entry away.
///
/// # Safety
/// `t` live from glm_tbl_new, mode Dense or Sparse; the caller holds
/// no other borrow.
#[inline(never)]
unsafe fn overflow_map(t: *mut GlmTable) -> &'static mut HashMap<i64, u128> {
    let tbl = unsafe { &mut *t };
    if tbl.sparse_map.is_null() {
        tbl.sparse_map = Box::into_raw(Box::new(HashMap::with_capacity(1)));
    }
    unsafe { &mut *tbl.sparse_map }
}

// === The cell-repr shell ============================================
// GlmTable is the exported shell; beneath it, storage is a dense
// monomorphic CELL STORE whose element repr hangs in through one
// trait — the "GlmData" seam. Three reprs ride it today: the 1-byte
// bool, the 8-byte machine word (int/float/str-ptr/table-ptr), and
// the 16-byte tagged Any word — exactly the monomorphic tables and
// the Any tables the language has. A future construct (string-keyed
// tables, a fuller Lua table) does not fork this logic: it hangs in a
// new repr with ONE impl block here and, when it needs its own
// exported face, calls the same generic cores (`set_core`/`get_core`).
// The horizontal extension for the cell dimension collapses to a
// single point: one impl, one match arm in each extern.
//
// The alignment contract lives here, once, as two trust tiers. The
// dense span is ENGINE-OWNED memory — elem_layout allocates it at the
// repr's ALIGN (16 for the u128 cell), so span movements use aligned
// ops by construction and debug asserts police the promise. The FFI
// seam is a BYTE MOVER — the exported *const u8 / *mut u8 contract
// guarantees 8 bytes of alignment, never more — so slot/dst movements
// stay unaligned: not timidity, the only op correct against the
// promise the ABI actually makes. And the 16-byte cell does not ride
// that seam at all: glm_tbl_set_any / glm_tbl_get_any pass the
// whole word in the i128 register pair (glm_any_print's dialect) — a
// register has no alignment.
//
// The fill-loop fast store the IR emits does not pass through these
// cores at all (its raw in-bounds GEP is sound by the density
// invariant), so the fast dense path stays exactly what it is: forced
// by the compiler, opt-in by construction.

mod cell_sealed {
    /// Sealed on purpose: reprs are added HERE, one deliberate impl at
    /// a time — never by a drive-by impl elsewhere in the workspace.
    pub trait Sealed {}
}

/// One element repr a GlmTable can hold, implemented on the cell's own
/// value type: `u8` (bool), `u64` (the machine word), `u128` (the
/// tagged Any word). The methods mirror the three physical movements a
/// cell makes: out of an emitted value slot (`load_slot`), through the
/// overflow map's u128 lane (`to_lane`/`from_lane`), and across the
/// dense span or a caller's dst slot (`store_span`, `load_span`,
/// `store_dst`).
pub trait CellRepr: Copy + cell_sealed::Sealed {
    /// Bytes per cell — the GlmTable esize this repr answers to.
    const ESIZE: usize;

    /// The dense span's allocation alignment — elem_layout demands it
    /// of the allocator at every site, so span movements may use
    /// aligned ops by construction (the engine half of the tiered
    /// alignment law; the tripwire asserts below police it).
    const ALIGN: usize;

    /// Read one cell from a caller's FFI value slot. The seam's byte
    /// pointers promise 8 bytes of alignment, never more, so a repr
    /// wider than 8 bytes reads unaligned (the forgiving half of the
    /// tiered law).
    ///
    /// # Safety
    /// `slot` readable for `ESIZE` bytes.
    unsafe fn load_slot(slot: *const u8) -> Self;

    /// Widen into the overflow map's u128 lane — small reprs sit in
    /// the low bits, zero-extended; the Any word rides whole.
    fn to_lane(self) -> u128;

    /// Narrow from the lane (the exact complement of `to_lane`).
    /// Lane zero is every repr's zero cell — the total read's answer
    /// for an absent entry.
    fn from_lane(lane: u128) -> Self;

    /// Store one cell into the dense span — engine-owned memory,
    /// ALIGN-aligned by elem_layout.
    ///
    /// # Safety
    /// `slot` writable for `ESIZE` bytes, in bounds, and aligned to
    /// `ALIGN` (guaranteed by construction for span cells).
    unsafe fn store_span(slot: *mut u8, cell: Self);

    /// Read one cell out of the dense span — engine-owned memory,
    /// ALIGN-aligned by elem_layout.
    ///
    /// # Safety
    /// `slot` readable for `ESIZE` bytes, in bounds, and aligned to
    /// `ALIGN`.
    unsafe fn load_span(slot: *const u8) -> Self;

    /// Store one cell into a caller's FFI dst slot (the byte face of
    /// the get side) — align 8 by the seam's promise, unaligned for
    /// wide reprs.
    ///
    /// # Safety
    /// `dst` writable for `ESIZE` bytes.
    unsafe fn store_dst(cell: Self, dst: *mut u8);
}

impl cell_sealed::Sealed for u8 {}
impl cell_sealed::Sealed for u64 {}
impl cell_sealed::Sealed for u128 {}

impl CellRepr for u8 {
    const ESIZE: usize = 1;
    const ALIGN: usize = 8;
    unsafe fn load_slot(slot: *const u8) -> Self {
        unsafe { *slot }
    }
    fn to_lane(self) -> u128 {
        u128::from(self)
    }
    fn from_lane(lane: u128) -> Self {
        (lane & 0xff) as u8
    }
    unsafe fn store_span(slot: *mut u8, cell: Self) {
        unsafe { slot.write(cell) };
    }
    unsafe fn load_span(slot: *const u8) -> Self {
        unsafe { slot.read() }
    }
    unsafe fn store_dst(cell: Self, dst: *mut u8) {
        unsafe { dst.write(cell) };
    }
}

impl CellRepr for u64 {
    const ESIZE: usize = 8;
    const ALIGN: usize = 8;
    unsafe fn load_slot(slot: *const u8) -> Self {
        unsafe { (slot as *const u64).read() }
    }
    fn to_lane(self) -> u128 {
        u128::from(self)
    }
    fn from_lane(lane: u128) -> Self {
        lane as u64
    }
    unsafe fn store_span(slot: *mut u8, cell: Self) {
        unsafe { (slot as *mut u64).write(cell) };
    }
    unsafe fn load_span(slot: *const u8) -> Self {
        unsafe { (slot as *const u64).read() }
    }
    unsafe fn store_dst(cell: Self, dst: *mut u8) {
        unsafe { (dst as *mut u64).write(cell) };
    }
}

impl CellRepr for u128 {
    const ESIZE: usize = 16;
    const ALIGN: usize = 16;
    unsafe fn load_slot(slot: *const u8) -> Self {
        unsafe { (slot as *const u128).read_unaligned() }
    }
    fn to_lane(self) -> u128 {
        self
    }
    fn from_lane(lane: u128) -> Self {
        lane
    }
    unsafe fn store_span(slot: *mut u8, cell: Self) {
        // Engine-owned span cell: 16-aligned by elem_layout. The
        // assert is the tripwire — a debug-gauntlet run dies here the
        // moment any allocator math regresses the promise.
        debug_assert!(slot.addr() & 15 == 0, "u128 span cell not 16-aligned");
        unsafe { (slot as *mut u128).write(cell) };
    }
    unsafe fn load_span(slot: *const u8) -> Self {
        debug_assert!(slot.addr() & 15 == 0, "u128 span cell not 16-aligned");
        unsafe { (slot as *const u128).read() }
    }
    unsafe fn store_dst(cell: Self, dst: *mut u8) {
        unsafe { (dst as *mut u128).write_unaligned(cell) };
    }
}

// The match literals in the two exported faces are pinned to the impls
// — change one side without the other and the compiler stops the
// build here. The alignment mapping rides the same pin: elem_layout
// allocates every span at the repr's ALIGN, so the promise the span
// movements rely on cannot drift from the impls either.
const _: () = assert!(u8::ESIZE == 1 && u64::ESIZE == 8 && u128::ESIZE == 16);
const _: () = {
    assert!(u8::ALIGN == 8 && u64::ALIGN == 8 && u128::ALIGN == 16);
    assert!(span_align(u8::ESIZE) == u8::ALIGN);
    assert!(span_align(u64::ESIZE) == u64::ALIGN);
    assert!(span_align(u128::ESIZE) == u128::ALIGN);
};

// === The hang-in demonstration ======================================
// A future construct's cell, hung into the same shell: 16 bytes —
// payload in the low 64, the string symbol ID in bits 64–96, the kind
// tag in bits 96–128 (the spare lane ROADMAP.md claims). Dead by
// design — nothing wires it to the ABI or the compiler — it exists to
// prove the shell: ONE impl block rides the same storage cores every
// dense table runs on today, with zero changes to them. The string-
// key milestone materializes its exported face; the storage below is
// already done.

/// The string-keyed cell of a future construct — see the section
/// comment above for the bit layout and why this compiles.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct KeyedCell(u128);

#[allow(dead_code)]
impl cell_sealed::Sealed for KeyedCell {}

#[allow(dead_code)]
impl CellRepr for KeyedCell {
    const ESIZE: usize = 16;
    const ALIGN: usize = 16;
    unsafe fn load_slot(slot: *const u8) -> Self {
        Self(unsafe { (slot as *const u128).read_unaligned() })
    }
    fn to_lane(self) -> u128 {
        self.0
    }
    fn from_lane(lane: u128) -> Self {
        Self(lane)
    }
    unsafe fn store_span(slot: *mut u8, cell: Self) {
        debug_assert!(slot.addr() & 15 == 0, "KeyedCell span cell not 16-aligned");
        unsafe { (slot as *mut u128).write(cell.0) };
    }
    unsafe fn load_span(slot: *const u8) -> Self {
        debug_assert!(slot.addr() & 15 == 0, "KeyedCell span cell not 16-aligned");
        Self(unsafe { (slot as *const u128).read() })
    }
    unsafe fn store_dst(cell: Self, dst: *mut u8) {
        unsafe { (dst as *mut u128).write_unaligned(cell.0) };
    }
}

/// The demonstration that a future construct reuses the core: a
/// string-keyed store is this one call over its own exported face —
/// the routing (sparse/far/dense), the shadowing, and the alignment
/// contract all come free.
///
/// # Safety
/// `t` a live GlmTable; sketch only, never wired.
#[allow(dead_code)]
unsafe fn keyed_set(t: *mut GlmTable, index: i64, cell: KeyedCell) {
    unsafe { set_core::<KeyedCell>(t, index, cell) };
}

// Phase 3 — C-ABI set / get endpoints (mode-aware routing)

/// Lua-parity death for integer division/modulo by zero, called ahead
/// of every integer sdiv/srem the backend emits (the bare instruction
/// is LLVM UB: it SIGFPEs at runtime, and a CONSTANT zero divisor
/// folded to garbage). The MIN/-1 overflow trap is handled
/// branchlessly in the emitted IR (wraps to MIN / 0), so this guard
/// only owns the zero case. Sidecar-censused like the null-row death.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_div_zero_guard(rhs: i64) {
    if rhs == 0 {
        unsafe { glm_trace_set(TRACE_FAIL_DIV_ZERO) };
        eprintln!("glm runtime error: attempt to perform 'n/0' (integer division by zero)");
        std::process::abort();
    }
}

/// # Safety
/// `t` live from glm_tbl_new, or null: a null `t` is row absence at
/// runtime (the base is a table of tables, this cell holds null) — a
/// language-level error, not a silent drop; the process dies with
/// "glm runtime error: attempt to index a nil value (null table
/// row)" and pokes sidecar slot 71. `val` points to `esize` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_set(t: *mut GlmTable, index: i64, val: *const u8) {
    if t.is_null() {
        // Row absence at the store side: the base is a table of tables
        // (the flow-insensitive proof passed — some row exists), but this
        // cell holds null at runtime. A store needs a real destination:
        // die like Lua ("attempt to index a nil value") — no silent drop
        // of the value.
        unsafe { glm_trace_set(TRACE_FAIL_NULL_ROW_STORE) };
        eprintln!("glm runtime error: attempt to index a nil value (null table row)");
        std::process::abort();
    }
    // The exported face routes the runtime esize to its repr; the
    // sparse/far/dense routing is shared, written once in set_core.
    // (The checker pins esize to 1, 8, or 16 — anything else is a
    // compiler bug and dies loudly instead of mis-storing.)
    match unsafe { (&*t).esize } {
        1 => unsafe { set_core::<u8>(t, index, u8::load_slot(val)) },
        8 => unsafe { set_core::<u64>(t, index, u64::load_slot(val)) },
        16 => unsafe { set_core::<u128>(t, index, u128::load_slot(val)) },
        other => {
            eprintln!("glm runtime error: GlmTable esize {other} has no cell repr");
            std::process::abort();
        }
    }
}

/// The store routing core, written once for every repr: the
/// mode/threshold/shadow routing. The cell arrives BY VALUE — the
/// faces are adapters that produce it (the byte-pointer face loads it
/// from the caller's slot, the register face passes it straight
/// through), so the only physical movements here are the lane ride
/// and the span store. The hand-duplicated esize arms this replaces
/// are exactly where the aligned-u128 bug lived — a duplicated arm
/// cannot exist here.
///
/// # Safety
/// `t` live from glm_tbl_new (non-null).
unsafe fn set_core<C: CellRepr>(t: *mut GlmTable, index: i64, cell: C) {
    let tbl = unsafe { &mut *t };

    // The border: one past the highest index ever stored — `#t`'s
    // answer, maintained here on every store the checked path performs
    // (dense, far-map, and born-Sparse alike). Negative indices name
    // no cell (the drop below), so they never move it.

    // Both faces below drop a negative index: it names no cell in
    // either layout (the array part addresses 0..up; the sparse map
    // is that same array sparsely addressed, not a general key
    // space), so the store is a defined silent no-op — the
    // write-side twin of the total read, which answers the element
    // zero. The drop is also the safety fallback every unproven index
    // rides: the fill-loop fast store is the only path that elides
    // it, by the non-negativity proof.

    // ---- Sparse: direct HashMap insert ----
    if tbl.mode == TableMode::Sparse {
        if index < 0 {
            return;
        }
        tbl.border = tbl.border.max(index.saturating_add(1));
        let map = unsafe { &mut *tbl.sparse_map };
        map.insert(index, cell.to_lane());
        return;
    }

    // ---- Dense: threshold check ----
    // A far key names a cell the dense span will not reach; it rides
    // the overflow map and the span stays exactly as it is. The dense
    // data pointer is therefore permanent — a converted fill loop's
    // raw GEP stores stay in-bounds and live against every store the
    // checked path performs, whatever the key computes to at runtime.
    if index > tbl.len.saturating_add(SPARSE_THRESHOLD) {
        tbl.border = tbl.border.max(index.saturating_add(1));
        unsafe { overflow_map(t).insert(index, cell.to_lane()) };
        return;
    }

    // ---- Dense, in-threshold: span grow then write ----------------------
    if index < 0 {
        return;
    }
    tbl.border = tbl.border.max(index.saturating_add(1));
    unsafe { span_grow(t, index.wrapping_add(1)) };
    let ptr = unsafe { tbl.data.add(index as usize * C::ESIZE) };
    unsafe { C::store_span(ptr, cell) };
    // The dense write is the newer value at this index — shadow away
    // an overflow entry an earlier far store left, so the read-side
    // overlay (map wins while present) cannot resurrect it.
    if !tbl.sparse_map.is_null() {
        unsafe { (&mut *tbl.sparse_map).remove(&index) };
    }
}

/// # Safety
/// `t` live from glm_tbl_new, or null: a null `t` (the unallocated
/// nested row) zero-fills `dst` with the caller-pinned `esize` bytes
/// — a null header carries no size of its own. `dst` points to a
/// caller-allocated slot of `esize` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_get(t: *mut GlmTable, index: i64, dst: *mut u8, esize: usize) {
    if dst.is_null() {
        return;
    }
    // Null table (the unallocated nested row): a null header carries
    // no esize, so the caller — which pins the element size statically —
    // supplies it; the read yields zeros instead of whatever the stack
    // held.
    if t.is_null() {
        std::ptr::write_bytes(dst, 0, esize);
        return;
    }
    // The exported face routes the runtime esize to its repr — the
    // same shell as the store side.
    match unsafe { (&*t).esize } {
        1 => unsafe { u8::store_dst(get_core::<u8>(t, index), dst) },
        8 => unsafe { u64::store_dst(get_core::<u64>(t, index), dst) },
        16 => unsafe { u128::store_dst(get_core::<u128>(t, index), dst) },
        other => {
            eprintln!("glm runtime error: GlmTable esize {other} has no cell repr");
            std::process::abort();
        }
    }
}

/// The read routing core — the total read: sparse map, dense overlay,
/// bounds, zero-fill. Returns the cell BY VALUE; the calling face
/// delivers it (the byte-pointer face through store_dst, the register
/// face straight out of its own return). The zero cell is lane zero
/// narrowed: 0 for every scalar repr, and for the Any word tag 0
/// payload 0 — an Integer zero, LoadAnyZero's convention, the total
/// read's answer for an absent cell.
///
/// # Safety
/// `t` live from glm_tbl_new (non-null).
unsafe fn get_core<C: CellRepr>(t: *mut GlmTable, index: i64) -> C {
    let tbl = unsafe { &mut *t };

    // ---- Sparse: HashMap lookup ----------------------------------------
    if tbl.mode == TableMode::Sparse {
        if index < 0 {
            return C::from_lane(0);
        }
        let map = unsafe { &mut *tbl.sparse_map };
        return match map.get(&index) {
            Some(&lane) => C::from_lane(lane),
            None => C::from_lane(0),
        };
    }

    // ---- Dense: overflow overlay, then bounds check --------------------
    // A far-key cell rides the overflow map even after the span grows
    // past its index, so the map answers first; the dense span answers
    // everything else (absent cells zero-fill, the total read).
    if index >= 0
        && !tbl.sparse_map.is_null()
        && let Some(&lane) = unsafe { (&*tbl.sparse_map).get(&index) }
    {
        return C::from_lane(lane);
    }
    if index < 0 || index >= tbl.len {
        return C::from_lane(0);
    }
    let ptr = unsafe { tbl.data.add(index as usize * C::ESIZE) };
    unsafe { C::load_span(ptr) }
}

/// The register face of the store seam for the tagged Any cell: the
/// whole 16-byte word crosses in the i128 register pair — the same
/// dialect glm_any_print and glm_any_eq already speak — so there is
/// no caller slot and no alignment question at the seam at all. The
/// IR emits this call for every esize-16 store; glm_tbl_set stays
/// frozen for byte-pointer callers.
///
/// # Safety
/// `t` live from glm_tbl_new with `esize == 16`, or null: a null `t`
/// is row absence at runtime — the same Lua-parity death as
/// glm_tbl_set's store side.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_set_any(t: *mut GlmTable, index: i64, val: i128) {
    if t.is_null() {
        // Row absence, register dialect: the same death as the byte
        // face — no silent drop of the value.
        unsafe { glm_trace_set(TRACE_FAIL_NULL_ROW_STORE) };
        eprintln!("glm runtime error: attempt to index a nil value (null table row)");
        std::process::abort();
    }
    match unsafe { (&*t).esize } {
        16 => unsafe { set_core::<u128>(t, index, val as u128) },
        other => {
            eprintln!("glm runtime error: GlmTable esize {other} has no register cell face");
            std::process::abort();
        }
    }
}

/// The register face of the read seam for the tagged Any cell: the
/// cell returns whole in the i128 register pair. A null table (the
/// unallocated nested row) and an absent cell both answer the tagged
/// zero — tag 0 payload 0, an Integer zero, LoadAnyZero's convention.
/// The IR emits this call for every esize-16 read; glm_tbl_get stays
/// frozen for byte-pointer callers.
///
/// # Safety
/// `t` live from glm_tbl_new with `esize == 16`, or null (answers 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_get_any(t: *mut GlmTable, index: i64) -> i128 {
    if t.is_null() {
        return 0;
    }
    match unsafe { (&*t).esize } {
        16 => unsafe { get_core::<u128>(t, index) as i128 },
        other => {
            eprintln!("glm runtime error: GlmTable esize {other} has no register cell face");
            std::process::abort();
        }
    }
}

unsafe fn span_grow(t: *mut GlmTable, want: i64) {
    if want < 0 {
        // A negative reserve reaches here only from a converted
        // fill-loop's zero-trip bound (`while i < -5`): there is
        // nothing to reserve — no-op, not a fatal. No negative STORE
        // reaches this point (glm_tbl_set drops them first).
        return;
    }

    let tbl = unsafe { &mut *t };
    if want <= tbl.len {
        return;
    }
    let new_len = want.max(tbl.len.wrapping_mul(2)).max(8);
    let new_bytes = usize::try_from(new_len)
        .ok()
        .and_then(|l| l.checked_mul(tbl.esize))
        .unwrap_or_else(|| abort_alloc(usize::MAX));
    if tbl.reserve != 0 {
        unsafe { grow_vm(tbl, new_bytes) };
        tbl.len = new_len;
        return;
    }
    if new_bytes >= GRADUATE_AT && vm_enabled() {
        unsafe { graduate(tbl, new_bytes) };
        tbl.len = new_len;
        return;
    }
    let new = elem_layout(new_len, tbl.esize);
    let buf = if tbl.data.is_null() {
        unsafe { alloc_zeroed(new) }
    } else {
        let old = elem_layout(tbl.len, tbl.esize);
        let grown = unsafe { realloc(tbl.data.cast(), old, new.size()) };

        if grown.is_null() {
            abort_alloc(new.size());
        }

        unsafe {
            grown
                .byte_offset((tbl.len * tbl.esize as i64) as isize)
                .write_bytes(0, (new_len - tbl.len) as usize * tbl.esize);
        }
        grown
    };
    tbl.data = buf;
    tbl.len = new_len;
}

const GRADUATE_AT: usize = 1 << 20; // 1 MiB
const RESERVE_FLOOR: usize = 1 << 26; // 64 MiB
const PAGE: usize = 4096;

const PROT_NONE: i32 = 0x0;
const PROT_READ: i32 = 0x1;
const PROT_WRITE: i32 = 0x2;
const MAP_PRIVATE: i32 = 0x02;
const MAP_SHARED: i32 = 0x01;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_NORESERVE: i32 = 0x4000;
const MREMAP_MAYMOVE: i32 = 1;

unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
    -> *mut c_void;
    fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn mremap(addr: *mut c_void, old_len: usize, new_len: usize, flags: i32, ...) -> *mut c_void;
    fn atexit(cb: unsafe extern "C" fn()) -> i32;
}

fn reserve_for(bytes: usize) -> usize {
    bytes
        .max(RESERVE_FLOOR)
        .checked_next_power_of_two()
        .unwrap_or_else(|| abort_alloc(usize::MAX))
}

fn is_map_failed(p: *mut c_void) -> bool {
    p.addr() == usize::MAX
}

fn round_up_page(n: usize) -> usize {
    (n + PAGE - 1) & !(PAGE - 1)
}

fn vm_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STATE: AtomicU8 = AtomicU8::new(0);
    match STATE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let off = std::env::var_os("GLM_VM").is_some_and(|v| v == "off" || v == "0");
            STATE.store(if off { 2 } else { 1 }, Ordering::Relaxed);
            !off
        }
    }
}

unsafe fn graduate(tbl: &mut GlmTable, new_bytes: usize) {
    let reserve = reserve_for(new_bytes);
    let vm = unsafe {
        mmap(
            ptr::null_mut(),
            reserve,
            PROT_NONE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
            -1,
            0,
        )
    };
    if is_map_failed(vm) {
        abort_alloc(reserve);
    }
    let commit = round_up_page(new_bytes);
    if unsafe { mprotect(vm, commit, PROT_READ | PROT_WRITE) } != 0 {
        unsafe { munmap(vm, reserve) };
        abort_alloc(commit);
    }
    if !tbl.data.is_null() {
        unsafe {
            ptr::copy_nonoverlapping(
                tbl.data,
                vm.cast::<u8>(),
                elem_layout(tbl.len, tbl.esize).size(),
            );
            dealloc(tbl.data, elem_layout(tbl.len, tbl.esize));
        }
    }
    tbl.data = vm.cast::<u8>();
    tbl.reserve = reserve;
}

unsafe fn grow_vm(tbl: &mut GlmTable, new_bytes: usize) {
    if new_bytes > tbl.reserve {
        let new_reserve = reserve_for(new_bytes);
        let moved = unsafe { mremap(tbl.data.cast(), tbl.reserve, new_reserve, MREMAP_MAYMOVE) };
        if is_map_failed(moved) {
            abort_alloc(new_reserve);
        }
        tbl.data = moved.cast::<u8>();
        tbl.reserve = new_reserve;
    }
    let commit = round_up_page(new_bytes);
    if unsafe { mprotect(tbl.data.cast(), commit, PROT_READ | PROT_WRITE) } != 0 {
        abort_alloc(commit);
    }
}

/// # Safety
/// `t` live from glm_tbl_new, or null (null is a no-op: frees ride
/// carrier registers, which are null exactly on the paths where an
/// inner drop, rebind, or move already freed or transferred the
/// header).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_free(t: *mut GlmTable) {
    unsafe { free_tbl(t, &[]) };
}

/// The keep-free: the deep free with one pointer-identity skip. Frees
/// `t`'s header, cells, and every row reachable from it — except the
/// subtree rooted at `keep` (compared at every depth, so intermediate
/// shells release while the kept row survives for whoever holds
/// it: the boundary host, or an inline caller). `keep == null` (an
/// out-of-bounds read handed the host nothing) keeps nothing — a
/// plain deep free. `keep == t` frees nothing but the guard itself.
///
/// # Safety
/// `t` live from glm_tbl_new or null; `keep` a GlmTable pointer or
/// null. No allocation freed by this call may be touched afterwards
/// except the kept subtree, which is untouched.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_free_except(t: *mut GlmTable, keep: *mut GlmTable) {
    unsafe { free_tbl(t, &[keep]) };
}

/// The multi-keep twin of glm_tbl_free_except: the deep free of `t`'s
/// whole reachable tree, skipping every subtree rooted at one of the
/// `n` keep pointers (pointer identity, compared at every depth — the
/// comparison precedes any dereference, so a keep naming an already
/// released row still skips soundly without touching it). The block-exit
/// plan's channel: a base dropping while several outer bindings hold
/// rows out of its tree spares them all in one call.
///
/// # Safety
/// `t` live from glm_tbl_new or null; `keeps` readable for `n`
/// GlmTable pointers (null entries keep nothing). No allocation freed
/// by this call may be touched afterwards except the kept subtrees,
/// which are untouched.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_tbl_free_except_n(
    t: *mut GlmTable,
    keeps: *const *mut GlmTable,
    n: usize,
) {
    if keeps.is_null() {
        unsafe { free_tbl(t, &[]) };
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(keeps, n) };
    unsafe { free_tbl(t, slice) };
}

/// The shared free core all exports ride: the deep free of `t`'s
/// whole reachable tree, skipping every node equal to one of the
/// `keeps` (and, with each, everything below — the kept subtrees
/// stay live for their new owners). Membership is a linear scan:
/// one block exit spares a handful of borrower rows, not a set.
///
/// # Safety
/// See the exported wrappers.
unsafe fn free_tbl(t: *mut GlmTable, keeps: &[*mut GlmTable]) {
    if t.is_null() || keeps.contains(&t) {
        return;
    }
    unsafe { glm_trace_set(TRACE_RT_FREE) };
    ALLOC_COUNT.fetch_sub(1, Ordering::Relaxed);
    let tbl = unsafe { Box::from_raw(t) };

    // === Deep-free children if this table contains nested tables ===
    if tbl.contains_tables != 0 {
        // Inline platform-aware validation: avoids zero-page, accounts for
        // ARM64/jemalloc lower bounds, and filters upper kernel/vDSO space.
        let is_valid_ptr = |addr: usize| -> bool {
            #[cfg(target_pointer_width = "64")]
            let is_user_space = addr < 0x0000_7FFF_FFFF_FFFF;
            #[cfg(not(target_pointer_width = "64"))]
            let is_user_space = true;

            // > 16MB lower bound + 8-byte alignment + < 128TB upper bound
            addr > 0x0100_0000 && is_user_space && (addr & 7) == 0
        };

        if tbl.mode == TableMode::Dense && !tbl.data.is_null() {
            let ptrs =
                std::slice::from_raw_parts(tbl.data.cast::<*mut GlmTable>(), tbl.len as usize);
            for &child in ptrs {
                if is_valid_ptr(child as usize) {
                    unsafe { glm_trace_set(TRACE_RT_FREE_CHILD) };
                    unsafe { free_tbl(child, keeps) };
                }
            }
        }
        // Far-key cells ride the overflow map in both modes; a Dense
        // table can carry one alongside its span.
        if !tbl.sparse_map.is_null() {
            let map = unsafe { &*tbl.sparse_map };
            for &child_addr in map.values() {
                if is_valid_ptr(child_addr as usize) {
                    unsafe { glm_trace_set(TRACE_RT_FREE_CHILD) };
                    unsafe { free_tbl(child_addr as *mut GlmTable, keeps) };
                }
            }
        }
    }

    // === Existing flat deallocation path ===
    // The map's value lane is u128 (Any cells ride it whole) — the
    // cast MUST match the allocation: a Box of the wrong element type
    // computes its dealloc layout from the wrong bucket stride and
    // glibc dies with free(): invalid size on the first sparse table.
    if !tbl.sparse_map.is_null() {
        let _ = unsafe {
            Box::from_raw(
                tbl.sparse_map
                    .cast::<std::collections::HashMap<i64, u128>>(),
            )
        };
    }
    if tbl.mode == TableMode::Sparse {
        // Born-sparse tables carry no dense buffer to release.
    } else if !tbl.data.is_null() {
        if tbl.reserve != 0 {
            unsafe { munmap(tbl.data.cast(), tbl.reserve) };
        } else {
            unsafe { dealloc(tbl.data, elem_layout(tbl.len, tbl.esize)) };
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn glm_print_int(val: i64) {
    print!("{val}");
}

#[unsafe(no_mangle)]
pub extern "C" fn glm_print_float(val: f64) {
    print!("{val}");
}

#[unsafe(no_mangle)]
pub extern "C" fn glm_print_bool(val: bool) {
    print!("{val}");
}

/// # Safety
/// `val` NUL-terminated and readable through the terminator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_print_string(val: *const u8) {
    let mut len = 0usize;
    unsafe {
        while *val.add(len) != 0 {
            len += 1;
        }
    }
    let bytes = unsafe { std::slice::from_raw_parts(val, len) };
    print!("{}", String::from_utf8_lossy(bytes));
}

/// `#s`: a string's length, in bytes up to the terminator. The intern
/// gives each literal one address, so this reads the one shared
/// constant — no per-call heap traffic.
///
/// # Safety
/// `val` NUL-terminated and readable through the terminator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_str_len(val: *const u8) -> i64 {
    let mut len = 0usize;
    unsafe {
        while *val.add(len) != 0 {
            len += 1;
        }
    }
    len as i64
}

// === The boundary string intern space ===
// String cells cross the boundary as pointers, and string equality is
// IDENTITY over the intern space — so a boundary word must hold the
// same address the script's own literal holds. The module's distinct
// literals are registered here at load (the backend's .init_array
// registry), and a word matching none of them gets its own immortal
// copy: one flat identity space, pool semantics extended across the
// boundary. All of it rides the deep-free exemption — an interned
// string owns no GlmTable.

use std::sync::Mutex;

static STR_POOL: Mutex<Option<HashMap<Vec<u8>, usize>>> = Mutex::new(None);

fn str_pool() -> std::sync::MutexGuard<'static, Option<HashMap<Vec<u8>, usize>>> {
    // Poisoned is still usable: the map is a cache, never a safety
    // boundary — a torn map only costs an extra intern.
    let mut guard = STR_POOL.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(HashMap::new());
    }
    guard
}

/// The module's literal registry: `ptrs` names `n` NUL-terminated
/// pool constants (the .init_array constructor passes the backend's
/// strtab). Later registrations win nothing — first address per
/// content sticks, and within one module every content is one literal.
///
/// # Safety
/// `ptrs` readable for `n` pointers; each readable through its NUL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_str_register(ptrs: *const *const u8, n: usize) {
    let mut pool = str_pool();
    let Some(map) = pool.as_mut() else {
        return;
    };
    for i in 0..n {
        let p = unsafe { *ptrs.add(i) };
        if p.is_null() {
            continue;
        }
        let bytes = unsafe { cstr_bytes(p) };
        map.entry(bytes.to_vec()).or_insert(p as usize);
    }
}

/// The intern: the address for this content, whatever it takes — the
/// registered literal's address on a match (a pool hit: the cell holds
/// the literal's own address, identity equality with the script's
/// strings, no allocation), else a freshly allocated immortal
/// NUL-terminated copy cached for every later ask. One address per
/// distinct content, for the whole process.
///
/// # Safety
/// `s` readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_str_intern(s: *const u8, len: usize) -> *const u8 {
    let bytes = unsafe { std::slice::from_raw_parts(s, len) };
    {
        let pool = str_pool();
        if let Some(&addr) = pool.as_ref().and_then(|m| m.get(bytes)) {
            unsafe { glm_trace_set(TRACE_RT_STR_POOL_HIT) };
            return addr as *const u8;
        }
    }
    unsafe { glm_trace_set(TRACE_RT_STR_INTERN) };
    let mut buf = Vec::with_capacity(len + 1);
    buf.extend_from_slice(bytes);
    buf.push(0);
    let p = Box::into_raw(buf.into_boxed_slice()) as *const u8;
    let mut pool = str_pool();
    if let Some(map) = pool.as_mut() {
        map.insert(bytes.to_vec(), p as usize);
    }
    p
}

// === The Any (dynamic) boundary cell ===
// An unconstrained `arg` element: a 16-byte tagged cell — payload in
// the low 64 bits, GLM_ARG_* kind tag in the high 64 — riding the
// same GlmTable machinery as every other cell (esize 16). The script
// proved (by checking clean without ever pinning the seed) that it
// only copies, prints, passes along, or returns its cells, so the tag
// travels WITH the value and the two dispatched operations (print,
// equality) read it. The whole word may be copied blind; only these
// two operations look inside. Zero-fill (a total read of an absent
// cell) is tag 0 payload 0 — an Integer zero, the total-read
// convention of every other repr.

/// Pack one tagged cell: tag in the high 64 bits, payload in the low.
#[inline]
fn any_pack(tag: i32, payload: u64) -> i128 {
    ((tag as i64 as i128) << 64) | (payload as i128)
}

/// The GLM_ARG_* tag of one tagged cell.
#[inline]
fn any_tag(v: i128) -> i32 {
    (v >> 64) as i32
}

/// The payload bits of one tagged cell.
#[inline]
fn any_payload(v: i128) -> u64 {
    v as u64
}

/// Print one tagged cell with its own kind's printer — the dispatch
/// the backend emits for `print` of an Any operand. Strings print
/// through the pool pointer the host interned at load.
#[unsafe(no_mangle)]
pub extern "C" fn glm_any_print(v: i128) {
    match any_tag(v) {
        GLM_ARG_INT => print!("{}", any_payload(v) as i64),
        GLM_ARG_FLOAT => print!("{}", f64::from_bits(any_payload(v))),
        GLM_ARG_BOOL => print!("{}", any_payload(v) != 0),
        GLM_ARG_STRING => {
            let p = any_payload(v) as *const u8;
            if !p.is_null() {
                let bytes = unsafe { cstr_bytes(p) };
                print!("{}", String::from_utf8_lossy(bytes));
            }
        }
        _ => print!("{}", any_payload(v) as i64),
    }
}

/// Equality of two tagged cells: equal iff same kind and equal
/// payload — strings by pool identity (the intern guarantees one
/// address per distinct content, so pointer equality IS content
/// equality here). The answer is 0/1, materialized into an i1 by the
/// backend's compare emission.
#[unsafe(no_mangle)]
pub extern "C" fn glm_any_eq(l: i128, r: i128) -> i32 {
    i32::from(l == r)
}

/// `#cell`: the dynamic cell's length — the string kind answers strlen
/// of its interned payload; every other kind DIES, loudly and by name
/// (the checker admitted the operand without ever knowing the kind —
/// the host's CLI word picked it at load — so strictness lands here,
/// as a named runtime death: the glm_div_zero_guard precedent). A
/// silent 0 for non-strings would be a lie a dynamic cell must never
/// tell.
#[unsafe(no_mangle)]
pub extern "C" fn glm_any_len(v: i128) -> i64 {
    let kind_word = |k: i32| match k {
        GLM_ARG_INT => "int",
        GLM_ARG_FLOAT => "float",
        GLM_ARG_BOOL => "bool",
        GLM_ARG_STRING => "string",
        _ => "unknown",
    };
    match any_tag(v) {
        GLM_ARG_STRING => {
            let p = any_payload(v) as *const u8;
            if p.is_null() {
                return 0;
            }
            unsafe { glm_str_len(p) }
        }
        k => {
            unsafe { glm_trace_set(TRACE_FAIL_ANY_LEN) };
            eprintln!(
                "glm runtime error: glm_any_len: '#' demands a string cell, got kind {} ({})",
                k,
                kind_word(k)
            );
            std::process::abort();
        }
    }
}

/// The four written opt-ins into the dynamic cell: pack one scalar as
/// a tagged word — a mixed constructor's entries and a scalar store
/// into an Any table lower to these. The tag is written exactly once,
/// INSIDE the helpers (the emitted IR names the helper and never the
/// tag — tag blindness holds on the pack side exactly as on the read
/// side).
#[unsafe(no_mangle)]
pub extern "C" fn glm_any_from_int(v: i64) -> i128 {
    any_pack(GLM_ARG_INT, v as u64)
}

/// See `glm_any_from_int` — the float packs its bits.
#[unsafe(no_mangle)]
pub extern "C" fn glm_any_from_float(v: f64) -> i128 {
    any_pack(GLM_ARG_FLOAT, v.to_bits())
}

/// See `glm_any_from_int` — the ABI passes the i1 as an i8 0/1.
#[unsafe(no_mangle)]
pub extern "C" fn glm_any_from_bool(v: bool) -> i128 {
    any_pack(GLM_ARG_BOOL, u64::from(v))
}

/// See `glm_any_from_int` — a string packs its interned pointer (the
/// pool's identity guarantee rides the payload untouched).
///
/// # Safety
/// `p` NUL-terminated and readable through the terminator for as long
/// as the cell lives (an interned literal: forever).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_any_from_str(p: *const u8) -> i128 {
    any_pack(GLM_ARG_STRING, p as u64)
}

/// One word classified under the declared precedence — pure, no
/// interning, no side effects. The dev-loop host (whose process holds
/// TWO runtime instances: the compiler's rlib copy and the .so's
/// staticlib copy) classifies through this and interns through the
/// .so's own glm_str_intern, so the boundary's string identity space
/// and the sidecar trace stay owned by the module's instance.
pub enum AnyWord {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str,
}

/// Classify one boundary word under the declared precedence: int,
/// then float, then bool, then string. `5` classifies Int (documented,
/// deterministic — a property of the boundary vocabulary, not a guess
/// about the script); `2.5` Float; `true`/`false` Bool; anything else
/// String. Every word classifies, so Any never refuses a word. The
/// ONE grammar both hosts share.
pub fn classify_word(w: &[u8]) -> AnyWord {
    if let Ok(s) = std::str::from_utf8(w) {
        if let Ok(v) = s.parse::<i64>() {
            return AnyWord::Int(v);
        }
        if let Ok(v) = s.parse::<f64>() {
            return AnyWord::Float(v);
        }
        match s {
            "true" => return AnyWord::Bool(true),
            "false" => return AnyWord::Bool(false),
            _ => {}
        }
    }
    AnyWord::Str
}

/// Pack one tagged cell — the format every consumer of the Any
/// contract speaks: GLM_ARG_* tag in the high 64 bits, payload in the
/// low 64. Exported so the compiler's dev-loop host packs cells with
/// the module's format without linking the interning path.
#[unsafe(no_mangle)]
pub extern "C" fn glm_any_pack(tag: i32, payload: u64) -> i128 {
    any_pack(tag, payload)
}

/// Parse one boundary word into a tagged cell — the single-instance
/// form (the exe host's path: classify, then intern through THIS
/// runtime instance, the only one in the process).
///
/// # Safety
/// `w` any bytes (interned or copied, never stored beyond the call).
unsafe fn any_of_word(w: &[u8]) -> i128 {
    match classify_word(w) {
        AnyWord::Int(v) => any_pack(GLM_ARG_INT, v as u64),
        AnyWord::Float(v) => any_pack(GLM_ARG_FLOAT, v.to_bits()),
        AnyWord::Bool(v) => any_pack(GLM_ARG_BOOL, u64::from(v)),
        AnyWord::Str => {
            let p = unsafe { glm_str_intern(w.as_ptr(), w.len()) };
            any_pack(GLM_ARG_STRING, p as u64)
        }
    }
}

/// # Safety
/// `p` NUL-terminated and readable through the terminator.
unsafe fn cstr_bytes(p: *const u8) -> &'static [u8] {
    let mut len = 0usize;
    unsafe {
        while *p.add(len) != 0 {
            len += 1;
        }
    }
    unsafe { std::slice::from_raw_parts(p, len) }
}

// === The standalone host ===
// The exe twin of the compiler's dev-loop host: the boundary element
// type is pinned at COMPILE time (the checker's usage inference) and
// embedded in @main as one of these constants, so the executable
// carries its own arg contract — no compiler process, no dlopen. The
// words cross at exec time, through the same glm_tbl_* calls and the
// same intern pool (the module's literal registry already ran via
// .init_array before main). The same constants are the module's
// exported glm_arg_kind() answer, so ANY host (C, LuaJIT FFI, a Rust
// runner) queries the same contract the embedded host parses with.
pub const GLM_ARG_NONE: i32 = -1;
pub const GLM_ARG_INT: i32 = 0;
pub const GLM_ARG_FLOAT: i32 = 1;
pub const GLM_ARG_BOOL: i32 = 2;
pub const GLM_ARG_STRING: i32 = 3;
/// The dynamic boundary cell: an unconstrained `arg` element. The
/// script only copies, prints, passes along, or returns its cells
/// (any typed use would have pinned a concrete scalar), so the cells
/// carry their own tag and the HOST's words pick each cell's kind at
/// load — the one place dynamics are allowed.
pub const GLM_ARG_ANY: i32 = 4;

/// The GLM_ARG_* kind of a boundary element type — the one mapping
/// every consumer of the exported contract speaks (the backend's
/// glm_arg_kind() body, @main's embedded constant, and both hosts).
pub fn arg_kind_of(elem: &crate::GlmElem) -> i32 {
    match elem {
        crate::GlmElem::Float => GLM_ARG_FLOAT,
        crate::GlmElem::Boolean => GLM_ARG_BOOL,
        crate::GlmElem::String => GLM_ARG_STRING,
        crate::GlmElem::Any => GLM_ARG_ANY,
        crate::GlmElem::Integer => GLM_ARG_INT,
    }
}

/// The boundary element type a GLM_ARG_* kind names — the inverse of
/// arg_kind_of. A kind outside the constants maps to None (the
/// argless module: the script never names `arg`).
pub fn elem_of_kind(kind: i32) -> Option<crate::GlmElem> {
    match kind {
        GLM_ARG_INT => Some(crate::GlmElem::Integer),
        GLM_ARG_FLOAT => Some(crate::GlmElem::Float),
        GLM_ARG_BOOL => Some(crate::GlmElem::Boolean),
        GLM_ARG_STRING => Some(crate::GlmElem::String),
        GLM_ARG_ANY => Some(crate::GlmElem::Any),
        _ => None,
    }
}

fn arg_kind_words(kind: i32) -> &'static str {
    match kind {
        GLM_ARG_FLOAT => "numbers (64-bit floats)",
        GLM_ARG_BOOL => "booleans ('true'/'false')",
        GLM_ARG_STRING => "strings (any word)",
        // Any: every word parses — the tags differ per cell, chosen by
        // the declared precedence below.
        GLM_ARG_ANY => "int, float, bool, or string words",
        _ => "integers (64-bit)",
    }
}

fn arg_kind_table(kind: i32) -> &'static str {
    match kind {
        GLM_ARG_FLOAT => "Table<Float>",
        GLM_ARG_BOOL => "Table<Boolean>",
        GLM_ARG_STRING => "Table<String>",
        GLM_ARG_ANY => "Table<Any>",
        _ => "Table<Integer>",
    }
}

/// Parse and store one boundary word per the pinned kind. The value's
/// address is a local — glm_tbl_set copies the bytes immediately. The
/// ANY kind parses every word into a tagged cell (the declared
/// precedence int → float → bool → string, the same grammar the
/// dev-loop host's parse_word applies) and stores it through the
/// register face — no value slot at all.
///
/// # Safety
/// `t` live from glm_tbl_new; `w` any bytes.
unsafe fn set_word(t: *mut GlmTable, i: i64, w: &[u8], kind: i32) -> Result<(), ()> {
    match kind {
        GLM_ARG_INT => {
            let v: i64 = String::from_utf8_lossy(w).parse().map_err(|_| ())?;
            unsafe { glm_tbl_set(t, i, (&v as *const i64).cast()) };
        }
        GLM_ARG_FLOAT => {
            let v: f64 = String::from_utf8_lossy(w).parse().map_err(|_| ())?;
            unsafe { glm_tbl_set(t, i, (&v as *const f64).cast()) };
        }
        GLM_ARG_BOOL => {
            let v = match w {
                b"true" => 1u8,
                b"false" => 0u8,
                _ => return Err(()),
            };
            unsafe { glm_tbl_set(t, i, (&v as *const u8).cast()) };
        }
        GLM_ARG_STRING => {
            let p = unsafe { glm_str_intern(w.as_ptr(), w.len()) };
            unsafe { glm_tbl_set(t, i, (&p as *const *const u8).cast()) };
        }
        GLM_ARG_ANY => {
            let v = unsafe { any_of_word(w) };
            unsafe { glm_tbl_set_any(t, i, v) };
        }
        _ => return Err(()),
    }
    Ok(())
}

/// The standalone entry's whole host role: parse the CLI words
/// (argv[1..]) per the compile-time pinned element kind, build the
/// boundary table, call the module's @glm_exec, and free both tables —
/// the identity check freeing `return arg` exactly once. A word that
/// fails to parse dies with the same message the dev-loop host prints,
/// exit code 1. The NONE kind names the argless form (the script never
/// names `arg`): no table is built, @glm_exec receives null, and the
/// extra words are simply not the script's business.
///
/// # Safety
/// `argv` readable for `argc` NUL-terminated pointers (or null with
/// `argc` <= 0); `exec` the module's @glm_exec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glm_exec_main(
    argc: i32,
    argv: *const *const u8,
    kind: i32,
    exec: unsafe extern "C" fn(*mut GlmTable) -> *mut GlmTable,
) -> i32 {
    if kind == GLM_ARG_NONE {
        let result = exec(std::ptr::null_mut());
        unsafe { glm_tbl_free(result) };
        return 0;
    }
    let mut words: Vec<&[u8]> = Vec::new();
    if !argv.is_null() {
        for i in 1..(argc.max(0) as usize) {
            let p = unsafe { *argv.add(i) };
            if !p.is_null() {
                words.push(unsafe { cstr_bytes(p) });
            }
        }
    }
    let esize = match kind {
        GLM_ARG_BOOL => 1,
        GLM_ARG_ANY => 16,
        _ => 8,
    };
    let args = unsafe { glm_tbl_new(esize, 0) };
    for (i, w) in words.iter().enumerate() {
        if unsafe { set_word(args, i as i64, w, kind) }.is_err() {
            eprintln!(
                "glm error: boundary args must be {}, got '{}' — the arg table is {}",
                arg_kind_words(kind),
                String::from_utf8_lossy(w),
                arg_kind_table(kind),
            );
            unsafe { glm_tbl_free(args) };
            return 1;
        }
    }
    let result = exec(args);
    unsafe {
        if result != args {
            glm_tbl_free(args);
        }
        glm_tbl_free(result);
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn glm_print_sep() {
    print!("\t");
}

#[unsafe(no_mangle)]
pub extern "C" fn glm_print_nl() {
    println!();
}

/// Live table-header count (ALLOC_COUNT): +1 per glm_tbl_new, -1 per
/// glm_tbl_free (each recursive deep-free child included). Live
/// headers, not bytes; not monotonic; the exit leak report is a
/// report, not reclamation.
#[unsafe(no_mangle)]
pub extern "C" fn sys_alloc_count() -> i64 {
    ALLOC_COUNT.load(Ordering::Relaxed) as i64
}

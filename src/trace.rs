use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

include!(concat!(env!("OUT_DIR"), "/trace_signals.rs"));

// The plate is the chronology and nothing else:
//   bytes 0..4   total events fired this run (u32 LE, the writer's sequence)
//   bytes 4..8   ring capacity in events (u32 LE)
//   bytes 8..    the ring — one slot byte per event, in fire order;
//                past capacity the oldest event is overwritten
const TRACE_FILE: &str = ".glm_trace.bin";
pub const TRACE_RING_CAPACITY: u32 = 8192;
const TRACE_FIRED_OFF: usize = 0;
const TRACE_CAPACITY_OFF: usize = 4;
const TRACE_RING_OFF: usize = 8;
const TRACE_PLATE_LEN: usize = TRACE_RING_OFF + TRACE_RING_CAPACITY as usize;

static TRACE_MAP: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());

const PROT_READ: i32 = 0x1;
const PROT_WRITE: i32 = 0x2;
const MAP_SHARED: i32 = 0x01;

unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
    -> *mut c_void;
}

fn is_map_failed(p: *mut c_void) -> bool {
    p.addr() == usize::MAX
}

pub fn compiler_trace_init() {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;
    // Per-run plate: the previous compile's file is removed before this
    // run writes, so every plate a decoder reads starts all-zero and
    // carries nothing from an earlier run.
    let _ = std::fs::remove_file(TRACE_FILE);
    if let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(TRACE_FILE)
    {
        let _ = file.set_len(TRACE_PLATE_LEN as u64);
        let ptr = unsafe {
            mmap(
                ptr::null_mut(),
                TRACE_PLATE_LEN,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if !is_map_failed(ptr) {
            let map = ptr.cast::<u8>();
            TRACE_MAP.store(map, Ordering::SeqCst);
            unsafe {
                // total events starts at 0 (the file is fresh); announce
                // the capacity so the plate self-describes its ring.
                (map.add(TRACE_CAPACITY_OFF) as *mut u32).write(TRACE_RING_CAPACITY);
            }
        }
    }
}

/// One fire, one event: append `slot` to the ring. A bare store, so
/// tracing survives crashes (the panic hook pokes BUILD_FAIL through
/// here) with no flush step.
pub fn compiler_trace_signal(slot: u8) {
    let map = TRACE_MAP.load(Ordering::Relaxed);
    if map.is_null() {
        return;
    }
    unsafe {
        let seq = (*map.add(TRACE_FIRED_OFF).cast::<AtomicU32>()).fetch_add(1, Ordering::Relaxed);
        let ring_idx = TRACE_RING_OFF + (seq as usize % TRACE_RING_CAPACITY as usize);
        *map.add(ring_idx) = slot;
    }
}

#[macro_export]
macro_rules! signal {
    ($slot:expr) => {
        $crate::trace::compiler_trace_signal($slot)
    };
    ($gate:expr,$slot:expr) => {
        if $gate {
            $crate::trace::compiler_trace_signal($slot)
        }
    };
}

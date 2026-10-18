//! Bare-metal sizing demo for the §4.1 gates: the whole engine
//! exercised (create/insert/select over MemStorage) from a
//! `no_std` binary on `thumbv7em-none-eabihf`, `--profile tiny`.
//! On hosted targets the same code builds as a normal binary.
//!
//! The bump allocator below is *demo scaffolding*: its static heap
//! is caller-supplied memory (where the §4.1 RAM budget lives),
//! not engine statics — the gate measurement subtracts it.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

extern crate alloc;

use hipp::storage::block_on;
use hipp::{Config, Connection, MemStorage, Step};

#[cfg(target_os = "none")]
mod bare {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;
    use core::ptr::null_mut;

    pub const HEAP_BYTES: usize = 96 * 1024;

    pub struct Bump {
        heap: UnsafeCell<[u8; HEAP_BYTES]>,
        next: UnsafeCell<usize>,
    }

    unsafe impl Sync for Bump {}

    unsafe impl GlobalAlloc for Bump {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let heap = unsafe { &mut *self.heap.get() };
            let next = unsafe { &mut *self.next.get() };
            let align = layout.align().max(8);
            let start = (*next + align - 1) & !(align - 1);
            let end = start + layout.size();
            if end > HEAP_BYTES {
                return null_mut();
            }
            *next = end;
            unsafe { heap.as_mut_ptr().add(start) }
        }
        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
            // Bump: scopes die with the process in this demo.
        }
    }

    #[global_allocator]
    static ALLOC: Bump = Bump {
        heap: UnsafeCell::new([0; HEAP_BYTES]),
        next: UnsafeCell::new(0),
    };

    #[panic_handler]
    fn panic(_info: &core::panic::PanicInfo) -> ! {
        loop {}
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn _start() -> ! {
        crate::run();
        loop {}
    }
}

fn run() {
    block_on(async {
        let mut db = Connection::open(MemStorage::new(), Config::tiny())
            .await
            .expect("open");
        let mut st = db
            .prepare("CREATE TABLE t (id INTEGER PRIMARY KEY, payload TEXT)")
            .await
            .expect("prep create");
        assert_eq!(st.step().await.expect("create"), Step::Done);
        drop(st);
        let payload = "demo-payload";
        for i in 1..=50i64 {
            let mut buf = alloc::string::String::new();
            buf.push_str("INSERT INTO t VALUES (");
            push_i64(&mut buf, i);
            buf.push_str(", '");
            buf.push_str(payload);
            buf.push_str("')");
            let mut st = db.prepare(&buf).await.expect("prep insert");
            assert_eq!(st.step().await.expect("insert"), Step::Done);
            drop(st);
        }
        let mut q = db
            .prepare("SELECT id, payload FROM t")
            .await
            .expect("prep select");
        let mut n = 0i64;
        while q.step().await.expect("step") == Step::Row {
            n += 1;
            assert_eq!(q.column_i64(0), Some(n));
        }
        assert_eq!(n, 50);
    });
}

fn push_i64(s: &mut alloc::string::String, mut v: i64) {
    if v == 0 {
        s.push('0');
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    s.push_str(core::str::from_utf8(&tmp[i..]).expect("digits"));
}

#[cfg(not(target_os = "none"))]
fn main() {
    run();
    println!("hipp_thumb_demo: ok");
}

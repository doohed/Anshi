//! A unidirectional in-kernel byte ring -- the transport under pipe() and
//! use_as_stdio(). Deliberately non-blocking only: the tick only preempts
//! *ring-3* code, never ring 0, so a busy-spin inside a syscall waiting on
//! another process would never yield the CPU to that process -- a real
//! deadlock, not a style issue. Every operation here returns at once;
//! callers that want blocking semantics loop in their own ring-3 code
//! (safely preemptible), as anshi's read_key() does.

use alloc::boxed::Box;

pub const BUF_SIZE: usize = 4096;

pub struct Pipe {
    buf: [u8; BUF_SIZE],
    /// Index of the next byte to read.
    head: usize,
    /// Bytes currently buffered.
    count: usize,
    /// Reference counts, not booleans -- fork() copies open_files, so
    /// after one both parent and child hold separate entries referencing
    /// the *same* pipe. A plain "is this end open" flag would let one
    /// process's close() of its own copy close the end out from under a
    /// sibling still using it -- found exactly this way, by the pipetest
    /// self-test's exec()'d child silently capturing zero bytes (the
    /// child's close() of its unneeded read-end copy closed the *shared*
    /// read end before the exec'd program ever got to write). create()
    /// sets both to 1 (the two entries pipe() itself makes);
    /// process::fork() bumps them for every pipe entry it copies.
    pub read_refs: i32,
    pub write_refs: i32,
}

/// A new pipe, both ends referenced once.
pub fn create() -> *mut Pipe {
    Box::into_raw(Box::new(Pipe { buf: [0; BUF_SIZE], head: 0, count: 0, read_refs: 1, write_refs: 1 }))
}

impl Pipe {
    /// Copies in as much of `data` as fits -- a short write, never a
    /// block. The byte count, or None with no read-end references left.
    pub fn write(&mut self, data: &[u8]) -> Option<usize> {
        if self.read_refs == 0 {
            return None;
        }
        let n = data.len().min(BUF_SIZE - self.count);
        let tail = (self.head + self.count) % BUF_SIZE;
        for (i, &b) in data[..n].iter().enumerate() {
            self.buf[(tail + i) % BUF_SIZE] = b;
        }
        self.count += n;
        Some(n)
    }

    /// Copies out up to `out.len()` bytes: Some(n > 0) if any were
    /// buffered, Some(0) if empty but a write end is still referenced (try
    /// again later), None if empty with every write end gone (EOF).
    pub fn read(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.count == 0 {
            return if self.write_refs > 0 { Some(0) } else { None };
        }
        let n = out.len().min(self.count);
        for (i, b) in out[..n].iter_mut().enumerate() {
            *b = self.buf[(self.head + i) % BUF_SIZE];
        }
        self.head = (self.head + n) % BUF_SIZE;
        self.count -= n;
        Some(n)
    }
}

fn free_if_unused(p: *mut Pipe) {
    let pipe = unsafe { &*p };
    if pipe.read_refs == 0 && pipe.write_refs == 0 {
        drop(unsafe { Box::from_raw(p) });
    }
}

/// Drops one reference to the read end; frees the pipe once *both* ends
/// have none. Called once per entry that stops referencing it -- close(),
/// process exit.
pub fn close_read(p: *mut Pipe) {
    unsafe { (*p).read_refs -= 1 };
    free_if_unused(p);
}

pub fn close_write(p: *mut Pipe) {
    unsafe { (*p).write_refs -= 1 };
    free_if_unused(p);
}

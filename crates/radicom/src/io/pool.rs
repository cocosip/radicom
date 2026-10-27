//! Reusable large-buffer pool (MEM-01; docs/05-file-io.md, docs/01-feature-list.md).
//!
//! Parser scratch buffers and chunked copies need temporary byte buffers that are
//! large but short-lived. Allocating and freeing those repeatedly is wasteful, so
//! this module hands out [`PooledBuffer`] leases drawn from a [`BufferPool`]; when
//! a lease is dropped its allocation returns to the pool for reuse, up to a cap.
//!
//! This is the "pooled allocator / large-buffer lease" abstraction from MEM-01.

use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, OnceLock};

/// Default cap on the total bytes kept for reuse by the shared pool (16 MiB).
pub const DEFAULT_MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;

/// A pool of reusable byte buffers.
#[derive(Debug)]
pub struct BufferPool {
    free: Mutex<Vec<Vec<u8>>>,
    retained: Mutex<usize>,
    max_retained_bytes: usize,
}

impl Default for BufferPool {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_RETAINED_BYTES)
    }
}

impl BufferPool {
    /// Creates a pool that keeps at most `max_retained_bytes` of buffer capacity
    /// for reuse; further returns are discarded.
    pub fn new(max_retained_bytes: usize) -> Self {
        BufferPool {
            free: Mutex::new(Vec::new()),
            retained: Mutex::new(0),
            max_retained_bytes,
        }
    }

    /// The process-wide pool used by convenience helpers.
    pub fn shared() -> &'static BufferPool {
        static POOL: OnceLock<BufferPool> = OnceLock::new();
        POOL.get_or_init(BufferPool::default)
    }

    /// Takes a lease whose buffer is at least `min_len` bytes long.
    ///
    /// A pooled buffer with enough capacity is reused (its length set to
    /// `min_len`); otherwise a new one is allocated. The contents are unspecified
    /// and must be written before reading.
    pub fn lease(&self, min_len: usize) -> PooledBuffer<'_> {
        let mut free = self.free.lock().expect("pool lock");
        let mut bytes = match free.pop() {
            Some(buffer) => {
                let cap = buffer.capacity();
                {
                    let mut retained = self.retained.lock().expect("pool lock");
                    *retained = retained.saturating_sub(cap);
                }
                buffer
            }
            None => Vec::new(),
        };
        if bytes.capacity() < min_len {
            bytes.reserve(min_len - bytes.capacity());
        }
        bytes.clear();
        bytes.resize(min_len, 0); // zero-filled for the whole lease
        PooledBuffer { bytes, pool: self }
    }

    /// Returns a buffer's storage to the pool when it fits, otherwise drops it.
    fn recycle(&self, bytes: Vec<u8>) {
        let cap = bytes.capacity();
        let mut retained = self.retained.lock().expect("pool lock");
        if cap == 0 || *retained + cap > self.max_retained_bytes {
            return; // drop: over the retention budget
        }
        *retained += cap;
        drop(retained);
        self.free.lock().expect("pool lock").push(bytes);
    }
}

/// A leased byte buffer that returns to its [`BufferPool`] when dropped.
pub struct PooledBuffer<'a> {
    bytes: Vec<u8>,
    pool: &'a BufferPool,
}

impl PooledBuffer<'_> {
    /// The lease contents.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// The lease contents, mutable.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// The lease length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True when the lease is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Resizes the lease, filling any growth with zeroes.
    pub fn resize(&mut self, new_len: usize) {
        self.bytes.resize(new_len, 0);
    }
}

impl Deref for PooledBuffer<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl DerefMut for PooledBuffer<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

impl std::fmt::Debug for PooledBuffer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledBuffer").field("len", &self.bytes.len()).finish()
    }
}

impl Drop for PooledBuffer<'_> {
    fn drop(&mut self) {
        let bytes = std::mem::take(&mut self.bytes);
        self.pool.recycle(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_has_at_least_the_requested_length() {
        let pool = BufferPool::new(DEFAULT_MAX_RETAINED_BYTES);
        let lease = pool.lease(1024);
        assert_eq!(lease.len(), 1024);
        assert!(!lease.is_empty());
    }

    #[test]
    fn a_returned_buffer_is_reused_by_the_next_lease() {
        let pool = BufferPool::new(DEFAULT_MAX_RETAINED_BYTES);
        let ptr = {
            let lease = pool.lease(4096);
            lease.as_slice().as_ptr()
        };
        // after the drop the storage should be recycled; the next lease reuses it
        let reused = pool.lease(4096);
        assert_eq!(reused.as_slice().as_ptr(), ptr, "the same allocation is reused");
    }

    #[test]
    fn a_larger_request_grows_the_reused_buffer() {
        let pool = BufferPool::new(DEFAULT_MAX_RETAINED_BYTES);
        drop(pool.lease(64));
        let lease = pool.lease(10_000);
        assert_eq!(lease.len(), 10_000, "the buffer grows to satisfy the bigger request");
    }

    #[test]
    fn leases_are_zeroed_on_reuse() {
        let pool = BufferPool::new(DEFAULT_MAX_RETAINED_BYTES);
        {
            let mut lease = pool.lease(8);
            lease.as_mut_slice().copy_from_slice(&[9, 9, 9, 9, 9, 9, 9, 9]);
        }
        let lease = pool.lease(8);
        assert_eq!(lease.as_slice(), &[0u8; 8], "reused buffers are zeroed");
    }

    #[test]
    fn nothing_is_retained_beyond_the_budget() {
        let pool = BufferPool::new(1024);
        // a buffer far larger than the budget is dropped, not retained
        drop(pool.lease(4096));
        let retained = *pool.retained.lock().unwrap();
        assert_eq!(retained, 0, "an over-budget buffer is not kept");
    }

    #[test]
    fn the_shared_pool_lends_and_reclaims() {
        let pool = BufferPool::shared();
        {
            let mut lease = pool.lease(128);
            lease[0] = 42;
        }
        // smoke: subsequent leases still work after a return
        let lease = pool.lease(128);
        assert_eq!(lease.len(), 128);
    }

    #[test]
    fn resize_and_deref_work() {
        let pool = BufferPool::new(DEFAULT_MAX_RETAINED_BYTES);
        let mut lease = pool.lease(4);
        lease.resize(10);
        assert_eq!(lease.len(), 10);
        lease[9] = 7;
        assert_eq!(lease[9], 7);
    }
}

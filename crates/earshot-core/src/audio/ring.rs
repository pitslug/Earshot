//! Fixed-capacity FIFO of samples for streaming.

use super::Sample;

/// A fixed-capacity FIFO of samples. Storage is allocated once in [`RingBuffer::new`];
/// no method allocates afterwards, so it is safe on a real-time path.
#[derive(Debug, Clone)]
pub struct RingBuffer {
    buf: Box<[Sample]>,
    /// Index of the oldest sample.
    head: usize,
    len: usize,
}

impl RingBuffer {
    /// Creates an empty buffer holding up to `capacity` samples.
    ///
    /// # Panics
    /// If `capacity` is 0.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ring buffer capacity must be non-zero");
        Self {
            buf: vec![0.0; capacity].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    /// Maximum number of samples held.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Number of samples held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True if no samples are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True if `len() == capacity()`.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.len == self.buf.len()
    }

    /// Free space in samples.
    #[must_use]
    pub fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    /// Drops all samples.
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Appends as many samples from the front of `src` as fit and returns how many were
    /// written. Samples that do not fit are not written.
    pub fn push(&mut self, src: &[Sample]) -> usize {
        let n = src.len().min(self.free());
        let cap = self.buf.len();
        let tail = (self.head + self.len) % cap;
        let first = n.min(cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&src[..first]);
        self.buf[..n - first].copy_from_slice(&src[first..n]);
        self.len += n;
        n
    }

    /// Appends all of `src`, discarding the oldest samples to make room. Returns the number
    /// of samples discarded (old ones, plus the front of `src` if it is longer than the
    /// capacity). Afterwards the buffer ends with the last `min(src.len(), capacity)`
    /// samples of `src`.
    pub fn push_overwrite(&mut self, src: &[Sample]) -> usize {
        let cap = self.buf.len();
        if src.len() >= cap {
            let dropped = self.len + (src.len() - cap);
            self.buf.copy_from_slice(&src[src.len() - cap..]);
            self.head = 0;
            self.len = cap;
            return dropped;
        }
        let dropped = self.discard(src.len().saturating_sub(self.free()));
        self.push(src);
        dropped
    }

    /// Copies samples starting `offset` samples after the oldest into `dst` without
    /// removing them. Returns the number copied: `min(dst.len(), len() - offset)`, or 0 if
    /// `offset >= len()`.
    pub fn peek_at(&self, offset: usize, dst: &mut [Sample]) -> usize {
        if offset >= self.len {
            return 0;
        }
        let n = dst.len().min(self.len - offset);
        let cap = self.buf.len();
        let start = (self.head + offset) % cap;
        let first = n.min(cap - start);
        dst[..first].copy_from_slice(&self.buf[start..start + first]);
        dst[first..n].copy_from_slice(&self.buf[..n - first]);
        n
    }

    /// Copies the oldest samples into `dst` without removing them; returns the count.
    pub fn peek(&self, dst: &mut [Sample]) -> usize {
        self.peek_at(0, dst)
    }

    /// Removes up to `n` of the oldest samples and returns how many were removed.
    pub fn discard(&mut self, n: usize) -> usize {
        let n = n.min(self.len);
        self.head = (self.head + n) % self.buf.len();
        self.len -= n;
        if self.len == 0 {
            self.head = 0;
        }
        n
    }

    /// Moves the oldest samples into `dst`; returns the count.
    pub fn pop(&mut self, dst: &mut [Sample]) -> usize {
        let n = self.peek(dst);
        self.discard(n)
    }
}

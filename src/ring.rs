/// A circular byte buffer for capturing PTY output.
///
/// Writes append to the tail; when full, the oldest bytes (at the head)
/// are overwritten. `snapshot()` returns a contiguous copy of the current
/// contents — safe to call while the ring is still being written to by
/// the server event loop (the copy ensures no concurrent-read hazards).
#[derive(Debug)]
pub struct RingBuffer {
    buf: Box<[u8]>,
    /// Index of the oldest byte (read position). Only meaningful when len > 0.
    head: usize,
    /// Index of the next write position.
    tail: usize,
    /// Number of valid bytes currently stored.
    len: usize,
}

impl RingBuffer {
    /// Create a new ring buffer with the given capacity.
    ///
    /// # Panics
    /// Panics if `capacity` is 0.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ring buffer capacity must be > 0");
        Self {
            buf: vec![0u8; capacity].into_boxed_slice(),
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    /// Total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Number of bytes currently stored.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True if the buffer is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Append `data` to the ring buffer. If `data` is larger than capacity,
    /// only the last `capacity` bytes are kept.
    pub fn write(&mut self, data: &[u8]) {
        let cap = self.buf.len();

        // If data is bigger than the whole buffer, only keep the tail end.
        if data.len() >= cap {
            let start = data.len() - cap;
            self.buf.copy_from_slice(&data[start..]);
            self.head = 0;
            self.tail = 0;
            self.len = cap;
            return;
        }

        // Write in up to two chunks (wrap around).
        let first = cap - self.tail; // space from tail to end of buffer
        if data.len() <= first {
            self.buf[self.tail..self.tail + data.len()].copy_from_slice(data);
        } else {
            self.buf[self.tail..self.tail + first].copy_from_slice(&data[..first]);
            let rest = data.len() - first;
            self.buf[..rest].copy_from_slice(&data[first..]);
        }

        // Advance tail.
        self.tail = (self.tail + data.len()) % cap;

        // If we overwrote data, advance head to match.
        let new_len = self.len + data.len();
        if new_len > cap {
            self.head = self.tail; // head catches up to tail
            self.len = cap;
        } else {
            self.len = new_len;
        }
    }

    /// Return a contiguous copy of the buffer contents, oldest bytes first.
    /// This is the replay snapshot — safe to call at any time.
    pub fn snapshot(&self) -> Vec<u8> {
        if self.len == 0 {
            return Vec::new();
        }

        let cap = self.buf.len();
        let mut out = Vec::with_capacity(self.len);

        if self.head < self.tail || self.len < cap {
            // Data is contiguous: head..head+len
            if self.head + self.len <= cap {
                out.extend_from_slice(&self.buf[self.head..self.head + self.len]);
            } else {
                // Wraps around
                out.extend_from_slice(&self.buf[self.head..]);
                let rest = self.len - (cap - self.head);
                out.extend_from_slice(&self.buf[..rest]);
            }
        } else {
            // head == tail and len == cap — buffer is full
            out.extend_from_slice(&self.buf[self.head..]);
            if self.head > 0 {
                out.extend_from_slice(&self.buf[..self.head]);
            }
        }

        out
    }

    /// Clear the buffer.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.len = 0;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer() {
        let ring = RingBuffer::new(16);
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());
        assert_eq!(ring.snapshot(), Vec::<u8>::new());
    }

    #[test]
    fn simple_write_and_snapshot() {
        let mut ring = RingBuffer::new(16);
        ring.write(b"hello");
        assert_eq!(ring.len(), 5);
        assert_eq!(ring.snapshot(), b"hello");
    }

    #[test]
    fn multiple_writes() {
        let mut ring = RingBuffer::new(16);
        ring.write(b"hello ");
        ring.write(b"world");
        assert_eq!(ring.len(), 11);
        assert_eq!(ring.snapshot(), b"hello world");
    }

    #[test]
    fn wrap_around() {
        let mut ring = RingBuffer::new(8);
        ring.write(b"abcdefgh"); // fills exactly
        assert_eq!(ring.len(), 8);
        assert_eq!(ring.snapshot(), b"abcdefgh");

        ring.write(b"ij"); // overwrites 'a','b'
        assert_eq!(ring.len(), 8);
        assert_eq!(ring.snapshot(), b"cdefghij");
    }

    #[test]
    fn overwrite_multiple_times() {
        let mut ring = RingBuffer::new(4);
        ring.write(b"abcd");
        ring.write(b"ef");
        assert_eq!(ring.snapshot(), b"cdef");
        ring.write(b"gh");
        assert_eq!(ring.snapshot(), b"efgh");
        ring.write(b"ijkl");
        assert_eq!(ring.snapshot(), b"ijkl");
    }

    #[test]
    fn data_larger_than_capacity() {
        let mut ring = RingBuffer::new(4);
        ring.write(b"abcdefghij"); // 10 bytes into 4-byte buffer
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.snapshot(), b"ghij");
    }

    #[test]
    fn single_byte_capacity() {
        let mut ring = RingBuffer::new(1);
        ring.write(b"a");
        assert_eq!(ring.snapshot(), b"a");
        ring.write(b"b");
        assert_eq!(ring.snapshot(), b"b");
    }

    #[test]
    fn clear() {
        let mut ring = RingBuffer::new(16);
        ring.write(b"data");
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.snapshot(), Vec::<u8>::new());
    }

    #[test]
    fn incremental_wrap() {
        let mut ring = RingBuffer::new(4);
        ring.write(b"ab");
        ring.write(b"cd");
        assert_eq!(ring.snapshot(), b"abcd");
        ring.write(b"e");
        assert_eq!(ring.snapshot(), b"bcde");
        ring.write(b"f");
        assert_eq!(ring.snapshot(), b"cdef");
    }

    #[test]
    fn capacity_returns_correct_value() {
        let ring = RingBuffer::new(1024);
        assert_eq!(ring.capacity(), 1024);
    }

    #[test]
    fn snapshot_after_exact_fill_then_one_more() {
        let mut ring = RingBuffer::new(4);
        ring.write(b"abcd"); // exact fill
        assert_eq!(ring.snapshot(), b"abcd");
        ring.write(b"e"); // one past full
        assert_eq!(ring.snapshot(), b"bcde");
    }
}

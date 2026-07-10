//! Big-endian byte cursor shared by every hand-rolled binary decoder
//! in the workspace (`holofs-wire::wire`, `holofs-model::manifest`).
//!
//! The two decoders used to keep their own private `struct Cursor`
//! copy with only the error message string different — a maintenance
//! trap flagged by review v2 P4.3. Extracting the type here keeps
//! the semantics identical and lets a caller supply a domain tag
//! (`"buffer truncated"` vs `"manifest truncated"`) so error strings
//! still make sense in context.

use std::io;

/// Big-endian read cursor over a borrowed byte slice.
#[derive(Debug)]
pub struct BeCursor<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Prefix embedded in every `UnexpectedEof` returned by [`Self::take`].
    /// Keeps error messages precise across decoders that share this type
    /// (`"buffer truncated"` for the wire decoder, `"manifest truncated"`
    /// for the on-disk manifest decoder).
    truncated_msg: &'static str,
}

impl<'a> BeCursor<'a> {
    /// Wrap a byte slice with a caller-chosen truncation error tag.
    #[must_use]
    pub fn new(buf: &'a [u8], truncated_msg: &'static str) -> Self {
        Self {
            buf,
            pos: 0,
            truncated_msg,
        }
    }

    /// Consume `n` bytes and return them as a borrowed slice. Advances
    /// the cursor by `n` on success; on short-read returns
    /// `UnexpectedEof` with the tag configured in [`Self::new`].
    pub fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                self.truncated_msg,
            ));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Read one byte.
    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Read one BE u16.
    pub fn u16(&mut self) -> io::Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// Read one BE u32.
    pub fn u32(&mut self) -> io::Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read one BE u64.
    pub fn u64(&mut self) -> io::Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    /// Bytes still available past the current cursor position. Feeds
    /// `bounded_cap`-style guards on length-prefixed collections.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
}

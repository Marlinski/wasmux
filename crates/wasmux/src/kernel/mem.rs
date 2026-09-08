//! Checked access to a guest's linear memory.
//!
//! Every syscall argument that is a pointer comes through here. There is exactly one `unsafe`
//! block in the module and one invariant behind it: the backend hands over a base pointer and
//! a length that describe the guest's memory, the guest is not running while the kernel holds
//! them, and a [`Mem`] is never kept across a `memory.grow` (which may move the buffer). Every
//! accessor is bounds-checked and returns [`Errno::FAULT`] rather than panicking, because a
//! panic would abort the consumer's whole component.

use crate::errno::{Errno, VfsResult};

/// A borrowed view of one guest's memory.
pub(crate) struct Mem<'a> {
    bytes: &'a mut [u8],
}

impl<'a> Mem<'a> {
    /// Wrap a backend's `(base, len)`.
    ///
    /// # Safety
    ///
    /// `base` must point at `len` writable bytes that stay valid and unaliased for `'a`.
    pub(crate) unsafe fn new(base: *mut u8, len: usize) -> Mem<'a> {
        // SAFETY: the caller guarantees the region; the guest is suspended, so nothing else
        // reads or writes it while this borrow is alive.
        Mem {
            bytes: unsafe { core::slice::from_raw_parts_mut(base, len) },
        }
    }

    fn slice(&self, at: u32, len: usize) -> VfsResult<&[u8]> {
        let start = at as usize;
        let end = start.checked_add(len).ok_or(Errno::FAULT)?;
        self.bytes.get(start..end).ok_or(Errno::FAULT)
    }

    fn slice_mut(&mut self, at: u32, len: usize) -> VfsResult<&mut [u8]> {
        let start = at as usize;
        let end = start.checked_add(len).ok_or(Errno::FAULT)?;
        self.bytes.get_mut(start..end).ok_or(Errno::FAULT)
    }

    /// Read `len` bytes at `at`.
    pub(crate) fn bytes(&self, at: u32, len: u32) -> VfsResult<&[u8]> {
        self.slice(at, len as usize)
    }

    /// A mutable window of `len` bytes at `at`, for a syscall to fill.
    pub(crate) fn bytes_mut(&mut self, at: u32, len: u32) -> VfsResult<&mut [u8]> {
        self.slice_mut(at, len as usize)
    }

    pub(crate) fn u32(&self, at: u32) -> VfsResult<u32> {
        let b = self.slice(at, 4)?;
        let arr: [u8; 4] = b.try_into().map_err(|_| Errno::FAULT)?;
        Ok(u32::from_le_bytes(arr))
    }

    pub(crate) fn u64(&self, at: u32) -> VfsResult<u64> {
        let b = self.slice(at, 8)?;
        let arr: [u8; 8] = b.try_into().map_err(|_| Errno::FAULT)?;
        Ok(u64::from_le_bytes(arr))
    }

    pub(crate) fn i32_at(&self, at: u32) -> VfsResult<i32> {
        Ok(self.u32(at)? as i32)
    }

    pub(crate) fn put_u8(&mut self, at: u32, v: u8) -> VfsResult<()> {
        let dst = self.slice_mut(at, 1)?;
        match dst.first_mut() {
            Some(slot) => {
                *slot = v;
                Ok(())
            }
            None => Err(Errno::FAULT),
        }
    }

    pub(crate) fn put_u16(&mut self, at: u32, v: u16) -> VfsResult<()> {
        self.put(at, &v.to_le_bytes())
    }

    pub(crate) fn put_u32(&mut self, at: u32, v: u32) -> VfsResult<()> {
        self.put(at, &v.to_le_bytes())
    }

    pub(crate) fn put_u64(&mut self, at: u32, v: u64) -> VfsResult<()> {
        self.put(at, &v.to_le_bytes())
    }

    /// Copy `data` to `at`.
    pub(crate) fn put(&mut self, at: u32, data: &[u8]) -> VfsResult<()> {
        let dst = self.slice_mut(at, data.len())?;
        dst.copy_from_slice(data);
        Ok(())
    }

    /// Zero `len` bytes at `at`.
    pub(crate) fn zero(&mut self, at: u32, len: u32) -> VfsResult<()> {
        self.slice_mut(at, len as usize)?.fill(0);
        Ok(())
    }

    /// A NUL-terminated string at `at`, capped at [`Mem::MAX_STR`].
    /// A NUL-terminated string at `at`, as text.
    ///
    /// Lossy, which is right for a *path*: [`Vfs`](crate::Vfs) addresses files by `&str`, so
    /// a name that is not UTF-8 could not be passed on anyway. It is emphatically wrong for
    /// an argument — see [`Mem::cstr_bytes`].
    pub(crate) fn cstr(&self, at: u32) -> VfsResult<String> {
        Ok(String::from_utf8_lossy(&self.cstr_bytes(at)?).into_owned())
    }

    /// A NUL-terminated string at `at`, byte for byte.
    ///
    /// `argv` and `envp` are *bytes*, not text, and a program is entitled to put anything in
    /// them. BusyBox does: its no-MMU `fork` replacement re-executes `/proc/self/exe` with
    /// the high bit set on the first byte of `argv[0]` as the marker that says "this is the
    /// re-executed copy", and clears it again on the way in.
    ///
    /// Round-tripping that through `from_utf8_lossy` — which is what this used to do —
    /// turned `\xF4imeout` into `\u{FFFD}imeout`, and BusyBox then cleared the high bit of
    /// the replacement character's first byte instead, leaving `o\xBF\xBDimeout` and
    /// "applet not found". `timeout`, `time`, `nohup` and every other applet that re-executes
    /// were broken by it, silently and only on this platform.
    pub(crate) fn cstr_bytes(&self, at: u32) -> VfsResult<Vec<u8>> {
        let start = at as usize;
        let rest = self.bytes.get(start..).ok_or(Errno::FAULT)?;
        let capped = rest.get(..rest.len().min(Self::MAX_STR)).unwrap_or(rest);
        let end = capped
            .iter()
            .position(|&b| b == 0)
            .ok_or(Errno::NAMETOOLONG)?;
        Ok(capped.get(..end).ok_or(Errno::FAULT)?.to_vec())
    }

    /// A NULL-terminated array of string pointers at `at`, as `argv` and `envp` are passed.
    pub(crate) fn cstr_array(&self, at: u32) -> VfsResult<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        if at == 0 {
            return Ok(out);
        }
        let mut p = at;
        loop {
            let item = self.u32(p)?;
            if item == 0 {
                return Ok(out);
            }
            out.push(self.cstr_bytes(item)?);
            if out.len() >= Self::MAX_ARGS {
                return Err(Errno::TOOBIG);
            }
            p = p.checked_add(4).ok_or(Errno::FAULT)?;
        }
    }

    /// A `timespec` in the time64 layout: 64-bit seconds, 32-bit nanoseconds, 32-bit padding.
    pub(crate) fn timespec(&self, at: u32) -> VfsResult<core::time::Duration> {
        let secs = self.u64(at)? as i64;
        let nanos = self.u32(at.checked_add(8).ok_or(Errno::FAULT)?)?;
        if secs < 0 || nanos >= 1_000_000_000 {
            return Err(Errno::INVAL);
        }
        Ok(core::time::Duration::new(secs as u64, nanos))
    }

    /// Write a `timespec`.
    pub(crate) fn put_timespec(&mut self, at: u32, d: core::time::Duration) -> VfsResult<()> {
        self.put_u64(at, d.as_secs())?;
        self.put_u32(at.checked_add(8).ok_or(Errno::FAULT)?, d.subsec_nanos())?;
        self.put_u32(at.checked_add(12).ok_or(Errno::FAULT)?, 0)
    }

    /// Longest path or string the kernel will read from a guest.
    pub(crate) const MAX_STR: usize = 4096;
    /// Most entries in one `argv` or `envp`.
    pub(crate) const MAX_ARGS: usize = 4096;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with<R>(bytes: &mut [u8], f: impl FnOnce(Mem<'_>) -> R) -> R {
        let len = bytes.len();
        // SAFETY: `bytes` outlives the borrow and nothing else touches it.
        f(unsafe { Mem::new(bytes.as_mut_ptr(), len) })
    }

    #[test]
    fn out_of_bounds_is_a_fault_not_a_panic() {
        let mut buf = [0u8; 16];
        with(&mut buf, |mut m| {
            assert_eq!(m.u32(13), Err(Errno::FAULT), "straddling the end");
            assert_eq!(
                m.u32(u32::MAX),
                Err(Errno::FAULT),
                "overflowing the addition"
            );
            assert_eq!(m.bytes(8, 9), Err(Errno::FAULT));
            assert_eq!(m.put(15, b"ab"), Err(Errno::FAULT));
            assert!(m.u32(12).is_ok(), "the last aligned word is readable");
        });
    }

    #[test]
    fn strings_are_capped_and_unterminated_ones_rejected() {
        let mut buf = vec![b'a'; 8];
        with(&mut buf, |m| {
            assert_eq!(m.cstr(0).err(), Some(Errno::NAMETOOLONG), "no NUL in range");
        });
        let mut buf = b"hi\0there\0".to_vec();
        with(&mut buf, |m| {
            assert_eq!(m.cstr(0).ok().as_deref(), Some("hi"));
            assert_eq!(m.cstr(3).ok().as_deref(), Some("there"));
            assert_eq!(m.cstr(9).err(), Some(Errno::NAMETOOLONG));
        });
    }

    #[test]
    fn round_trips() {
        let mut buf = [0u8; 32];
        with(&mut buf, |mut m| {
            m.put_u64(0, 0x1122_3344_5566_7788).ok();
            assert_eq!(m.u64(0), Ok(0x1122_3344_5566_7788));
            m.put_timespec(8, core::time::Duration::new(7, 500)).ok();
            assert_eq!(m.timespec(8), Ok(core::time::Duration::new(7, 500)));
            m.zero(8, 16).ok();
            assert_eq!(m.u64(8), Ok(0));
        });
    }
}

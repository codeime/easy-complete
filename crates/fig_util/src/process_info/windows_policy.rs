//! Bounds for `QueryFullProcessImageNameA`.
//!
//! `windows.rs` stays `cfg(windows)` and talks to the Win32 API. The API
//! writes `len` path bytes (no NUL) into a `MAX_PATH+1` buffer; slicing
//! `0..=len` panics if that reported length is at or past the buffer end.
//! This helper compiles on every OS so Linux CI pins the bound.

#![allow(dead_code)]

/// Bytes that `CStr::from_bytes_with_nul` may read: the path plus the
/// trailing NUL at `buf[reported_len]`. `None` if the reported length
/// would index past the buffer.
pub fn win32_process_image_nul_terminated(buf: &[u8], reported_len: u32) -> Option<&[u8]> {
    let len = usize::try_from(reported_len).ok()?;
    buf.get(0..=len)
}

#[cfg(test)]
mod tests {
    use super::win32_process_image_nul_terminated;

    #[test]
    fn overflowing_reported_len_does_not_panic() {
        let buf = [0u8; 8];
        assert!(win32_process_image_nul_terminated(&buf, 8).is_none());
        assert_eq!(win32_process_image_nul_terminated(&buf, 7), Some(&buf[0..=7]));
        assert!(win32_process_image_nul_terminated(&buf, 0).is_some());
        assert!(win32_process_image_nul_terminated(&buf, u32::MAX).is_none());
    }

    #[test]
    fn windows_exe_lookup_uses_the_shared_bound() {
        let src = include_str!("windows.rs");
        assert!(
            src.contains("win32_process_image_nul_terminated"),
            "QueryFullProcessImageNameA must slice through the shared bound"
        );
        assert!(
            !src.contains("[0..=len as usize]") && !src.contains("u8::try_from('\\0')"),
            "do not index the image-name buffer with the raw API length"
        );
    }
}

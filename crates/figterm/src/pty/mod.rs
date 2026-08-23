use std::io::{self, ErrorKind};

use anyhow::Result;
use async_trait::async_trait;
use portable_pty::{Child, PtySize};
pub mod cmdbuilder;
pub use cmdbuilder::CommandBuilder;

#[cfg(unix)]
pub mod unix;

#[cfg(windows)]
pub mod win;

#[async_trait]
pub trait AsyncMasterPty {
    async fn read(&mut self, buff: &mut [u8]) -> io::Result<usize>;
    async fn write(&mut self, buff: &[u8]) -> io::Result<usize>;
    fn resize(&self, size: PtySize) -> Result<()>;
}

#[async_trait]
pub trait AsyncMasterPtyExt: AsyncMasterPty {
    async fn write_all(&mut self, mut buff: &[u8]) -> io::Result<()> {
        while !buff.is_empty() {
            match self.write(buff).await {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ));
                },
                Ok(n) => buff = &buff[n..],
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {},
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl<T: AsyncMasterPty + ?Sized> AsyncMasterPtyExt for T {}

/// Win32 `BOOL`: non-zero is success. `TerminateProcess` used to invert this
/// (`if res != 0 { Err }`), so a successful kill was reported as failure.
/// Compiled in tests on every OS so Linux CI pins the mapping.
#[cfg(any(test, windows))]
pub(crate) fn win32_bool_succeeded(res: i32) -> bool {
    res != 0
}

/// Win32 `HRESULT`: zero (`S_OK`) is success. Opposite of `BOOL`.
/// `CreatePseudoConsole` / `ResizePseudoConsole` return this — do not feed
/// them to [`win32_bool_succeeded`]. Live ConPTY I/O still needs Windows.
#[cfg(any(test, windows))]
pub(crate) fn win32_hresult_succeeded(hr: i32) -> bool {
    hr == 0
}

#[cfg(test)]
mod win32_bool_tests {
    use super::{win32_bool_succeeded, win32_hresult_succeeded};

    #[test]
    fn terminateprocess_success_is_nonzero() {
        assert!(win32_bool_succeeded(1));
        assert!(!win32_bool_succeeded(0));
    }

    #[test]
    fn conpty_hresult_success_is_zero_the_opposite_of_bool() {
        assert!(win32_hresult_succeeded(0));
        assert!(!win32_hresult_succeeded(1));
        assert_ne!(win32_hresult_succeeded(0), win32_bool_succeeded(0));
        assert_ne!(win32_hresult_succeeded(1), win32_bool_succeeded(1));
        // S_FALSE is 1, E_FAIL is 0x80004005. Neither is CreatePseudoConsole success.
        const S_FALSE: i32 = 1;
        const E_FAIL: i32 = 0x8000_4005_u32 as i32;
        assert!(!win32_hresult_succeeded(S_FALSE));
        assert!(!win32_hresult_succeeded(E_FAIL));
        assert!(win32_bool_succeeded(S_FALSE), "BOOL would misread S_FALSE as success");
    }
}

pub trait MasterPty {
    fn get_async_master_pty(self: Box<Self>) -> Result<Box<dyn AsyncMasterPty + Send + Sync>>;
}

pub trait SlavePty {
    fn spawn_command(&self, builder: CommandBuilder) -> Result<Box<dyn Child + Send + Sync>>;
    fn get_name(&self) -> Option<String>;
}

pub struct PtyPair {
    // slave is listed first so that it is dropped first.
    // The drop order is stable and specified by rust rfc 1857
    pub slave: Box<dyn SlavePty + Send>,
    pub master: Box<dyn MasterPty + Send>,
}

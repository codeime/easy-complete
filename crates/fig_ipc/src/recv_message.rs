use std::io;

use async_trait::async_trait;
use fig_proto::prost::Message;
use fig_proto::{FigMessage, ReflectMessage};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::BufferedReader;
use crate::error::RecvError;

#[async_trait]
pub trait RecvMessage {
    async fn recv_message<R>(&mut self) -> Result<Option<R>, RecvError>
    where
        R: Message + ReflectMessage + Default;
}

#[async_trait]
impl<T> RecvMessage for BufferedReader<T>
where
    T: AsyncRead + Unpin + Send,
{
    async fn recv_message<M>(&mut self) -> Result<Option<M>, RecvError>
    where
        M: Message + ReflectMessage + Default,
    {
        loop {
            // Split a complete frame off the BytesMut. Incomplete leaves the
            // buffer in place so the next read can append; a header error
            // advances 10 bytes the same way the old Cursor parse did.
            match FigMessage::take_from_bytes_mut(&mut self.buffer) {
                Ok(message) => return Ok(Some(message.decode()?)),
                Err(fig_proto::FigMessageParseError::Incomplete(_, _)) => {
                    let bytes = self.inner.read_buf(&mut self.buffer).await?;

                    // If the buffer is empty, we've reached EOF
                    if bytes == 0 {
                        if self.buffer.is_empty() {
                            return Ok(None);
                        } else {
                            return Err(RecvError::Io(io::Error::from(io::ErrorKind::UnexpectedEof)));
                        }
                    }
                },
                // On any other error, return the error.
                //
                // Resync-to-`\x1b@` is intentionally not implemented. Parse
                // already consumed up to 10 bytes (header + type) before this
                // error; callers (desktop local_ipc, figterm ipc, remote
                // handshake) drop the stream on RecvError, so leftover bytes
                // die with the socket. Scanning for the next magic would keep a
                // desynced connection alive and can false-match payload bytes
                // (`\x1b@` is legal in protobuf). See CROSS_PLATFORM_PLAN §8.
                Err(err) => return Err(err.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::SendMessage;

    fn mock(initial: Vec<u8>) -> BufferedReader<Cursor<Vec<u8>>> {
        let size = initial.len();
        let mut inner = Cursor::new(initial);
        inner.set_position(size as u64);
        BufferedReader::new(inner)
    }

    fn test_message_small() -> fig_proto::local::LocalMessage {
        fig_proto::hooks::hook_to_message(fig_proto::hooks::new_hide_hook())
    }

    fn test_message_large() -> fig_proto::local::LocalMessage {
        fig_proto::hooks::hook_to_message(fig_proto::hooks::new_edit_buffer_hook(
            None,
            "A".repeat(10000),
            0,
            0,
            None,
        ))
    }

    #[tokio::test]
    async fn single_message_small() {
        let mut mock = mock(vec![]);
        mock.send_message(test_message_small()).await.unwrap();
        mock.inner.set_position(0);
        assert_eq!(mock.recv_message().await.unwrap(), Some(test_message_small()));
    }

    #[tokio::test]
    async fn single_message_large() {
        let mut mock = mock(vec![]);
        mock.send_message(test_message_large()).await.unwrap();
        mock.inner.set_position(0);
        assert_eq!(mock.recv_message().await.unwrap(), Some(test_message_large()));
    }

    #[tokio::test]
    async fn mutlti_message_small() {
        let mut mock = mock(vec![]);
        for _ in 0..500 {
            mock.send_message(test_message_small()).await.unwrap();
        }
        mock.inner.set_position(0);
        for _ in 0..500 {
            assert_eq!(mock.recv_message().await.unwrap(), Some(test_message_small()));
        }
        assert_eq!(mock.read(&mut [0u8]).await.unwrap(), 0);
        assert_eq!(mock.buffer.len(), 0);
    }

    #[tokio::test]
    async fn mutlti_message_large() {
        let mut mock = mock(vec![]);
        for _ in 0..500 {
            mock.send_message(test_message_large()).await.unwrap();
        }
        mock.inner.set_position(0);
        for _ in 0..500 {
            assert_eq!(mock.recv_message().await.unwrap(), Some(test_message_large()));
        }
        assert_eq!(mock.read(&mut [0u8]).await.unwrap(), 0);
        assert_eq!(mock.buffer.len(), 0);
    }

    #[tokio::test]
    async fn invalid_header() {
        let mut mock = mock(vec![b'f', b'o', b'o']);
        mock.inner.set_position(0);
        assert!(mock.recv_message::<fig_proto::local::LocalMessage>().await.is_err());
    }

    /// Ten garbage bytes that are not `\x1b@` hit `InvalidHeader` (parser needs
    /// 10 bytes before it can reject the header). The recv loop advances those
    /// 10 bytes and returns Err. Callers drop the socket; they do not retry.
    #[tokio::test]
    async fn invalid_header_advances_ten_bytes_and_errors() {
        let mut writer = mock(vec![]);
        writer.send_message(test_message_small()).await.unwrap();
        let framed = writer.inner.into_inner();
        let mut bytes = vec![b'x'; 10];
        bytes.extend_from_slice(&framed);
        let mut reader = mock(bytes);
        reader.inner.set_position(0);
        assert!(reader.recv_message::<fig_proto::local::LocalMessage>().await.is_err());
        // A *retry* on this same buffer would see an aligned frame (garbage was
        // exactly one header's worth). Production callers do not retry.
        assert_eq!(
            reader.recv_message::<fig_proto::local::LocalMessage>().await.unwrap(),
            Some(test_message_small())
        );
    }

    /// A one-byte prefix shifts the real `\x1b@` so the parser consumes 10
    /// bytes of mixed garbage+payload and returns Err. A second recv still
    /// fails: there is no scan-for-magic. This is the desync the TODO named;
    /// fixing it would change "drop the socket" into "keep reading", which is
    /// not clearly compatible (magic can appear in protobuf bodies).
    #[test]
    fn recv_splits_frames_from_the_bytesmut() {
        let src = include_str!("recv_message.rs");
        let start = src
            .find("FigMessage::take_from_bytes_mut")
            .expect("take_from_bytes_mut call");
        let body = &src[start..];
        let end = body.find("Err(err) => return Err(err.into())").expect("error arm");
        let body = &src[..start + end];
        assert!(
            body.contains("take_from_bytes_mut"),
            "recv must split a complete frame off BytesMut instead of copying the body"
        );
        let cursor = ["Cursor", "new"].join("::");
        assert!(
            !body.contains(&cursor) && !body.contains("buffer.advance"),
            "recv must not parse through a Cursor and then advance the original buffer"
        );
    }

    #[tokio::test]
    async fn one_byte_desync_does_not_resync_to_next_frame() {
        let mut writer = mock(vec![]);
        writer.send_message(test_message_small()).await.unwrap();
        let framed = writer.inner.into_inner();
        let mut bytes = vec![0x00];
        bytes.extend_from_slice(&framed);
        let mut reader = mock(bytes);
        reader.inner.set_position(0);
        assert!(reader.recv_message::<fig_proto::local::LocalMessage>().await.is_err());
        assert!(reader.recv_message::<fig_proto::local::LocalMessage>().await.is_err());
    }
}

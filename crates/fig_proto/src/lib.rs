//! Protocol buffer definitions

pub mod fig;
pub mod fig_common;
pub mod figterm;
pub mod hooks;
pub mod local;
pub mod mux;
pub(crate) mod proto;
pub mod remote_hooks;
pub mod util;
use std::fmt::Debug;
use std::mem::size_of;
use std::num::TryFromIntError;
use std::sync::LazyLock;

use bytes::{Buf, Bytes, BytesMut};
pub use prost;
use prost::{DecodeError, Message};
use prost_reflect::DescriptorPool;
pub use prost_reflect::{DynamicMessage, ReflectMessage};
use serde::Serialize;
use thiserror::Error;

pub mod remote {
    pub use crate::proto::remote::*;
}

// This is not used explicitly, but it must be here for the derive
// impls on the protos for dynamic message
static DESCRIPTOR_POOL: LazyLock<DescriptorPool> = LazyLock::new(|| {
    DescriptorPool::decode(include_bytes!(concat!(env!("OUT_DIR"), "/file_descriptor_set.bin")).as_ref()).unwrap()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FigMessageType {
    Protobuf,
    Json,
    MessagePack,
}

impl FigMessageType {
    pub const fn header(&self) -> &'static [u8] {
        match self {
            FigMessageType::Protobuf => b"fig-pbuf",
            FigMessageType::Json => b"fig-json",
            FigMessageType::MessagePack => b"fig-mpak",
        }
    }
}

/// A fig message
///
/// The format of a fig message is:
///
///   - The header `\x1b@`
///   - The type of the message (must be 8 bytes)
///     - `fig-pbuf` - Protocol Buffer
///     - `fig-json` - Json
///     - `fig-mpak` - MessagePack
///   - The length of the remainder of the message encoded as a big endian u64
///   - The message, encoded as protobuf, json-protobuf, or messagepack-protobuf
#[derive(Debug, Clone)]
pub struct FigMessage {
    pub inner: Bytes,
    pub message_type: FigMessageType,
}

#[derive(Debug)]
pub enum FigMessageComponent {
    Header,
    BodySize,
    Body,
}

#[derive(Debug, Error)]
pub enum FigMessageParseError {
    /// The missing component and the needed bytes
    #[error("incomplete message, missing {0:?}")]
    Incomplete(FigMessageComponent, usize),
    #[error("invalid message header {0} (raw type {1})")]
    InvalidHeader(String, String),
    #[error("invalid message type")]
    InvalidMessageType([u8; 8]),
    #[error("failed to convert int: {0}")]
    TryFromInt(#[from] TryFromIntError),
    /// A frame whose body length plus the 18-byte prefix does not fit in
    /// `usize`. Distinct from [`Self::Incomplete`]: waiting cannot complete
    /// this frame, so parse/take consume the prefix the same way as
    /// [`Self::TryFromInt`].
    #[error("frame body length overflows addressable size")]
    BodyLengthOverflow,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum FigMessageDecodeError {
    #[error("name is not a valid protobuf: {0}")]
    NameNotValid(String),
    #[error(transparent)]
    ProstDecode(#[from] DecodeError),
    #[error(transparent)]
    JsonDecode(#[from] serde_json::Error),
    #[error(transparent)]
    RmpDecode(#[from] rmp_serde::decode::Error),
}

#[derive(Debug, Error)]
pub enum FigMessageEncodeError {
    #[error(transparent)]
    IntError(#[from] std::num::TryFromIntError),
    #[error(transparent)]
    JsonEncode(#[from] serde_json::Error),
    #[error(transparent)]
    RmpEncode(#[from] rmp_serde::encode::Error),
    #[error(transparent)]
    IoError(#[from] std::io::Error),
}

impl FigMessage {
    pub fn json(json: impl Serialize) -> Result<Bytes, FigMessageEncodeError> {
        FigMessage::encode(FigMessageType::Json, serde_json::to_vec(&json)?.into())
    }

    pub fn message_pack(message_pack: impl Serialize) -> Result<Bytes, FigMessageEncodeError> {
        FigMessage::encode(FigMessageType::MessagePack, rmp_serde::to_vec(&message_pack)?.into())
    }

    pub fn encode_buf(&self, dst: &mut BytesMut) -> Result<(), FigMessageEncodeError> {
        let body = &self.inner;
        let message_type = self.message_type;

        let message_len: u64 = body.len().try_into()?;
        let message_len_be = message_len.to_be_bytes();

        let prefix = b"\x1b@".len() + message_type.header().len() + message_len_be.len();
        let Some(cap) = prefix.checked_add(body.len()) else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame length overflows").into());
        };
        dst.reserve(cap);
        dst.extend_from_slice(b"\x1b@");
        dst.extend_from_slice(message_type.header());
        dst.extend_from_slice(&message_len_be);
        dst.extend_from_slice(body);

        Ok(())
    }

    pub fn to_encoded(&self) -> Result<Bytes, FigMessageEncodeError> {
        let mut inner: BytesMut = BytesMut::new();
        self.encode_buf(&mut inner)?;
        Ok(inner.freeze())
    }

    pub fn encode(message_type: FigMessageType, body: Bytes) -> Result<Bytes, FigMessageEncodeError> {
        let msg = Self {
            inner: body,
            message_type,
        };
        msg.to_encoded()
    }

    /// Magic + type tag + body length. Body bytes follow.
    pub const FRAME_PREFIX_LEN: usize = 2 + 8 + size_of::<u64>();

    /// Inspect a contiguous prefix without consuming. Incomplete errors leave
    /// the caller's buffer untouched; header errors still consume 10 bytes at
    /// the parse/take site, matching the old Cursor behavior.
    fn inspect_frame(bytes: &[u8]) -> Result<(usize, FigMessageType), FigMessageParseError> {
        if bytes.len() < 10 {
            return Err(FigMessageParseError::Incomplete(
                FigMessageComponent::Header,
                10 - bytes.len(),
            ));
        }
        if bytes[0] != b'\x1b' || bytes[1] != b'@' {
            let header = [bytes[0], bytes[1]];
            let mut message_type_buf = [0; 8];
            message_type_buf.copy_from_slice(&bytes[2..10]);
            return Err(FigMessageParseError::InvalidHeader(
                hex::encode(header),
                hex::encode(message_type_buf),
            ));
        }
        let mut message_type_buf = [0; 8];
        message_type_buf.copy_from_slice(&bytes[2..10]);
        let message_type = match &message_type_buf {
            b"fig-pbuf" => FigMessageType::Protobuf,
            b"fig-json" => FigMessageType::Json,
            b"fig-mpak" => FigMessageType::MessagePack,
            _ => return Err(FigMessageParseError::InvalidMessageType(message_type_buf)),
        };
        if bytes.len() < Self::FRAME_PREFIX_LEN {
            return Err(FigMessageParseError::Incomplete(
                FigMessageComponent::BodySize,
                Self::FRAME_PREFIX_LEN - bytes.len(),
            ));
        }
        let Some(len_slice) = bytes.get(10..Self::FRAME_PREFIX_LEN) else {
            return Err(FigMessageParseError::Incomplete(
                FigMessageComponent::BodySize,
                Self::FRAME_PREFIX_LEN.saturating_sub(bytes.len()),
            ));
        };
        let Ok(len_bytes) = <[u8; 8]>::try_from(len_slice) else {
            return Err(FigMessageParseError::BodyLengthOverflow);
        };
        let body_len: usize = u64::from_be_bytes(len_bytes).try_into()?;
        let Some(total) = Self::FRAME_PREFIX_LEN.checked_add(body_len) else {
            return Err(FigMessageParseError::BodyLengthOverflow);
        };
        if bytes.len() < total {
            return Err(FigMessageParseError::Incomplete(
                FigMessageComponent::Body,
                total - bytes.len(),
            ));
        }
        Ok((total, message_type))
    }

    pub fn parse(src: &mut impl bytes::Buf) -> Result<(usize, FigMessage), FigMessageParseError> {
        match Self::inspect_frame(src.chunk()) {
            Ok((total, message_type)) => {
                src.advance(Self::FRAME_PREFIX_LEN);
                let inner = src.copy_to_bytes(total.saturating_sub(Self::FRAME_PREFIX_LEN));
                Ok((total, FigMessage { inner, message_type }))
            },
            Err(err @ FigMessageParseError::Incomplete(_, _)) => Err(err),
            Err(err @ (FigMessageParseError::InvalidHeader(_, _) | FigMessageParseError::InvalidMessageType(_))) => {
                src.advance(10.min(src.remaining()));
                Err(err)
            },
            Err(err @ (FigMessageParseError::TryFromInt(_) | FigMessageParseError::BodyLengthOverflow)) => {
                src.advance(Self::FRAME_PREFIX_LEN.min(src.remaining()));
                Err(err)
            },
            Err(err) => Err(err),
        }
    }

    /// Split a complete frame off `buf`. Incomplete leaves `buf` untouched.
    /// Header errors advance 10 bytes, same as [`Self::parse`].
    pub fn take_from_bytes_mut(buf: &mut BytesMut) -> Result<Self, FigMessageParseError> {
        match Self::inspect_frame(buf) {
            Ok((total, message_type)) => {
                let mut frame = buf.split_to(total);
                let _ = frame.split_to(Self::FRAME_PREFIX_LEN);
                Ok(Self {
                    inner: frame.freeze(),
                    message_type,
                })
            },
            Err(err @ FigMessageParseError::Incomplete(_, _)) => Err(err),
            Err(err @ (FigMessageParseError::InvalidHeader(_, _) | FigMessageParseError::InvalidMessageType(_))) => {
                let skip = 10.min(buf.len());
                let _ = buf.split_to(skip);
                Err(err)
            },
            Err(err @ (FigMessageParseError::TryFromInt(_) | FigMessageParseError::BodyLengthOverflow)) => {
                let skip = Self::FRAME_PREFIX_LEN.min(buf.len());
                let _ = buf.split_to(skip);
                Err(err)
            },
            Err(err) => Err(err),
        }
    }

    pub fn decode<T>(self) -> Result<T, FigMessageDecodeError>
    where
        T: Message + ReflectMessage + Default,
    {
        match self.message_type {
            FigMessageType::Protobuf => Ok(T::decode(self.inner)?),
            FigMessageType::Json => Ok(DynamicMessage::deserialize(
                T::default().descriptor(),
                &mut serde_json::Deserializer::from_slice(self.inner.as_ref()),
            )?
            .transcode_to()?),
            FigMessageType::MessagePack => Ok(DynamicMessage::deserialize(
                T::default().descriptor(),
                &mut rmp_serde::Deserializer::from_read_ref(self.inner.as_ref()),
            )?
            .transcode_to()?),
        }
    }
}

impl std::ops::Deref for FigMessage {
    type Target = Bytes;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// A trait for types that can be converted to a FigProtobuf
pub trait FigProtobufEncodable: Debug + Send + Sync {
    /// Encodes a protobuf message into a fig message
    fn encode_fig_protobuf(&self) -> Result<Bytes, FigMessageEncodeError>;
}

impl<T: Message + Debug> FigProtobufEncodable for T {
    fn encode_fig_protobuf(&self) -> Result<Bytes, FigMessageEncodeError> {
        // One buffer: header + length + protobuf. Framing used to encode the
        // body separately, wrap it, then copy it into the framed buffer.
        let body_len = self.encoded_len();
        let message_len: u64 = body_len.try_into()?;
        let Some(cap) = FigMessage::FRAME_PREFIX_LEN.checked_add(body_len) else {
            return Err(
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "protobuf frame length overflows").into(),
            );
        };
        let mut buf = BytesMut::with_capacity(cap);
        buf.extend_from_slice(b"\x1b@");
        buf.extend_from_slice(FigMessageType::Protobuf.header());
        buf.extend_from_slice(&message_len.to_be_bytes());
        self.encode(&mut buf)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::WriteZero, err))?;
        debug_assert_eq!(buf.len(), cap);
        Ok(buf.freeze())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn test_message() -> local::LocalMessage {
        let ctx = local::ShellContext {
            pid: Some(123),
            ttys: Some("/dev/pty123".into()),
            process_name: Some("/bin/bash".into()),
            current_working_directory: Some("/home/user".into()),
            session_id: None,
            terminal: None,
            hostname: None,
            shell_path: Some("/bin/bash".into()),
            wsl_distro: None,
            environment_variables: vec![],
            qterm_version: None,
            preexec: Some(false),
            osc_lock: Some(true),
            alias: Some("alias abc='abc d'\n".into()),
        };
        let hook = hooks::new_edit_buffer_hook(Some(ctx), "test", 2, 3, None);
        hooks::hook_to_message(hook)
    }

    #[test]
    fn test_to_fig_pbuf() {
        let message = test_message();
        let framed = message.encode_fig_protobuf().unwrap();
        assert_eq!(&framed[..10], b"\x1b@fig-pbuf");
        let legacy = FigMessage::encode(FigMessageType::Protobuf, message.encode_to_vec().into()).unwrap();
        assert_eq!(framed, legacy, "direct encode must match header-plus-body framing");
    }

    #[test]
    fn protobuf_encode_writes_the_body_once() {
        let src = include_str!("lib.rs");
        let start = src.find("let body_len = self.encoded_len()").expect("encoded_len");
        let body = &src[start..];
        let end = body.find("Ok(buf.freeze())").expect("freeze");
        let body = &body[..end];
        assert!(
            body.contains("self.encode(&mut buf)"),
            "protobuf frames must encode into the framed buffer"
        );
        let encode_to_vec = ["encode", "to_vec"].join("_");
        assert!(
            !body.contains(&encode_to_vec) && !body.contains("to_vec"),
            "protobuf frames must not copy the body through an intermediate vec"
        );
    }

    #[test]
    fn encode_reuses_body_bytes_without_to_vec() {
        let src = include_str!("lib.rs");
        let start = src.find("pub fn encode(").expect("encode");
        let body = &src[start..];
        let end = body.find("\n    pub const FRAME_PREFIX_LEN").expect("FRAME_PREFIX_LEN");
        let body = &body[..end];
        assert!(
            !body.contains("to_vec"),
            "FigMessage::encode must frame the caller's Bytes, not copy them"
        );
    }

    #[test]
    fn incomplete_header_does_not_consume() {
        let mut buf: &[u8] = b"\x1b@fig";
        let err = FigMessage::parse(&mut buf).expect_err("short header");
        assert!(matches!(
            err,
            FigMessageParseError::Incomplete(FigMessageComponent::Header, _)
        ));
        assert_eq!(buf, b"\x1b@fig");
    }

    #[test]
    fn take_from_bytes_mut_splits_a_complete_frame() {
        let framed = test_message().encode_fig_protobuf().unwrap();
        let mut buf = BytesMut::from(framed.as_ref());
        let message = FigMessage::take_from_bytes_mut(&mut buf).expect("complete frame");
        assert!(buf.is_empty());
        let decoded: local::LocalMessage = message.decode().unwrap();
        assert_eq!(decoded, test_message());
    }

    #[test]
    fn take_from_bytes_mut_leaves_incomplete_bytes() {
        let framed = test_message().encode_fig_protobuf().unwrap();
        let mut buf = BytesMut::from(&framed[..10]);
        let err = FigMessage::take_from_bytes_mut(&mut buf).expect_err("incomplete");
        assert!(matches!(
            err,
            FigMessageParseError::Incomplete(FigMessageComponent::BodySize, _)
        ));
        assert_eq!(&buf[..], &framed[..10]);
    }

    #[test]
    fn json_round_trip() {
        let message = test_message();
        let json = serde_json::to_vec(&message.transcode_to_dynamic()).unwrap();

        let msg = FigMessage {
            inner: Bytes::from(json),
            message_type: FigMessageType::Json,
        };

        assert_eq!(&msg.to_encoded().unwrap()[..10], b"\x1b@fig-json");

        let decoded_message: local::LocalMessage = msg.decode().unwrap();

        assert_eq!(message, decoded_message);
    }

    #[test]
    fn json_decode() {
        let msg = FigMessage {
            inner: Bytes::from(
                serde_json::to_vec(&json!({
                    "hook": {
                        "caretPosition": {
                          "x": 123.0,
                          "y": 456,
                          "width": 34.0,
                          "height": 61
                        }
                    }
                }))
                .unwrap(),
            ),
            message_type: FigMessageType::Json,
        };

        let decoded_message: local::LocalMessage = msg.decode().unwrap();

        let hook = match decoded_message.r#type.unwrap() {
            local::local_message::Type::Hook(hook) => hook,
            local::local_message::Type::Command(_) => panic!(),
        };

        let caret_position = match hook.hook.unwrap() {
            local::hook::Hook::CaretPosition(caret_position) => caret_position,
            _ => panic!(),
        };

        assert_eq!(caret_position.x, 123.0);
        assert_eq!(caret_position.y, 456.0);
        assert_eq!(caret_position.width, 34.0);
        assert_eq!(caret_position.height, 61.0);
    }

    #[test]
    fn overflowing_body_length_does_not_panic() {
        let mut header = [0u8; FigMessage::FRAME_PREFIX_LEN];
        header[..10].copy_from_slice(b"\x1b@fig-pbuf");
        header[10..].copy_from_slice(&u64::MAX.to_be_bytes());
        let mut buf = BytesMut::from(header.as_slice());
        let err = FigMessage::take_from_bytes_mut(&mut buf).expect_err("unaddressable body");
        assert!(
            matches!(
                err,
                FigMessageParseError::BodyLengthOverflow | FigMessageParseError::TryFromInt(_)
            ),
            "overflow must not stall as Incomplete: {err:?}"
        );
        assert!(
            buf.is_empty(),
            "the 18-byte prefix must be consumed so the socket is dropped, not waited on"
        );

        let mut slice: &[u8] = &header;
        let err = FigMessage::parse(&mut slice).expect_err("unaddressable body");
        assert!(matches!(
            err,
            FigMessageParseError::BodyLengthOverflow | FigMessageParseError::TryFromInt(_)
        ));
        assert!(slice.is_empty(), "parse must consume the prefix on overflow");
    }

    #[test]
    fn inspect_frame_does_not_unwrap_the_length_field() {
        let src = include_str!("lib.rs");
        let start = src.find("fn inspect_frame(").expect("inspect_frame");
        let body = &src[start..];
        let end = body.find("\n    pub fn parse(").expect("parse");
        let body = &body[..end];
        assert!(
            !body.contains(".expect(") && !body.contains(".unwrap()"),
            "length-field conversion must return a parse error, not panic"
        );
        assert!(
            body.contains("checked_add(body_len)") && body.contains("BodyLengthOverflow"),
            "prefix + body must not wrap usize"
        );
    }

    #[test]
    fn rmp_round_trip() {
        let message = test_message();
        let mpack = rmp_serde::to_vec(&message.transcode_to_dynamic()).unwrap();

        let msg = FigMessage {
            inner: Bytes::from(mpack),
            message_type: FigMessageType::MessagePack,
        };

        assert_eq!(&msg.to_encoded().unwrap()[..10], b"\x1b@fig-mpak");

        let decoded_message: local::LocalMessage = msg.decode().unwrap();

        assert_eq!(message, decoded_message);
    }
}

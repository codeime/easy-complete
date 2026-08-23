//! The one message this process ever writes: a caret position hook wrapped in a
//! `LocalMessage`, framed the way `fig_ipc` frames it.
//!
//! Hand-encoded so the binary does not link prost and its descriptor pool for a
//! payload that is always under fifty bytes. [`tests::frame_matches_fig_proto`]
//! diffs the bytes against the generated encoder.
//!
//! Caret-rect usability and coalesce policy also live here so Linux CI pins
//! the IMK gates without AppKit. `imk.rs` is still `cfg(macos)`.

/// `\x1b@` plus the eight-byte type tag, per `fig_proto::FigMessage`.
const FIG_PBUF_PREFIX: &[u8] = b"\x1b@fig-pbuf";

/// `LocalMessage.hook`, from `proto/local.proto`.
const LOCAL_MESSAGE_HOOK_FIELD: u32 = 3;
/// `Hook.caret_position`, from `proto/local.proto`.
const HOOK_CARET_POSITION_FIELD: u32 = 113;

const WIRE_TYPE_VARINT: u32 = 0;
const WIRE_TYPE_FIXED64: u32 = 1;
const WIRE_TYPE_LEN: u32 = 2;

/// `CaretPositionHook.Origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    #[allow(dead_code)] // The IMK caret rect is always bottom-left.
    TopLeft = 0,
    BottomLeft = 1,
}

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn push_tag(out: &mut Vec<u8>, field: u32, wire_type: u32) {
    push_varint(out, u64::from(field << 3 | wire_type));
}

/// Skips the field at its default, which is what proto3 and prost both do; the
/// desktop decodes an absent `double` as `0.0` either way.
fn push_double(out: &mut Vec<u8>, field: u32, value: f64) {
    if value == 0.0 {
        return;
    }
    push_tag(out, field, WIRE_TYPE_FIXED64);
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_message(out: &mut Vec<u8>, field: u32, body: &[u8]) {
    push_tag(out, field, WIRE_TYPE_LEN);
    push_varint(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn caret_position_hook(x: f64, y: f64, width: f64, height: f64, origin: Origin) -> Vec<u8> {
    let mut out = Vec::with_capacity(38);
    push_double(&mut out, 1, x);
    push_double(&mut out, 2, y);
    push_double(&mut out, 3, width);
    push_double(&mut out, 4, height);
    // `optional Origin origin = 5`, so it is written even when it is zero.
    push_tag(&mut out, 5, WIRE_TYPE_VARINT);
    push_varint(&mut out, origin as u64);
    out
}

/// A remote IMK proxy may return a zeroed rect when the client is gone. Sending
/// that on places the overlay at the screen-space origin. Height ≤ 0 is the
/// same class of unusable box IBus / Win32 already drop.
pub fn caret_rect_is_usable(x: f64, y: f64, width: f64, height: f64) -> bool {
    x.is_finite() && y.is_finite() && width.is_finite() && height.is_finite() && height > 0.0
}

pub const CARET_EPS: f64 = 0.5;

pub type CaretRect = (f64, f64, f64, f64);

pub fn caret_rects_close(left: CaretRect, right: CaretRect) -> bool {
    (left.0 - right.0).abs() < CARET_EPS
        && (left.1 - right.1).abs() < CARET_EPS
        && (left.2 - right.2).abs() < CARET_EPS
        && (left.3 - right.3).abs() < CARET_EPS
}

/// Duplicate IMK frames (same box within [`CARET_EPS`]) are not written again.
pub fn caret_should_replace(previous: Option<CaretRect>, next: CaretRect) -> bool {
    !previous.is_some_and(|previous| caret_rects_close(previous, next))
}

/// A complete frame, ready to write to the desktop socket.
pub fn caret_position_frame(x: f64, y: f64, width: f64, height: f64, origin: Origin) -> Vec<u8> {
    let hook = caret_position_hook(x, y, width, height, origin);

    let mut body = Vec::with_capacity(hook.len() + 8);
    let mut caret_field = Vec::with_capacity(hook.len() + 4);
    push_message(&mut caret_field, HOOK_CARET_POSITION_FIELD, &hook);
    push_message(&mut body, LOCAL_MESSAGE_HOOK_FIELD, &caret_field);

    let mut frame = Vec::with_capacity(FIG_PBUF_PREFIX.len() + size_of::<u64>() + body.len());
    frame.extend_from_slice(FIG_PBUF_PREFIX);
    frame.extend_from_slice(&(body.len() as u64).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

#[cfg(test)]
mod tests {
    use fig_proto::FigProtobufEncodable;
    use fig_proto::hooks::{hook_to_message, new_caret_position_hook};
    use fig_proto::local::caret_position_hook::Origin as ProtoOrigin;

    use super::*;

    fn proto_origin(origin: Origin) -> ProtoOrigin {
        match origin {
            Origin::TopLeft => ProtoOrigin::TopLeft,
            Origin::BottomLeft => ProtoOrigin::BottomLeft,
        }
    }

    #[test]
    fn frame_matches_fig_proto() {
        let cases = [
            (1200.0, 800.0, 8.0, 16.0, Origin::BottomLeft),
            (0.0, 0.0, 0.0, 16.0, Origin::TopLeft),
            (-1920.5, -450.25, 0.0, 21.0, Origin::BottomLeft),
            (f64::MAX, f64::MIN, 1.0, f64::MIN_POSITIVE, Origin::TopLeft),
        ];

        for (x, y, width, height, origin) in cases {
            let expected = hook_to_message(new_caret_position_hook(x, y, width, height, proto_origin(origin)))
                .encode_fig_protobuf()
                .unwrap();
            assert_eq!(
                caret_position_frame(x, y, width, height, origin),
                expected.as_ref(),
                "frame mismatch for {x},{y},{width},{height},{origin:?}"
            );
        }
    }

    /// Beyond byte equality with the encoder: the exact parser the desktop app
    /// runs must accept the frame and hand back the same caret.
    #[test]
    fn frame_decodes_through_the_desktop_parser() {
        use fig_proto::local::{LocalMessage, hook, local_message};

        let frame = caret_position_frame(432.5, 918.0, 1.0, 21.0, Origin::BottomLeft);
        let mut buf = frame.as_slice();
        let (consumed, message) = fig_proto::FigMessage::parse(&mut buf).unwrap();
        assert_eq!(consumed, frame.len());

        let decoded: LocalMessage = message.decode().unwrap();
        let Some(local_message::Type::Hook(hook)) = decoded.r#type else {
            panic!("expected a hook message, got {decoded:?}");
        };
        let Some(hook::Hook::CaretPosition(caret)) = hook.hook else {
            panic!("expected a caret position hook, got {hook:?}");
        };
        assert_eq!((caret.x, caret.y, caret.width, caret.height), (432.5, 918.0, 1.0, 21.0));
        assert_eq!(caret.origin, Some(ProtoOrigin::BottomLeft as i32));
    }

    #[test]
    fn varint_spans_multiple_bytes() {
        let mut out = Vec::new();
        push_tag(&mut out, HOOK_CARET_POSITION_FIELD, WIRE_TYPE_LEN);
        assert_eq!(out, [0x8a, 0x07]);
    }

    #[test]
    fn zero_height_or_non_finite_caret_is_not_usable() {
        assert!(caret_rect_is_usable(10.0, 20.0, 0.0, 16.0));
        assert!(!caret_rect_is_usable(10.0, 20.0, 8.0, 0.0));
        assert!(!caret_rect_is_usable(10.0, 20.0, 8.0, -1.0));
        assert!(!caret_rect_is_usable(f64::NAN, 20.0, 8.0, 16.0));
        assert!(!caret_rect_is_usable(10.0, f64::INFINITY, 8.0, 16.0));
    }

    #[test]
    fn duplicate_caret_frames_are_coalesced() {
        let first = (10.0, 20.0, 8.0, 16.0);
        assert!(caret_should_replace(None, first));
        assert!(!caret_should_replace(Some(first), first));
        assert!(!caret_should_replace(Some(first), (10.4, 20.0, 8.0, 16.0)));
        assert!(caret_should_replace(Some(first), (11.0, 20.0, 8.0, 16.0)));
    }
}

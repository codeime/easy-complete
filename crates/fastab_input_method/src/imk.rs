use std::cell::Cell;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use objc2::mutability::InteriorMutable;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, Bool};
use objc2::{ClassType, DeclaredClass, declare_class, msg_send, msg_send_id, sel};
use objc2_foundation::{NSDistributedNotificationCenter, NSPoint, NSRange, NSRect, NSSize, NSString, ns_string};
use objc2_input_method_kit::{IMKInputController, IMKServer};

use crate::wire::{self, Origin};
use crate::{paths, terminals};

const INPUT_CONTROLLER_CLASS_NAME: &str = env!("InputMethodServerControllerClass");

/// A remote IMK proxy may return nil here. Never assume otherwise: a panic on
/// this process's main thread kills the IME and strands every attached terminal.
fn bundle_identifier(client: &AnyObject) -> Option<String> {
    let bundle_id: Option<Retained<NSString>> = unsafe { msg_send_id![client, bundleIdentifier] };
    Some(bundle_id?.to_string())
}

/// A client that no longer backs a live window — a closed Ghostty window whose input controller
/// is still registered, for example — leaves `attributesForCharacterIndex:lineHeightRectangle:`
/// untouched, so the caret rect stays at its zeroed default. Sending that on would place the
/// overlay at the screen-space origin, which the desktop then clamps to the bottom-left corner of
/// the primary monitor — on multi-monitor setups that is a different screen than the terminal.
fn is_valid_caret_rect(rect: NSRect) -> bool {
    rect.origin.x.is_finite()
        && rect.origin.y.is_finite()
        && rect.size.width.is_finite()
        && rect.size.height.is_finite()
        && rect.size.height > 0.0
}

/// Otty / Ghostty / Kitty attach through IMK. After an IME restart or a
/// palette-input-source switch, `deactivateServer:` flips `is_active` off
/// while the terminal is still the key window — AX cannot see their caret,
/// so dropping the request here is what makes the overlay vanish mid-session.
fn client_is_key_window(client: &AnyObject) -> bool {
    let window_sel = sel!(window);
    let responds: Bool = unsafe { msg_send![client, respondsToSelector: window_sel] };
    if !responds.as_bool() {
        return false;
    }
    let window: *const AnyObject = unsafe { msg_send![client, window] };
    if window.is_null() {
        return false;
    }
    let is_key: Bool = unsafe { msg_send![window, isKeyWindow] };
    is_key.as_bool()
}

const CARET_EPS: f64 = 0.5;
const CARET_WRITE_TIMEOUT: Duration = Duration::from_millis(200);
/// How long the sender thread waits for another caret before giving itself up. An
/// idle input method should hold nothing but its AppKit main thread; the next
/// keystroke starts a new one.
const SENDER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

type CaretRect = (f64, f64, f64, f64);

fn caret_rect_tuple(rect: NSRect) -> CaretRect {
    (rect.origin.x, rect.origin.y, rect.size.width, rect.size.height)
}

fn caret_rects_close(left: CaretRect, right: CaretRect) -> bool {
    (left.0 - right.0).abs() < CARET_EPS
        && (left.1 - right.1).abs() < CARET_EPS
        && (left.2 - right.2).abs() < CARET_EPS
        && (left.3 - right.3).abs() < CARET_EPS
}

fn should_send_caret(last: &Cell<Option<CaretRect>>, rect: NSRect) -> bool {
    let next = caret_rect_tuple(rect);
    if last.get().is_some_and(|previous| caret_rects_close(previous, next)) {
        return false;
    }
    last.set(Some(next));
    true
}

/// Live only while carets are flowing; see [`SENDER_IDLE_TIMEOUT`].
static SENDER: Mutex<Option<Sender<Vec<u8>>>> = Mutex::new(None);

fn enqueue_caret_frame(frame: Vec<u8>) {
    let mut guard = SENDER.lock().unwrap_or_else(|err| err.into_inner());
    if guard.is_none() {
        *guard = start_caret_sender();
    }
    let Some(sender) = guard.as_ref() else {
        return;
    };
    // Retirement takes this lock before exiting, so a disconnect here means the
    // thread died some other way. Rebuild it and hand the frame straight over.
    if let Err(err) = sender.send(frame) {
        *guard = start_caret_sender();
        if let Some(sender) = guard.as_ref() {
            let _ = sender.send(err.0);
        }
    }
}

fn start_caret_sender() -> Option<Sender<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    match std::thread::Builder::new()
        .name("imk-caret".into())
        .spawn(move || caret_sender_loop(rx))
    {
        Ok(_) => Some(tx),
        Err(err) => {
            log_error!("could not start the caret sender thread: {err}");
            None
        },
    }
}

fn drain_latest(rx: &Receiver<Vec<u8>>, first: Vec<u8>) -> Vec<u8> {
    let mut latest = first;
    while let Ok(next) = rx.try_recv() {
        latest = next;
    }
    latest
}

fn caret_sender_loop(rx: Receiver<Vec<u8>>) {
    loop {
        let first = match rx.recv_timeout(SENDER_IDLE_TIMEOUT) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => {
                // Retire under the registry lock: an `enqueue_caret_frame` that
                // has already taken a clone of this sender cannot then lose its
                // frame to the exit.
                let mut guard = SENDER.lock().unwrap_or_else(|err| err.into_inner());
                match rx.try_recv() {
                    Ok(frame) => frame,
                    Err(_) => {
                        guard.take();
                        return;
                    },
                }
            },
            Err(RecvTimeoutError::Disconnected) => return,
        };

        send_caret_frame(&drain_latest(&rx, first));
    }
}

fn send_caret_frame(frame: &[u8]) {
    if let Some(path) = paths::desktop_socket_path() {
        send_caret_frame_to(path, frame);
    }
}

fn send_caret_frame_to(path: &std::path::Path, frame: &[u8]) {
    // The desktop owns this socket. When it is not running there is nobody to
    // tell, so skip the connect rather than fail it on every keystroke.
    if !path.exists() {
        return;
    }

    match UnixStream::connect(path) {
        Ok(mut stream) => {
            let _ = stream.set_write_timeout(Some(CARET_WRITE_TIMEOUT));
            if let Err(err) = stream.write_all(frame) {
                log_debug!("caret frame not written: {err}");
            }
        },
        Err(err) => log_debug!("desktop socket not reachable: {err}"),
    }
}

fn report_caret_from_client(
    client: &AnyObject,
    bundle_id: Option<&str>,
    reason: &str,
    last_sent: &Cell<Option<CaretRect>>,
) {
    let bundle_id = bundle_id.unwrap_or_default();
    if !terminals::supports_input_method(bundle_id) {
        log_debug!("Instance {bundle_id:?} is not a supported terminal, ignoring request");
        return;
    }

    log_debug!("Instance {bundle_id:?} is {reason}, handling request");
    let mut rect: NSRect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            height: 0.0,
            width: 0.0,
        },
    };
    let _: () = unsafe { msg_send![client, attributesForCharacterIndex: 0 lineHeightRectangle: &mut rect] };

    if !is_valid_caret_rect(rect) {
        log_debug!("Instance {bundle_id:?} reported an invalid caret rect {rect:?}, ignoring request");
        return;
    }
    if !should_send_caret(last_sent, rect) {
        return;
    }

    log_debug!("Sending cursor position for {bundle_id:?}: {rect:?}");
    enqueue_caret_frame(wire::caret_position_frame(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
        Origin::BottomLeft,
    ));
}

struct Ivars {
    is_active: Cell<bool>,
    last_sent: Cell<Option<CaretRect>>,
}

declare_class!(
    struct MyInputController;

    // - The superclass IMKInputController does not have any subclassing requirements.
    // - Interior mutability is a safe default.
    // - `MyInputController` does not implement `Drop`.
    unsafe impl ClassType for MyInputController {
        type Super = IMKInputController;
        type Mutability = InteriorMutable;
        const NAME: &'static str = INPUT_CONTROLLER_CLASS_NAME;
    }

    impl DeclaredClass for MyInputController {
        type Ivars = Ivars;
    }

    unsafe impl MyInputController {
        #[method_id(initWithServer:delegate:client:)]
        fn init_with_server_delegate_client(this: Allocated<Self>, server: Option<&IMKServer>, delegate: Option<&AnyObject>, client: Option<&AnyObject>) -> Retained<Self> {
            log_info!("INITING");
            let partial = this.set_ivars(Ivars {
                is_active: Cell::new(true),
                last_sent: Cell::new(None),
            });
            let this: Retained<Self> = unsafe { msg_send_id![super(partial, IMKInputController::class()), initWithServer:server delegate: delegate client: client] };

            // The desktop app posts this whenever it wants the caret for the focused window.
            let center = unsafe { NSDistributedNotificationCenter::defaultCenter() };
            unsafe {
                center.addObserver_selector_name_object(
                    &this,
                    sel!(handleCursorPositionRequest:),
                    Some(ns_string!("com.amazon.codewhisperer.edit_buffer_updated")),
                    None,
                );
            }

            this
        }

        #[method(activateServer:)]
        fn activate_server(&self, client: Option<&AnyObject>) {
            self.ivars().is_active.set(true);
            let Some(client) = client else {
                return;
            };

            let bundle_id = bundle_identifier(client);
            log_info!("activated server: {bundle_id:?}");

            // Used to trigger input method enabled in Alacritty
            if bundle_id.as_deref().is_some_and(terminals::is_alacritty) {
                let empty_range = NSRange::new(0, 0);
                let space_string = ns_string!(" ");
                let empty_string = ns_string!("");

                unsafe {
                    // First, setMarkedText with a non-empty string in order to enable winit IME
                    // https://github.com/rust-windowing/winit/blob/97d4c7b303bb8110df6c492f0c2327b7d5098347/src/platform_impl/macos/view.rs#L330-L337
                    let _: () = msg_send![client, setMarkedText: space_string selectionRange: empty_range replacementRange: empty_range];

                    // Then, since we don't *actually* want to be in the preedit state, set marked text to an empty
                    // string to invalidate https://github.com/rust-windowing/winit/blob/97d4c7b303bb8110df6c492f0c2327b7d5098347/src/platform_impl/macos/view.rs#L345-L351
                    let _: () = msg_send![client, setMarkedText: empty_string selectionRange: empty_range replacementRange: empty_range];
                }
            }
        }

        #[method(deactivateServer:)]
        fn deactivate_server(&self, client: Option<&AnyObject>) {
            self.ivars().is_active.set(false);
            log_info!("deactivated server: {:?}", client.and_then(bundle_identifier));
        }

        #[method(handleCursorPositionRequest:)]
        fn handle_cursor_position_request(&self, _notif: Option<&AnyObject>) {
            // Nil once the client app has gone away while the controller lingers.
            let client: *mut AnyObject = unsafe { msg_send![self, client] };
            let Some(client) = (unsafe { client.as_ref() }) else {
                return;
            };
            let is_active = self.ivars().is_active.get();
            if !is_active && !client_is_key_window(client) {
                return;
            }
            let bundle_id = bundle_identifier(client);
            let reason = if is_active { "active" } else { "key window" };
            report_caret_from_client(client, bundle_id.as_deref(), reason, &self.ivars().last_sent);
        }
    }
);

pub fn connect_imkserver(name: &NSString, identifier: Option<&NSString>) {
    log_info!("connecting to imkserver");
    let server_alloc = IMKServer::alloc();
    unsafe { IMKServer::initWithName_bundleIdentifier(server_alloc, Some(name), identifier) };
    log_info!("connected to imkserver");
}

pub fn register_controller() {
    log_info!("registering {INPUT_CONTROLLER_CLASS_NAME}...");

    MyInputController::class();

    log_info!("finished registering {INPUT_CONTROLLER_CLASS_NAME}.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect {
            origin: NSPoint { x, y },
            size: NSSize { width, height },
        }
    }

    #[test]
    fn zeroed_caret_rect_is_rejected() {
        assert!(!is_valid_caret_rect(rect(0.0, 0.0, 0.0, 0.0)));
    }

    #[test]
    fn caret_rect_without_height_is_rejected() {
        assert!(!is_valid_caret_rect(rect(1200.0, 800.0, 8.0, 0.0)));
        assert!(!is_valid_caret_rect(rect(1200.0, 800.0, 8.0, -16.0)));
    }

    #[test]
    fn non_finite_caret_rect_is_rejected() {
        assert!(!is_valid_caret_rect(rect(f64::NAN, 800.0, 8.0, 16.0)));
        assert!(!is_valid_caret_rect(rect(1200.0, f64::INFINITY, 8.0, 16.0)));
    }

    #[test]
    fn caret_rect_on_a_secondary_monitor_is_accepted() {
        // Screens left of or below the primary one report negative Cocoa origins.
        assert!(is_valid_caret_rect(rect(-1920.0, -450.0, 0.0, 16.0)));
        assert!(is_valid_caret_rect(rect(1200.0, 800.0, 8.0, 16.0)));
    }

    #[test]
    fn identical_caret_is_not_resent() {
        let last = Cell::new(None);
        assert!(should_send_caret(&last, rect(100.0, 200.0, 1.0, 16.0)));
        assert!(!should_send_caret(&last, rect(100.2, 200.1, 1.0, 16.3)));
        assert!(should_send_caret(&last, rect(140.0, 200.0, 1.0, 16.0)));
    }

    #[test]
    fn caret_sender_keeps_only_the_latest_frame() {
        let (tx, rx) = mpsc::channel();
        let first = wire::caret_position_frame(1.0, 2.0, 1.0, 16.0, Origin::BottomLeft);
        let last = wire::caret_position_frame(9.0, 8.0, 1.0, 16.0, Origin::BottomLeft);
        tx.send(first).unwrap();
        tx.send(last.clone()).unwrap();
        let seed = wire::caret_position_frame(0.0, 0.0, 1.0, 16.0, Origin::BottomLeft);
        assert_eq!(drain_latest(&rx, seed), last);
    }

    #[test]
    fn a_missing_desktop_socket_is_not_an_error() {
        // Uses an explicit dead path: the real one may have a live desktop app
        // behind it, and a test must not feed that overlay a fake caret.
        let dir = std::env::temp_dir().join("ec-imk-test-no-such-dir");
        send_caret_frame_to(
            &dir.join("desktop.sock"),
            &wire::caret_position_frame(1.0, 2.0, 1.0, 16.0, Origin::BottomLeft),
        );
    }
}

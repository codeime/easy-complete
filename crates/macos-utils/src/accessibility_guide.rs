//! Codex-style Accessibility grant: open System Settings and float a
//! draggable app-icon card beside the list. Never raises the system TCC sheet.

#![allow(unexpected_cfgs)]

use std::ffi::CStr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU8, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::context::CGContext;
use core_graphics::display::{CGPoint, CGRect, CGSize, CGWindowListCopyWindowInfo};
use core_graphics::window::{
    kCGNullWindowID, kCGWindowBounds, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
    kCGWindowOwnerName, kCGWindowOwnerPID,
};
use objc::declare::ClassDecl;
use objc::rc::autoreleasepool;
use objc::runtime::{BOOL, Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use tracing::debug;

use crate::accessibility::{accessibility_is_enabled, open_accessibility};
use crate::applications::running_application_pids;
use crate::bundle::{get_bundle_identifier, get_bundle_path};

const CARD_WIDTH: f64 = 288.0;
const CARD_HEIGHT: f64 = 172.0;
const CARD_GAP: f64 = 20.0;
const FLIGHT_MS: f64 = 480.0;
const ARC_HEIGHT: f64 = 140.0;
const ARROW_TAG: isize = 7101;
const SETTINGS_GONE_TICKS: u8 = 25;
const NS_DRAG_OPERATION_COPY: usize = 1;
const DRAG_CHIP_RADIUS: f64 = 10.0;

/// Same mask as the overlay: click/drag must not activate Easy Complete.
const NS_WINDOW_STYLE_NONACTIVATING_PANEL: u64 = 1 << 7;
const NS_WINDOW_ANIMATION_BEHAVIOR_NONE: i64 = 2;
const NS_MODAL_PANEL_WINDOW_LEVEL: i64 = 8;
const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: u64 = 1 << 0;
const NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY: u64 = 1 << 4;
const NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE: u64 = 1 << 6;
const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: u64 = 1 << 8;
const NS_VISUAL_EFFECT_MATERIAL_POPOVER: i64 = 6;
const NS_VISUAL_EFFECT_BLENDING_BEHIND_WINDOW: i64 = 0;
const NS_VISUAL_EFFECT_STATE_ACTIVE: i64 = 1;
const NS_FONT_WEIGHT_SEMIBOLD: f64 = 0.3;
const NS_FONT_WEIGHT_REGULAR: f64 = 0.0;

static GUIDE_ACTIVE: AtomicBool = AtomicBool::new(false);
static FLYING: AtomicBool = AtomicBool::new(false);
static DRAGGING: AtomicBool = AtomicBool::new(false);
static SETTINGS_MISSING: AtomicU8 = AtomicU8::new(0);
/// -1 system locale, 0 English, 1 Chinese.
static PREFER_ZH: AtomicI8 = AtomicI8::new(-1);
static PANEL: Mutex<Option<usize>> = Mutex::new(None);
static REGISTER_CLASSES: Once = Once::new();

pub fn accessibility_guide_is_active() -> bool {
    GUIDE_ACTIVE.load(Ordering::SeqCst)
}

/// Open the Accessibility pane and dock a drag-to-grant card beside it.
/// Safe to call from a background thread. Does nothing if already granted.
///
/// `prefer_zh` follows the settings page (`dashboard.language`): `Some(true)`
/// Chinese, `Some(false)` English, `None` the system locale.
pub fn begin_accessibility_guide(prefer_zh: Option<bool>) {
    if on_main_thread() {
        start_guide(prefer_zh);
    } else {
        dispatch::Queue::main().exec_async(move || start_guide(prefer_zh));
    }
}

fn start_guide(prefer_zh: Option<bool>) {
    if accessibility_is_enabled() {
        dismiss_guide();
        return;
    }
    if GUIDE_ACTIVE.load(Ordering::SeqCst) {
        if settings_window_cocoa().is_none() {
            open_accessibility();
        }
        if let Some(panel) = panel_ptr() {
            unsafe {
                let _: () = msg_send![panel, orderFrontRegardless];
            }
        }
        return;
    }

    PREFER_ZH.store(
        match prefer_zh {
            Some(false) => 0,
            Some(true) => 1,
            None => -1,
        },
        Ordering::SeqCst,
    );
    GUIDE_ACTIVE.store(true, Ordering::SeqCst);
    let origin = mouse_location();
    // A cdhash-stale grant stays in the list with the switch on, but this
    // process is not trusted. Drop our row so the current binary can be dragged in.
    clear_stale_accessibility_row();
    open_accessibility();
    present_card_at(origin);
    wait_for_settings(0, None);
}

fn wait_for_settings(attempt: u8, last: Option<(f64, f64, f64, f64)>) {
    if !GUIDE_ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    if DRAGGING.load(Ordering::SeqCst) {
        dispatch::Queue::main().exec_after(Duration::from_millis(50), move || {
            wait_for_settings(attempt, last);
        });
        return;
    }
    if accessibility_is_enabled() {
        dismiss_guide();
        return;
    }
    let current = settings_window_cocoa();
    let stable = match (last, current) {
        (Some(previous), Some(now)) => frames_close(previous, now),
        _ => false,
    };
    if stable || attempt >= 60 {
        let settings = current.or(last).unwrap_or_else(fallback_settings_frame);
        fly_to_docked(settings);
        return;
    }
    dispatch::Queue::main().exec_after(Duration::from_millis(50), move || {
        wait_for_settings(attempt.saturating_add(1), current.or(last));
    });
}

fn fly_to_docked(settings: (f64, f64, f64, f64)) {
    let Some(panel) = panel_ptr() else {
        return;
    };
    let start = unsafe {
        let frame: NSRect = msg_send![panel, frame];
        (frame.origin.x, frame.origin.y)
    };
    FLYING.store(true, Ordering::SeqCst);
    fly_step(start, settings, Instant::now());
}

fn fly_step(start: (f64, f64), last_settings: (f64, f64, f64, f64), started: Instant) {
    if !GUIDE_ACTIVE.load(Ordering::SeqCst) {
        FLYING.store(false, Ordering::SeqCst);
        return;
    }
    // Moving the source window (or releasing it) cancels an async drag.
    if DRAGGING.load(Ordering::SeqCst) {
        FLYING.store(false, Ordering::SeqCst);
        schedule_tick();
        return;
    }
    if accessibility_is_enabled() {
        dismiss_guide();
        return;
    }

    let settings = settings_window_cocoa().unwrap_or(last_settings);
    let screen = screen_containing(settings.0 + settings.2 / 2.0, settings.1 + settings.3 / 2.0);
    let docked = docked_card_frame(settings, screen);
    let linear = (started.elapsed().as_secs_f64() * 1000.0 / FLIGHT_MS).clamp(0.0, 1.0);
    let t = ease_in_out(linear);
    let (x, y) = bezier_point(start, (docked.0, docked.1), t);
    let (x, y) = clamp_to_screen(x, y, screen);

    if let Some(panel) = panel_ptr() {
        let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(CARD_WIDTH, CARD_HEIGHT));
        unsafe {
            let _: () = msg_send![panel, setFrame: frame display: YES];
        }
    }

    if linear >= 1.0 {
        FLYING.store(false, Ordering::SeqCst);
        update_arrow(card_is_left_of(settings, docked));
        schedule_tick();
        return;
    }
    dispatch::Queue::main().exec_after(Duration::from_millis(16), move || {
        fly_step(start, settings, started);
    });
}

fn present_card_at(origin: NSPoint) {
    if !GUIDE_ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    if panel_ptr().is_some() {
        return;
    }
    register_classes();

    let screen = screen_containing(origin.x, origin.y);
    let x = (origin.x - CARD_WIDTH / 2.0).clamp(screen.0 + 12.0, screen.0 + screen.2 - CARD_WIDTH - 12.0);
    let y = (origin.y - CARD_HEIGHT / 2.0).clamp(screen.1 + 12.0, screen.1 + screen.3 - CARD_HEIGHT - 12.0);
    let start = NSRect::new(NSPoint::new(x, y), NSSize::new(CARD_WIDTH, CARD_HEIGHT));

    let Some(cls) = Class::get("ECAccessibilityGuidePanel") else {
        return;
    };
    unsafe {
        let panel: id = msg_send![cls, alloc];
        let panel: id = msg_send![
            panel,
            initWithContentRect: start
            styleMask: NS_WINDOW_STYLE_NONACTIVATING_PANEL
            backing: 2u64
            defer: NO
        ];
        let _: () = msg_send![panel, setReleasedWhenClosed: NO];
        let _: () = msg_send![panel, setOpaque: NO];
        let _: () = msg_send![panel, setHasShadow: YES];
        let _: () = msg_send![panel, setHidesOnDeactivate: NO];
        let _: () = msg_send![panel, setFloatingPanel: YES];
        let _: () = msg_send![panel, setBecomesKeyOnlyIfNeeded: YES];
        let _: () = msg_send![panel, setLevel: NS_MODAL_PANEL_WINDOW_LEVEL];
        let behavior: u64 = NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
            | NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY
            | NS_WINDOW_COLLECTION_BEHAVIOR_IGNORES_CYCLE
            | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY;
        let _: () = msg_send![panel, setCollectionBehavior: behavior];
        let clear: id = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![panel, setBackgroundColor: clear];
        let _: () = msg_send![panel, setAnimationBehavior: NS_WINDOW_ANIMATION_BEHAVIOR_NONE];
        let _: () = msg_send![panel, setIgnoresMouseEvents: NO];

        let content: id = msg_send![panel, contentView];
        autoreleasepool(|| build_card_content(content));

        let _: () = msg_send![panel, orderFrontRegardless];
        *PANEL.lock().unwrap_or_else(|err| err.into_inner()) = Some(panel as usize);
    }
    debug!("accessibility guide card shown");
}

fn build_card_content(content: id) {
    unsafe {
        let bounds: NSRect = msg_send![content, bounds];
        let effect_cls = Class::get("ECAccessibilityCardView").unwrap_or_else(|| class!(NSVisualEffectView));
        let effect: id = msg_send![effect_cls, alloc];
        let effect: id = msg_send![effect, initWithFrame: bounds];
        let _: () = msg_send![effect, setMaterial: NS_VISUAL_EFFECT_MATERIAL_POPOVER];
        let _: () = msg_send![effect, setBlendingMode: NS_VISUAL_EFFECT_BLENDING_BEHIND_WINDOW];
        let _: () = msg_send![effect, setState: NS_VISUAL_EFFECT_STATE_ACTIVE];
        let _: () = msg_send![effect, setAutoresizingMask: 18u64];
        let _: () = msg_send![effect, setWantsLayer: YES];
        let layer: id = msg_send![effect, layer];
        let _: () = msg_send![layer, setCornerRadius: 16.0f64];
        let _: () = msg_send![layer, setMasksToBounds: YES];
        adopt_subview(content, effect);
        let _: () = msg_send![content, setWantsLayer: YES];
        let content_layer: id = msg_send![content, layer];
        let _: () = msg_send![content_layer, setCornerRadius: 16.0f64];
        let _: () = msg_send![content_layer, setMasksToBounds: YES];

        let zh = prefers_zh();
        let title = if zh {
            "授予辅助功能"
        } else {
            "Grant Accessibility"
        };
        let body = if zh {
            "把下面的图标拖进旁边的应用列表，然后打开开关。"
        } else {
            "Drag the icon into the app list beside this card, then turn it on."
        };

        add_label(
            effect,
            title,
            15.0,
            true,
            NSRect::new(NSPoint::new(20.0, 132.0), NSSize::new(220.0, 22.0)),
        );
        add_label(
            effect,
            body,
            12.0,
            false,
            NSRect::new(NSPoint::new(20.0, 78.0), NSSize::new(248.0, 50.0)),
        );
        add_close_button(effect);
        add_arrow(effect);
        add_drag_row(effect, NSRect::new(NSPoint::new(20.0, 16.0), NSSize::new(248.0, 52.0)));
    }
}

fn add_label(parent: id, text: &str, size: f64, bold: bool, frame: NSRect) {
    unsafe {
        let label: id = msg_send![class!(NSTextField), labelWithString: ns_string(text)];
        let _: () = msg_send![label, setFrame: frame];
        let weight = if bold {
            NS_FONT_WEIGHT_SEMIBOLD
        } else {
            NS_FONT_WEIGHT_REGULAR
        };
        let font: id = msg_send![class!(NSFont), systemFontOfSize: size weight: weight];
        if !font.is_null() {
            let _: () = msg_send![label, setFont: font];
        }
        let color: id = if bold {
            msg_send![class!(NSColor), labelColor]
        } else {
            msg_send![class!(NSColor), secondaryLabelColor]
        };
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setSelectable: NO];
        let _: () = msg_send![label, setLineBreakMode: 0i64];
        let _: () = msg_send![label, setUsesSingleLineMode: NO];
        let _: () = msg_send![label, setMaximumNumberOfLines: 3i64];
        let cell: id = msg_send![label, cell];
        if !cell.is_null() {
            let _: () = msg_send![cell, setWraps: YES];
        }
        let _: () = msg_send![parent, addSubview: label];
    }
}

fn add_close_button(parent: id) {
    let button_cls = Class::get("ECAccessibilityCloseButton").unwrap_or_else(|| class!(NSButton));
    unsafe {
        let button: id = msg_send![button_cls, new];
        let symbol: id = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: ns_string("xmark")
            accessibilityDescription: nil
        ];
        if symbol.is_null() {
            let _: () = msg_send![button, setTitle: ns_string("✕")];
        } else {
            let _: () = msg_send![button, setImage: symbol];
            let _: () = msg_send![button, setTitle: ns_string("")];
        }
        let _: () = msg_send![button, setBezelStyle: 1u64];
        let _: () = msg_send![button, setBordered: NO];
        let _: () = msg_send![button, setImagePosition: 1u64];
        let _: () = msg_send![
            button,
            setFrame: NSRect::new(NSPoint::new(254.0, 140.0), NSSize::new(22.0, 22.0))
        ];
        let target = close_target();
        let _: () = msg_send![button, setTarget: target];
        let _: () = msg_send![button, setAction: sel!(closeGuide:)];
        adopt_subview(parent, button);
    }
}

fn add_arrow(parent: id) {
    unsafe {
        let label: id = msg_send![class!(NSTextField), labelWithString: ns_string("")];
        let _: () = msg_send![label, setTag: ARROW_TAG];
        let _: () = msg_send![label, setHidden: YES];
        let font: id = msg_send![class!(NSFont), systemFontOfSize: 16.0f64];
        let _: () = msg_send![label, setFont: font];
        let color: id = msg_send![class!(NSColor), tertiaryLabelColor];
        let _: () = msg_send![label, setTextColor: color];
        let _: () = msg_send![label, setDrawsBackground: NO];
        let _: () = msg_send![label, setBezeled: NO];
        let _: () = msg_send![label, setEditable: NO];
        let _: () = msg_send![label, setSelectable: NO];
        let _: () = msg_send![label, setAlignment: 1u64];
        let _: () = msg_send![parent, addSubview: label];
    }
}

fn update_arrow(docked_on_left: bool) {
    let Some(panel) = panel_ptr() else {
        return;
    };
    autoreleasepool(|| unsafe {
        let content: id = msg_send![panel, contentView];
        if content.is_null() {
            return;
        }
        let arrow: id = msg_send![content, viewWithTag: ARROW_TAG];
        if arrow.is_null() {
            return;
        }
        let (text, frame) = if docked_on_left {
            (
                "▸",
                NSRect::new(NSPoint::new(CARD_WIDTH - 18.0, 28.0), NSSize::new(16.0, 20.0)),
            )
        } else {
            ("◂", NSRect::new(NSPoint::new(4.0, 28.0), NSSize::new(16.0, 20.0)))
        };
        let _: () = msg_send![arrow, setStringValue: ns_string(text)];
        let _: () = msg_send![arrow, setFrame: frame];
        let _: () = msg_send![arrow, setHidden: NO];
    });
}

fn add_drag_row(parent: id, frame: NSRect) {
    let Some(cls) = Class::get("ECAccessibilityDragView") else {
        return;
    };
    let Some(bundle) = get_bundle_path() else {
        return;
    };
    let name = app_display_name();
    unsafe {
        let view: id = msg_send![cls, alloc];
        let view: id = msg_send![view, initWithFrame: frame];
        let path = owned_ns_string(&bundle.to_string_lossy());
        (*view).set_ivar("bundlePath", path);
        let _: () = msg_send![view, setWantsLayer: YES];
        let _: () = msg_send![view, setOpaque: NO];
        let layer: id = msg_send![view, layer];
        let _: () = msg_send![layer, setCornerRadius: DRAG_CHIP_RADIUS];
        let _: () = msg_send![layer, setMasksToBounds: YES];
        style_drag_row_layer(layer, false);

        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let icon: id = msg_send![workspace, iconForFile: path];
        let _: () = msg_send![icon, setSize: NSSize::new(36.0, 36.0)];
        let image_view: id = msg_send![class!(NSImageView), new];
        let _: () = msg_send![image_view, setImage: icon];
        let _: () = msg_send![image_view, setFrame: NSRect::new(NSPoint::new(10.0, 8.0), NSSize::new(36.0, 36.0))];
        let _: () = msg_send![image_view, setEditable: NO];
        adopt_subview(view, image_view);

        add_label(
            view,
            &name,
            13.0,
            true,
            NSRect::new(NSPoint::new(54.0, 15.0), NSSize::new(184.0, 22.0)),
        );
        adopt_subview(parent, view);
    }
}

fn register_classes() {
    REGISTER_CLASSES.call_once(|| {
        if let Some(mut decl) = ClassDecl::new("ECAccessibilityGuidePanel", class!(NSPanel)) {
            unsafe {
                decl.add_method(
                    sel!(canBecomeKeyWindow),
                    can_become_key as extern "C" fn(&Object, Sel) -> BOOL,
                );
                decl.add_method(
                    sel!(canBecomeMainWindow),
                    can_become_main as extern "C" fn(&Object, Sel) -> BOOL,
                );
            }
            decl.register();
        }
        if let Some(mut decl) = ClassDecl::new("ECAccessibilityCardView", class!(NSVisualEffectView)) {
            unsafe {
                decl.add_method(
                    sel!(acceptsFirstMouse:),
                    accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL,
                );
            }
            decl.register();
        }
        if let Some(mut decl) = ClassDecl::new("ECAccessibilityCloseButton", class!(NSButton)) {
            unsafe {
                decl.add_method(
                    sel!(acceptsFirstMouse:),
                    accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL,
                );
            }
            decl.register();
        }
        if let Some(mut decl) = ClassDecl::new("ECAccessibilityDragView", class!(NSView)) {
            decl.add_ivar::<id>("bundlePath");
            decl.add_ivar::<BOOL>("didStartDrag");
            unsafe {
                decl.add_method(sel!(mouseDown:), mouse_down as extern "C" fn(&mut Object, Sel, id));
                decl.add_method(
                    sel!(mouseDragged:),
                    mouse_dragged as extern "C" fn(&mut Object, Sel, id),
                );
                decl.add_method(
                    sel!(acceptsFirstMouse:),
                    accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL,
                );
                decl.add_method(sel!(hitTest:), hit_test as extern "C" fn(&Object, Sel, NSPoint) -> id);
                decl.add_method(
                    sel!(resetCursorRects),
                    reset_cursor_rects as extern "C" fn(&Object, Sel),
                );
                decl.add_method(
                    sel!(draggingSession:sourceOperationMaskForDraggingContext:),
                    source_operation_mask as extern "C" fn(&Object, Sel, id, isize) -> usize,
                );
                decl.add_method(
                    sel!(draggingSession:endedAtPoint:operation:),
                    drag_ended as extern "C" fn(&Object, Sel, id, NSPoint, usize),
                );
                decl.add_method(sel!(dealloc), drag_dealloc as extern "C" fn(&mut Object, Sel));
            }
            decl.register();
        }
    });
}

extern "C" fn can_become_key(_this: &Object, _sel: Sel) -> BOOL {
    YES
}

extern "C" fn can_become_main(_this: &Object, _sel: Sel) -> BOOL {
    NO
}

fn view_id(this: &Object) -> id {
    this as *const Object as id
}

extern "C" fn mouse_down(this: &mut Object, _sel: Sel, _event: id) {
    unsafe {
        this.set_ivar("didStartDrag", NO);
    }
}

extern "C" fn mouse_dragged(this: &mut Object, _sel: Sel, event: id) {
    unsafe {
        let path: id = *this.get_ivar("bundlePath");
        if path.is_null() {
            return;
        }
        let started: BOOL = *this.get_ivar("didStartDrag");
        if started == YES {
            return;
        }
        this.set_ivar("didStartDrag", YES);
        let this_id = this as *mut Object as id;
        DRAGGING.store(true, Ordering::SeqCst);
        if !begin_url_drag(this_id, path, event) {
            let bounds: NSRect = msg_send![this_id, bounds];
            let _: BOOL = msg_send![this_id, dragFile: path fromRect: bounds slideBack: YES event: event];
            finish_drag();
        }
    }
}

fn begin_url_drag(view: id, path: id, event: id) -> bool {
    let Some(_) = Class::get("NSDraggingItem") else {
        return false;
    };
    autoreleasepool(|| unsafe {
        let url: id = msg_send![class!(NSURL), fileURLWithPath: path];
        if url.is_null() {
            return false;
        }
        let item: id = msg_send![class!(NSDraggingItem), alloc];
        let item: id = msg_send![item, initWithPasteboardWriter: url];
        if item.is_null() {
            return false;
        }
        let bounds: NSRect = msg_send![view, bounds];
        let preview = drag_preview_image(view);
        if preview.is_null() {
            let _: () = msg_send![item, release];
            return false;
        }
        let _: () = msg_send![item, setDraggingFrame: bounds contents: preview];
        let _: () = msg_send![preview, release];
        let items: id = msg_send![class!(NSArray), arrayWithObject: item];
        let _: () = msg_send![item, release];
        let session: id = msg_send![view, beginDraggingSessionWithItems: items event: event source: view];
        !session.is_null()
    })
}

fn style_drag_row_layer(layer: id, for_preview: bool) {
    if layer.is_null() {
        return;
    }
    unsafe {
        let dark = is_dark_appearance();
        let fill: id = if for_preview {
            if dark {
                msg_send![class!(NSColor), colorWithWhite: 0.22f64 alpha: 0.94f64]
            } else {
                msg_send![class!(NSColor), colorWithWhite: 1.0f64 alpha: 0.94f64]
            }
        } else if dark {
            msg_send![class!(NSColor), colorWithWhite: 1.0f64 alpha: 0.10f64]
        } else {
            msg_send![class!(NSColor), colorWithWhite: 0.0f64 alpha: 0.06f64]
        };
        let cg: id = msg_send![fill, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: cg];
        if for_preview {
            let stroke: id = if dark {
                msg_send![class!(NSColor), colorWithWhite: 1.0f64 alpha: 0.22f64]
            } else {
                msg_send![class!(NSColor), colorWithWhite: 0.0f64 alpha: 0.12f64]
            };
            let stroke_cg: id = msg_send![stroke, CGColor];
            let _: () = msg_send![layer, setBorderColor: stroke_cg];
            let _: () = msg_send![layer, setBorderWidth: 1.0f64];
        } else {
            let _: () = msg_send![layer, setBorderWidth: 0.0f64];
        }
    }
}

fn backing_scale_for_view(view: id) -> f64 {
    unsafe {
        let window: id = msg_send![view, window];
        if !window.is_null() {
            let screen: id = msg_send![window, screen];
            if !screen.is_null() {
                let scale: f64 = msg_send![screen, backingScaleFactor];
                if scale > 0.0 {
                    return scale;
                }
            }
        }
        let screens: id = msg_send![class!(NSScreen), screens];
        if !screens.is_null() {
            let count: usize = msg_send![screens, count];
            if count > 0 {
                let screen: id = msg_send![screens, objectAtIndex: 0usize];
                let scale: f64 = msg_send![screen, backingScaleFactor];
                if scale > 0.0 {
                    return scale;
                }
            }
        }
    }
    2.0
}

/// Snapshot the live icon+name row via the layer tree (rounded fill included).
fn drag_preview_image(view: id) -> id {
    unsafe {
        let bounds: NSRect = msg_send![view, bounds];
        if bounds.size.width < 1.0 || bounds.size.height < 1.0 {
            return nil;
        }
        let layer: id = msg_send![view, layer];
        if layer.is_null() {
            return nil;
        }
        style_drag_row_layer(layer, true);
        let _: () = msg_send![view, layoutSubtreeIfNeeded];
        let _: () = msg_send![class!(CATransaction), flush];
        let image = render_layer_preview(layer, view, bounds.size);
        style_drag_row_layer(layer, false);
        image
    }
}

fn render_layer_preview(layer: id, view: id, size: NSSize) -> id {
    let scale = backing_scale_for_view(view).max(1.0);
    let px_w = (size.width * scale).round().max(1.0) as i64;
    let px_h = (size.height * scale).round().max(1.0) as i64;
    unsafe {
        let rep: id = msg_send![class!(NSBitmapImageRep), alloc];
        let color_space = ns_string("NSCalibratedRGBColorSpace");
        let rep: id = msg_send![
            rep,
            initWithBitmapDataPlanes: nil
            pixelsWide: px_w
            pixelsHigh: px_h
            bitsPerSample: 8i64
            samplesPerPixel: 4i64
            hasAlpha: YES
            isPlanar: NO
            colorSpaceName: color_space
            bytesPerRow: 0i64
            bitsPerPixel: 0i64
        ];
        if rep.is_null() {
            return nil;
        }
        let nsctx: id = msg_send![class!(NSGraphicsContext), graphicsContextWithBitmapImageRep: rep];
        if nsctx.is_null() {
            let _: () = msg_send![rep, release];
            return nil;
        }
        let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
        let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: nsctx];
        let cg_ptr: core_graphics::sys::CGContextRef = msg_send![nsctx, CGContext];
        if cg_ptr.is_null() {
            let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];
            let _: () = msg_send![rep, release];
            return nil;
        }
        let cg = CGContext::from_existing_context_ptr(cg_ptr);
        cg.clear_rect(CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(px_w as f64, px_h as f64),
        ));
        // Bitmap and macOS CALayer are both bottom-left. Map pixels to points; do not Y-flip.
        cg.scale(scale, scale);
        let _: () = msg_send![layer, renderInContext: cg_ptr];
        drop(cg);
        let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];
        let _: () = msg_send![rep, setSize: size];
        let image: id = msg_send![class!(NSImage), alloc];
        let image: id = msg_send![image, initWithSize: size];
        if image.is_null() {
            let _: () = msg_send![rep, release];
            return nil;
        }
        let _: () = msg_send![image, addRepresentation: rep];
        let _: () = msg_send![rep, release];
        image
    }
}

fn tcc_bundle_id_is_safe(bundle_id: &str) -> bool {
    !bundle_id.is_empty()
        && bundle_id.len() <= 128
        && bundle_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

fn tccutil_reset_args(bundle_id: &str) -> Option<[&str; 3]> {
    tcc_bundle_id_is_safe(bundle_id).then_some(["reset", "Accessibility", bundle_id])
}

fn clear_stale_accessibility_row() {
    let Some(bundle_id) = get_bundle_identifier() else {
        return;
    };
    let Some(args) = tccutil_reset_args(&bundle_id) else {
        return;
    };
    match Command::new("/usr/bin/tccutil")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => debug!("cleared stale Accessibility TCC row"),
        Ok(status) => debug!(?status, "tccutil reset Accessibility returned non-zero"),
        Err(err) => debug!(%err, "tccutil reset Accessibility failed"),
    }
}

extern "C" fn source_operation_mask(_this: &Object, _sel: Sel, _session: id, _context: isize) -> usize {
    NS_DRAG_OPERATION_COPY
}

extern "C" fn drag_ended(_this: &Object, _sel: Sel, _session: id, _point: NSPoint, _op: usize) {
    finish_drag();
}

fn finish_drag() {
    DRAGGING.store(false, Ordering::SeqCst);
    if GUIDE_ACTIVE.load(Ordering::SeqCst) && accessibility_is_enabled() {
        dismiss_guide();
    }
}

extern "C" fn accepts_first_mouse(_this: &Object, _sel: Sel, _event: id) -> BOOL {
    YES
}

extern "C" fn hit_test(this: &Object, _sel: Sel, point: NSPoint) -> id {
    unsafe {
        let this_id = view_id(this);
        let superview: id = msg_send![this_id, superview];
        let local: NSPoint = msg_send![this_id, convertPoint: point fromView: superview];
        let bounds: NSRect = msg_send![this_id, bounds];
        let inside: BOOL = msg_send![this_id, mouse: local inRect: bounds];
        if inside == YES { this_id } else { nil }
    }
}

extern "C" fn drag_dealloc(this: &mut Object, _sel: Sel) {
    unsafe {
        let path: id = *this.get_ivar("bundlePath");
        if !path.is_null() {
            this.set_ivar("bundlePath", nil);
            let _: () = msg_send![path, release];
        }
        let _: () = msg_send![super(this, class!(NSView)), dealloc];
    }
}

extern "C" fn reset_cursor_rects(this: &Object, _sel: Sel) {
    unsafe {
        let this_id = view_id(this);
        let bounds: NSRect = msg_send![this_id, bounds];
        let cursor: id = msg_send![class!(NSCursor), openHandCursor];
        let _: () = msg_send![this_id, addCursorRect: bounds cursor: cursor];
    }
}

fn close_target() -> id {
    static TARGET: OnceLock<usize> = OnceLock::new();
    let ptr = *TARGET.get_or_init(|| {
        let Some(mut decl) = ClassDecl::new("ECAccessibilityGuideTarget", class!(NSObject)) else {
            return 0;
        };
        unsafe {
            decl.add_method(sel!(closeGuide:), close_guide as extern "C" fn(&Object, Sel, id));
        }
        let cls = decl.register();
        let obj: id = unsafe { msg_send![cls, new] };
        obj as usize
    });
    ptr as id
}

extern "C" fn close_guide(_this: &Object, _sel: Sel, _sender: id) {
    dismiss_guide();
}

fn schedule_tick() {
    dispatch::Queue::main().exec_after(Duration::from_millis(40), || {
        if !GUIDE_ACTIVE.load(Ordering::SeqCst) {
            return;
        }
        if DRAGGING.load(Ordering::SeqCst) {
            schedule_tick();
            return;
        }
        if accessibility_is_enabled() {
            dismiss_guide();
            return;
        }
        match settings_window_cocoa() {
            Some(settings) if !FLYING.load(Ordering::SeqCst) => {
                SETTINGS_MISSING.store(0, Ordering::SeqCst);
                let screen = screen_containing(settings.0 + settings.2 / 2.0, settings.1 + settings.3 / 2.0);
                let docked = docked_card_frame(settings, screen);
                update_arrow(card_is_left_of(settings, docked));
                let target = NSRect::new(NSPoint::new(docked.0, docked.1), NSSize::new(docked.2, docked.3));
                if let Some(panel) = panel_ptr() {
                    unsafe {
                        let current: NSRect = msg_send![panel, frame];
                        if !rects_close(current, target) {
                            let _: () = msg_send![panel, setFrame: target display: YES];
                        }
                    }
                }
            },
            Some(_) => SETTINGS_MISSING.store(0, Ordering::SeqCst),
            None => {
                let gone = SETTINGS_MISSING.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                if gone >= SETTINGS_GONE_TICKS {
                    dismiss_guide();
                    return;
                }
            },
        }
        schedule_tick();
    });
}

fn dismiss_guide() {
    GUIDE_ACTIVE.store(false, Ordering::SeqCst);
    FLYING.store(false, Ordering::SeqCst);
    DRAGGING.store(false, Ordering::SeqCst);
    SETTINGS_MISSING.store(0, Ordering::SeqCst);
    dismiss_panel_only();
}

fn dismiss_panel_only() {
    let panel = PANEL.lock().unwrap_or_else(|err| err.into_inner()).take();
    if let Some(panel) = panel {
        let panel = panel as id;
        unsafe {
            let _: () = msg_send![panel, orderOut: nil];
            let _: () = msg_send![panel, close];
            let _: () = msg_send![panel, release];
        }
    }
}

fn panel_ptr() -> Option<id> {
    (*PANEL.lock().unwrap_or_else(|err| err.into_inner())).map(|ptr| ptr as id)
}

fn mouse_location() -> NSPoint {
    unsafe { msg_send![class!(NSEvent), mouseLocation] }
}

fn on_main_thread() -> bool {
    unsafe { msg_send![class!(NSThread), isMainThread] }
}

fn prefers_zh() -> bool {
    match PREFER_ZH.load(Ordering::SeqCst) {
        0 => false,
        1 => true,
        _ => system_prefers_zh(),
    }
}

fn system_prefers_zh() -> bool {
    unsafe {
        let langs: id = msg_send![class!(NSLocale), preferredLanguages];
        if langs.is_null() {
            return false;
        }
        let count: usize = msg_send![langs, count];
        if count == 0 {
            return false;
        }
        let first: id = msg_send![langs, objectAtIndex: 0usize];
        let utf8: *const i8 = msg_send![first, UTF8String];
        if utf8.is_null() {
            return false;
        }
        CStr::from_ptr(utf8).to_string_lossy().starts_with("zh")
    }
}

fn is_dark_appearance() -> bool {
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return false;
        }
        let appearance: id = msg_send![app, effectiveAppearance];
        if appearance.is_null() {
            return false;
        }
        let name: id = msg_send![appearance, name];
        if name.is_null() {
            return false;
        }
        let utf8: *const i8 = msg_send![name, UTF8String];
        if utf8.is_null() {
            return false;
        }
        CStr::from_ptr(utf8).to_string_lossy().contains("Dark")
    }
}

fn app_display_name() -> String {
    unsafe {
        let bundle: id = msg_send![class!(NSBundle), mainBundle];
        for key in ["CFBundleDisplayName", "CFBundleName"] {
            let name: id = msg_send![bundle, objectForInfoDictionaryKey: ns_string(key)];
            if !name.is_null() {
                let utf8: *const i8 = msg_send![name, UTF8String];
                if !utf8.is_null() {
                    let value = CStr::from_ptr(utf8).to_string_lossy().into_owned();
                    if !value.is_empty() {
                        return value;
                    }
                }
            }
        }
    }
    "Easy Complete".into()
}

/// `addSubview:` retains. Drop the extra `alloc`/`new` retain so releasing
/// the panel can actually `dealloc` the card.
fn adopt_subview(parent: id, child: id) {
    if child.is_null() {
        return;
    }
    unsafe {
        let _: () = msg_send![parent, addSubview: child];
        let _: () = msg_send![child, release];
    }
}

fn ns_string(text: &str) -> id {
    unsafe {
        let string = NSString::alloc(nil).init_str(text);
        msg_send![string, autorelease]
    }
}

fn owned_ns_string(text: &str) -> id {
    unsafe { NSString::alloc(nil).init_str(text) }
}

fn settings_window_cocoa() -> Option<(f64, f64, f64, f64)> {
    let pids: Vec<i32> = ["com.apple.systempreferences", "com.apple.Settings"]
        .iter()
        .flat_map(|id| running_application_pids(id))
        .collect();
    let info = unsafe {
        CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        )
    };
    if info.is_null() {
        return None;
    }
    let windows = unsafe { core_foundation::array::CFArray::<CFDictionary>::wrap_under_create_rule(info) };
    let primary_h = primary_screen_height();
    let mut best: Option<(f64, CGRect)> = None;
    for window in windows.iter() {
        let owner = unsafe { dict_i64(&window, kCGWindowOwnerPID) }.unwrap_or(0) as i32;
        let name = unsafe { dict_string(&window, kCGWindowOwnerName) }.unwrap_or_default();
        let looks_like_settings = pids.contains(&owner)
            || name.contains("Settings")
            || name.contains("Preferences")
            || name.contains("系统设置")
            || name.contains("系统偏好");
        if !looks_like_settings {
            continue;
        }
        let Some(bounds) = (unsafe { dict_rect(&window, kCGWindowBounds) }) else {
            continue;
        };
        let area = bounds.size.width * bounds.size.height;
        if area < 20_000.0 {
            continue;
        }
        if best.is_none_or(|(best_area, _)| area > best_area) {
            best = Some((area, bounds));
        }
    }
    best.map(|(_, bounds)| quartz_to_cocoa(bounds, primary_h))
}

fn fallback_settings_frame() -> (f64, f64, f64, f64) {
    let screen = primary_screen_frame();
    (
        screen.0 + (screen.2 - 780.0).max(40.0) / 2.0,
        screen.1 + (screen.3 - 640.0).max(40.0) / 2.0,
        780.0,
        640.0,
    )
}

pub(crate) fn docked_card_frame(settings: (f64, f64, f64, f64), screen: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let right_x = settings.0 + settings.2 + CARD_GAP;
    let left_x = settings.0 - CARD_GAP - CARD_WIDTH;
    // The Accessibility app list is on the left of System Settings, so sit
    // there when there is room — dragging across the whole window is worse.
    let x = if left_x >= screen.0 + 12.0 {
        left_x
    } else if right_x + CARD_WIDTH <= screen.0 + screen.2 - 12.0 {
        right_x
    } else {
        left_x.max(screen.0 + 12.0)
    };
    let top = settings.1 + settings.3;
    let y = (top - CARD_HEIGHT - 52.0).max(screen.1 + 12.0);
    (x, y, CARD_WIDTH, CARD_HEIGHT)
}

fn card_is_left_of(settings: (f64, f64, f64, f64), docked: (f64, f64, f64, f64)) -> bool {
    docked.0 + docked.2 / 2.0 < settings.0 + settings.2 / 2.0
}

fn clamp_to_screen(x: f64, y: f64, screen: (f64, f64, f64, f64)) -> (f64, f64) {
    let max_x = (screen.0 + screen.2 - CARD_WIDTH - 8.0).max(screen.0 + 8.0);
    let max_y = (screen.1 + screen.3 - CARD_HEIGHT - 8.0).max(screen.1 + 8.0);
    (x.clamp(screen.0 + 8.0, max_x), y.clamp(screen.1 + 8.0, max_y))
}

pub(crate) fn quartz_to_cocoa(bounds: CGRect, primary_h: f64) -> (f64, f64, f64, f64) {
    (
        bounds.origin.x,
        primary_h - bounds.origin.y - bounds.size.height,
        bounds.size.width,
        bounds.size.height,
    )
}

pub(crate) fn bezier_point(start: (f64, f64), end: (f64, f64), t: f64) -> (f64, f64) {
    let t = t.clamp(0.0, 1.0);
    let control = ((start.0 + end.0) * 0.5, start.1.max(end.1) + ARC_HEIGHT);
    let mt = 1.0 - t;
    (
        mt * mt * start.0 + 2.0 * mt * t * control.0 + t * t * end.0,
        mt * mt * start.1 + 2.0 * mt * t * control.1 + t * t * end.1,
    )
}

pub(crate) fn ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `hitTest:` receives a point in the superview. Subtract the view's frame
/// origin to get the same space as `bounds`.
#[cfg(test)]
fn local_point_from_superview(point: (f64, f64), frame: (f64, f64, f64, f64)) -> (f64, f64) {
    (point.0 - frame.0, point.1 - frame.1)
}

#[cfg(test)]
fn point_in_size(point: (f64, f64), size: (f64, f64)) -> bool {
    point.0 >= 0.0 && point.0 <= size.0 && point.1 >= 0.0 && point.1 <= size.1
}

fn frames_close(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
    (a.0 - b.0).abs() < 1.0 && (a.1 - b.1).abs() < 1.0 && (a.2 - b.2).abs() < 1.0 && (a.3 - b.3).abs() < 1.0
}

fn rects_close(a: NSRect, b: NSRect) -> bool {
    (a.origin.x - b.origin.x).abs() < 1.0
        && (a.origin.y - b.origin.y).abs() < 1.0
        && (a.size.width - b.size.width).abs() < 1.0
        && (a.size.height - b.size.height).abs() < 1.0
}

fn primary_screen_height() -> f64 {
    primary_screen_frame().3
}

fn primary_screen_frame() -> (f64, f64, f64, f64) {
    screen_at(0)
}

fn screen_containing(x: f64, y: f64) -> (f64, f64, f64, f64) {
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        let count: usize = msg_send![screens, count];
        for index in 0..count {
            let screen: id = msg_send![screens, objectAtIndex: index];
            let frame: NSRect = msg_send![screen, frame];
            if x >= frame.origin.x
                && x <= frame.origin.x + frame.size.width
                && y >= frame.origin.y
                && y <= frame.origin.y + frame.size.height
            {
                return (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height);
            }
        }
    }
    primary_screen_frame()
}

fn screen_at(index: usize) -> (f64, f64, f64, f64) {
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        if screens.is_null() {
            return (0.0, 0.0, 1440.0, 900.0);
        }
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return (0.0, 0.0, 1440.0, 900.0);
        }
        let screen: id = msg_send![screens, objectAtIndex: index.min(count - 1)];
        let frame: NSRect = msg_send![screen, frame];
        (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height)
    }
}

fn dict_i64(dict: &CFDictionary, key: CFStringRef) -> Option<i64> {
    let val_ref = dict.find(key as CFTypeRef)?;
    let cf_type = unsafe { CFType::wrap_under_get_rule(*val_ref) };
    let num = cf_type.downcast::<CFNumber>()?;
    num.to_i64().or_else(|| num.to_i32().map(i64::from))
}

fn dict_string(dict: &CFDictionary, key: CFStringRef) -> Option<String> {
    let val_ref = dict.find(key as CFTypeRef)?;
    let cf_type = unsafe { CFType::wrap_under_get_rule(*val_ref) };
    let string = cf_type.downcast::<CFString>()?;
    Some(string.to_string())
}

fn dict_rect(dict: &CFDictionary, key: CFStringRef) -> Option<CGRect> {
    let val_ref = dict.find(key as CFTypeRef)?;
    let cf_type = unsafe { CFType::wrap_under_get_rule(*val_ref) };
    let bounds = cf_type.downcast::<CFDictionary>()?;
    CGRect::from_dict_representation(&bounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::geometry::{CGPoint, CGSize};

    #[test]
    fn docks_to_the_right_when_the_list_side_does_not_fit() {
        let settings = (100.0, 80.0, 700.0, 600.0);
        let screen = (0.0, 0.0, 1440.0, 900.0);
        let frame = docked_card_frame(settings, screen);
        assert!((frame.0 - (100.0 + 700.0 + CARD_GAP)).abs() < 0.1);
        assert_eq!(frame.2, CARD_WIDTH);
        assert_eq!(frame.3, CARD_HEIGHT);
        assert!(!card_is_left_of(settings, frame));
    }

    #[test]
    fn prefers_the_list_side_when_both_edges_fit() {
        let settings = (400.0, 80.0, 700.0, 600.0);
        let screen = (0.0, 0.0, 1440.0, 900.0);
        let frame = docked_card_frame(settings, screen);
        assert!(card_is_left_of(settings, frame));
        assert!((frame.0 - (400.0 - CARD_GAP - CARD_WIDTH)).abs() < 0.1);
    }

    #[test]
    fn docks_to_the_left_when_the_right_edge_overflows() {
        let settings = (1100.0, 80.0, 320.0, 600.0);
        let screen = (0.0, 0.0, 1440.0, 900.0);
        let frame = docked_card_frame(settings, screen);
        assert!(frame.0 < settings.0);
        assert!(frame.0 + frame.2 <= settings.0);
    }

    #[test]
    fn quartz_origin_flips_against_the_menu_bar_display() {
        let bounds = CGRect::new(&CGPoint::new(10.0, 20.0), &CGSize::new(100.0, 50.0));
        assert_eq!(quartz_to_cocoa(bounds, 900.0), (10.0, 830.0, 100.0, 50.0));
    }

    #[test]
    fn bezier_starts_and_ends_on_the_anchors() {
        let start = (10.0, 20.0);
        let end = (400.0, 300.0);
        assert_eq!(bezier_point(start, end, 0.0), start);
        assert_eq!(bezier_point(start, end, 1.0), end);
        let peak = bezier_point(start, end, 0.75);
        assert!(peak.1 > start.1.max(end.1));
    }

    #[test]
    fn ease_in_out_is_clamped_and_symmetric() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-9);
        assert!(ease_in_out(-1.0) == 0.0);
        assert!(ease_in_out(2.0) == 1.0);
    }

    #[test]
    fn drag_pill_right_edge_hits_after_converting_out_of_superview() {
        let frame = (20.0, 16.0, 248.0, 52.0);
        let click = (260.0, 40.0);
        assert!(!point_in_size(click, (frame.2, frame.3)));
        let local = local_point_from_superview(click, frame);
        assert!(point_in_size(local, (frame.2, frame.3)));
    }

    #[test]
    fn flight_stays_inside_a_small_display() {
        let (x, y) = clamp_to_screen(-40.0, 2000.0, (0.0, 0.0, 800.0, 500.0));
        assert!(x >= 8.0);
        assert!(x + CARD_WIDTH <= 800.0 - 8.0 + 0.1);
        assert!(y + CARD_HEIGHT <= 500.0 - 8.0 + 0.1);
    }

    #[test]
    fn grant_card_is_nonactivating_and_holds_still_during_drag() {
        assert_eq!(NS_WINDOW_STYLE_NONACTIVATING_PANEL, 1 << 7);
        let src = include_str!("accessibility_guide.rs");
        let fly = src
            .split("fn fly_step")
            .nth(1)
            .and_then(|rest| rest.split("fn present_card_at").next())
            .expect("fly_step");
        let drag = fly.find("DRAGGING").expect("fly_step must honor DRAGGING");
        let frame = fly.find("setFrame").expect("fly_step sets the frame");
        assert!(drag < frame);
        let tick = src
            .split("fn schedule_tick")
            .nth(1)
            .and_then(|rest| rest.split("fn dismiss_guide").next())
            .expect("schedule_tick");
        let drag = tick.find("DRAGGING").expect("schedule_tick must honor DRAGGING");
        let granted = tick
            .find("accessibility_is_enabled")
            .expect("schedule_tick checks grant");
        assert!(drag < granted);
        let wait = src
            .split("fn wait_for_settings")
            .nth(1)
            .and_then(|rest| rest.split("fn fly_to_docked").next())
            .expect("wait_for_settings");
        let drag = wait.find("DRAGGING").expect("wait_for_settings must honor DRAGGING");
        let granted = wait
            .find("accessibility_is_enabled")
            .expect("wait_for_settings checks grant");
        assert!(drag < granted);
    }

    #[test]
    fn tccutil_reset_requires_a_safe_bundle_id() {
        assert_eq!(
            tccutil_reset_args("dev.emmmm.easy-complete"),
            Some(["reset", "Accessibility", "dev.emmmm.easy-complete"])
        );
        assert_eq!(tccutil_reset_args(""), None);
        assert_eq!(tccutil_reset_args("foo;rm"), None);
        assert_eq!(tccutil_reset_args("a b"), None);
        assert!(tccutil_reset_args("dev.emmmm.easy-complete").unwrap().len() == 3);
    }

    #[test]
    fn grant_clears_a_stale_tcc_row_before_opening_settings() {
        let start = include_str!("accessibility_guide.rs")
            .split("fn start_guide")
            .nth(1)
            .and_then(|rest| rest.split("fn wait_for_settings").next())
            .expect("start_guide");
        let enabled = start
            .find("accessibility_is_enabled")
            .expect("start_guide bails when already granted");
        let clear = start
            .find("clear_stale_accessibility_row")
            .expect("start_guide must drop a stale list row");
        assert!(enabled < clear);
        assert!(
            start[clear..].contains("open_accessibility"),
            "reset must run before the first-start open_accessibility"
        );
    }

    #[test]
    fn drag_preview_is_the_icon_and_name_chip() {
        let drag = include_str!("accessibility_guide.rs")
            .split("fn begin_url_drag")
            .nth(1)
            .and_then(|rest| rest.split("fn style_drag_row_layer").next())
            .expect("begin_url_drag");
        assert!(drag.contains("drag_preview_image"));
        assert!(drag.contains("setDraggingFrame: bounds"));
        assert!(!drag.contains("36.0, 36.0"));
        assert_eq!(DRAG_CHIP_RADIUS, 10.0);
    }

    #[test]
    fn drag_preview_snapshots_the_live_row() {
        let src = include_str!("accessibility_guide.rs");
        let preview = src
            .split("fn drag_preview_image")
            .nth(1)
            .and_then(|rest| rest.split("fn render_layer_preview").next())
            .expect("drag_preview_image");
        let solid = preview
            .find("style_drag_row_layer(layer, true)")
            .expect("solid for snapshot");
        let flush = preview.find("CATransaction").expect("flush layer style");
        let rest = preview
            .find("style_drag_row_layer(layer, false)")
            .expect("restore rest style");
        assert!(solid < flush);
        assert!(flush < rest);
        assert!(preview.contains("flush"));
        let render = src
            .split("fn render_layer_preview")
            .nth(1)
            .and_then(|rest| rest.split("fn tcc_bundle_id_is_safe").next())
            .expect("render_layer_preview");
        assert!(render.contains("renderInContext"));
        assert!(render.contains("clear_rect"));
        assert!(!render.contains("cacheDisplayInRect"));
        assert!(!render.contains("mainScreen"));
        assert!(!render.contains("scale(1.0, -1.0)"));
        let scale = render.find("cg.scale(scale, scale)").expect("point mapping");
        let paint = render.find("renderInContext").expect("paint");
        assert!(scale < paint);
    }
}

use std::ffi::c_void;

use accessibility::util::{ax_call, bool_ax_call};
use accessibility::{AXAttribute, AXUIElement};
use accessibility_sys::{
    AXUIElementCopyParameterizedAttributeValue, AXValueCreate, AXValueGetValue, AXValueRef,
    kAXBoundsForRangeParameterizedAttribute, kAXValueTypeCFRange, kAXValueTypeCGRect,
};
use core_foundation::base::{CFRange, CFType, CFTypeRef, TCFType, TCFTypeRef};
use core_foundation::string::CFString;
use core_graphics::geometry::CGRect;
use tracing::debug;

#[derive(Debug)]
pub struct CaretPosition {
    pub valid: bool,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// AX selected-range length. Overlay usability (`length > 1` is a
    /// selection, not a caret) lives in `fig_desktop::platform::caret` so
    /// Linux CI can pin it. `valid` here is only "the AX calls succeeded".
    pub selected_length: i64,
}

const INVALID_CARET_POSITION: CaretPosition = CaretPosition {
    valid: false,
    x: 0.0,
    y: 0.0,
    width: 0.0,
    height: 0.0,
    selected_length: 0,
};

#[allow(clippy::missing_safety_doc)]
pub unsafe fn get_caret_position(extend_range: bool) -> CaretPosition {
    let system_wide_element: AXUIElement = AXUIElement::system_wide();

    // Get the focused element
    let focused_element: AXUIElement = match system_wide_element.attribute(&AXAttribute::focused_ui()) {
        Ok(focused_element) => focused_element,
        Err(err) => {
            debug!(%err, "focused UI is not available for caret tracking");

            return INVALID_CARET_POSITION;
        },
    };

    // Get the selected range value
    let selected_range_value: CFType = match focused_element.attribute(&AXAttribute::selected_range()) {
        Ok(selected_range_value) => selected_range_value,
        Err(err) => {
            debug!(%err, "selected range is not available for caret tracking");

            return INVALID_CARET_POSITION;
        },
    };

    // `ax_call` is necessary for the value ptr to actually change
    let selected_range_result: Result<CFRange, bool> = bool_ax_call(|x: *mut CFRange| {
        AXValueGetValue(
            selected_range_value.as_concrete_TypeRef() as AXValueRef,
            kAXValueTypeCFRange,
            x as *mut _ as *mut c_void,
        )
    });

    let selected_text_range: CFRange = match selected_range_result {
        Ok(selected_text_range) => selected_text_range,
        Err(err) => {
            debug!("Couldn't get selected text range, did types match {:?}", err);
            return INVALID_CARET_POSITION;
        },
    };

    let selected_range_value_2 = if extend_range {
        let updated_range = CFRange::init(selected_text_range.location, 1);
        AXValueCreate(kAXValueTypeCFRange, &updated_range as *const _ as *const c_void).as_void_ptr()
    } else {
        selected_range_value.as_concrete_TypeRef()
    };

    let select_bounds_result = ax_call(|x: *mut CFTypeRef| {
        AXUIElementCopyParameterizedAttributeValue(
            focused_element.as_concrete_TypeRef(),
            CFString::new(kAXBoundsForRangeParameterizedAttribute).as_concrete_TypeRef(),
            selected_range_value_2,
            x,
        )
    });

    let select_bounds: AXValueRef = match select_bounds_result {
        Ok(select_bounds) => select_bounds as AXValueRef,
        Err(err) => {
            debug!("Selected bounds error, error code {:?}", err);
            return INVALID_CARET_POSITION;
        },
    };

    let selected_rect_result =
        bool_ax_call(|x: *mut CGRect| AXValueGetValue(select_bounds, kAXValueTypeCGRect, x.cast()));

    let select_rect = match selected_rect_result {
        Ok(select_rect) => select_rect,
        Err(err) => {
            debug!("Couldn't get selected range, did types match {:?}", err);
            return INVALID_CARET_POSITION;
        },
    };

    // Quartz coordinates. Overlay usability (selection vs caret, 0×0 box)
    // is `macos_ax_caret_is_usable` in fig_desktop — not live AX.
    let result = CaretPosition {
        valid: true,
        x: select_rect.origin.x,
        y: select_rect.origin.y,
        width: select_rect.size.width,
        height: select_rect.size.height,
        selected_length: selected_text_range.length as i64,
    };
    debug!("Got position {result:?}");
    result
}

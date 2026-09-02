use std::ffi::c_void;

use accessibility::util::{ax_call, bool_ax_call};
use accessibility::{AXAttribute, AXUIElement};
use accessibility_sys::{
    AXUIElementCopyParameterizedAttributeValue, AXValue, AXValueCreate, AXValueGetValue, AXValueRef,
    kAXBoundsForRangeParameterizedAttribute, kAXValueTypeCFRange, kAXValueTypeCGRect,
};
use core_foundation::base::{CFRange, CFType, CFTypeRef, TCFType};
use core_foundation::string::CFString;
use core_graphics::geometry::CGRect;
use tracing::debug;

#[derive(Debug)]
pub struct CaretPosition {
    pub valid: bool,
    pub x: f64,
    pub y: f64,
    pub height: f64,
}

const INVALID_CARET_POSITION: CaretPosition = CaretPosition {
    valid: false,
    x: 0.0,
    y: 0.0,
    height: 0.0,
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

    // https://linear.app/fig/issue/ENG-109/ - autocomplete-popup-shows-when-copying-and-pasting-in-terminal
    if selected_text_range.length > 1 {
        debug!("selectedRange length > 1");
        return INVALID_CARET_POSITION;
    }

    // Owned so the `AXValueCreate` (+1) is released with the frame. Runs once per
    // keystroke in every AX terminal; unreleased it leaked one value per key.
    let extended_range: Option<AXValue> = if extend_range {
        let updated_range = CFRange::init(selected_text_range.location, 1);
        let created = AXValueCreate(kAXValueTypeCFRange, &updated_range as *const _ as *const c_void);
        if created.is_null() {
            debug!("Couldn't build the one-character range for caret bounds");
            return INVALID_CARET_POSITION;
        }
        Some(AXValue::wrap_under_create_rule(created))
    } else {
        None
    };
    let range_parameter: CFTypeRef = match &extended_range {
        Some(range) => range.as_CFTypeRef(),
        None => selected_range_value.as_concrete_TypeRef(),
    };

    let select_bounds_result = ax_call(|x: *mut CFTypeRef| {
        AXUIElementCopyParameterizedAttributeValue(
            focused_element.as_concrete_TypeRef(),
            CFString::new(kAXBoundsForRangeParameterizedAttribute).as_concrete_TypeRef(),
            range_parameter,
            x,
        )
    });

    // `Copy` rule again: take the +1 so the bounds value is released on return.
    let select_bounds: AXValue = match select_bounds_result {
        Ok(select_bounds) => AXValue::wrap_under_create_rule(select_bounds as AXValueRef),
        Err(err) => {
            debug!("Selected bounds error, error code {:?}", err);
            return INVALID_CARET_POSITION;
        },
    };

    let selected_rect_result = bool_ax_call(|x: *mut CGRect| {
        AXValueGetValue(select_bounds.as_concrete_TypeRef(), kAXValueTypeCGRect, x.cast())
    });

    let select_rect = match selected_rect_result {
        Ok(select_rect) => select_rect,
        Err(err) => {
            debug!("Couldn't get selected range, did types match {:?}", err);
            return INVALID_CARET_POSITION;
        },
    };
    // Sanity check: prevents flashing autocomplete in bottom corner
    if select_rect.size.width == 0.0 && select_rect.size.height == 0.0 {
        debug!("Prevents flashing autocomplete in bottom corner");
        return INVALID_CARET_POSITION;
    }

    // Tauri uses Quartz coordinates (don't need to convert coordinates to Cocoa like macos)
    let result = CaretPosition {
        valid: true,
        x: select_rect.origin.x,
        y: select_rect.origin.y,
        height: select_rect.size.height,
    };
    debug!("Got position {result:?}");
    result
}

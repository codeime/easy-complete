use std::ffi::c_void;
use std::fmt;

use accessibility::util::ax_call;
use accessibility_sys::{
    _AXUIElementGetWindow, AXError, AXUIElement, AXUIElementCopyAttributeNames, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementRef, AXUIElementSetAttributeValue, AXValue, kAXApplicationRole,
    kAXBrowserRole, kAXChildrenAttribute, kAXDOMClassListAttribute, kAXEnhancedUserInterfaceAttribute,
    kAXErrorAttributeUnsupported, kAXFocusedAttribute, kAXFocusedWindowAttribute, kAXFrameAttribute,
    kAXFullScreenAttribute, kAXGroupRole, kAXManualAccessibilityAttribute, kAXParentAttribute, kAXRoleAttribute,
    kAXScrollAreaRole, kAXSubroleAttribute, kAXTextFieldRole, kAXWebAreaRole, pid_t,
};
use core_foundation::ConcreteCFType;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, CFTypeRef, TCFType, TCFTypeRef};
use core_foundation::boolean::{CFBoolean, kCFBooleanTrue};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::{self, CGRect};
use core_graphics::window::{
    CGWindowID, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionAll, kCGWindowListOptionIncludingWindow, kCGWindowListOptionOnScreenOnly, kCGWindowNumber,
    kCGWindowOwnerPID,
};
use tracing::warn;

use crate::util::NSStringRef;

pub struct UIElement(AXUIElement);

impl Clone for UIElement {
    fn clone(&self) -> Self {
        UIElement::from(self.get_ref())
    }
}

impl fmt::Debug for UIElement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let position = self.frame().map(|CGRect { origin, size }| {
            format!(
                "({:.3}, {:.3}) - ({:.3}, {:.3})",
                origin.x,
                origin.y,
                origin.x + size.width,
                origin.y + size.height
            )
        });
        f.debug_struct("UIElement")
            .field("role", &self.role())
            .field("position", &position)
            .finish()
    }
}

// SAFETY: Pointer AXUIElement is send + sync safe
unsafe impl Send for UIElement {}
unsafe impl Sync for UIElement {}

impl PartialEq for UIElement {
    /// `CFEqual` is the only supported way to compare accessibility elements — two refs to the
    /// same UI element are not necessarily the same pointer.
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl From<AXUIElement> for UIElement {
    fn from(ax_ref: AXUIElement) -> Self {
        UIElement(ax_ref)
    }
}

/// Borrows a reference somebody else owns (an observer callback argument, an entry of a
/// `kAXChildren` array): retains it so the wrapper's release on drop balances out.
///
/// Do **not** feed this the result of `AXUIElementCreate*` or any `Copy*` call: those hand
/// over a +1 the caller must release, and the extra retain here would pin the element
/// forever. Wrap those with [`UIElement::application`] / [`AXUIElement::wrap_under_create_rule`].
impl From<AXUIElementRef> for UIElement {
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn from(ax_ref: AXUIElementRef) -> Self {
        UIElement(unsafe { AXUIElement::wrap_under_get_rule(ax_ref) })
    }
}

static XTERM_ROLES: &[&str] = &[
    kAXScrollAreaRole,
    kAXGroupRole,
    kAXWebAreaRole,
    kAXTextFieldRole,
    kAXApplicationRole,
    kAXBrowserRole,
];

type Result<T> = std::result::Result<T, AXError>;

#[derive(Debug)]
pub struct CGWindowInfo {
    pub window_id: CGWindowID,
    pub bounds: CGRect,
    pub owner_pid: u64,
    pub level: i64,
}

impl UIElement {
    /// The application element for `pid`, owned by the wrapper.
    pub fn application(pid: pid_t) -> Self {
        UIElement(unsafe { AXUIElement::wrap_under_create_rule(AXUIElementCreateApplication(pid)) })
    }

    pub fn get_ref(&self) -> AXUIElementRef {
        self.0.as_concrete_TypeRef()
    }

    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn get_window_id(&self) -> Result<CGWindowID> {
        // TODO: cache this value on struct?
        ax_call(|window_id: *mut CGWindowID| _AXUIElementGetWindow(self.get_ref(), window_id))
    }

    fn get_attr_ref(&self, attr: &str) -> Result<CFType> {
        unsafe {
            let cf_ref = ax_call(|value_ref: *mut CFTypeRef| {
                let attr = CFString::new(attr);
                let attr_ref = attr.as_concrete_TypeRef();
                AXUIElementCopyAttributeValue(self.get_ref(), attr_ref, value_ref)
            })?;
            // `Copy` rule: the value arrives at +1 and is ours to release. Wrapping it under
            // the *get* rule retained it again, so every attribute read pinned its value for
            // the life of the process. Reading a `kAXChildren` array pins every child with
            // it, and the xterm caret walk reads one per element per keystroke: a Cursor
            // session held 211k `AXUIElement`s (~70 MB) after a few minutes of typing.
            Ok(CFType::wrap_under_create_rule(cf_ref))
        }
    }

    fn get_attr<T: ConcreteCFType>(&self, attr: &str) -> Result<T> {
        self.get_attr_ref(attr)?.downcast::<T>().ok_or(-1)
    }

    fn set_attr<T: ConcreteCFType>(&self, attr: &str, value: T::Ref) -> Result<()> {
        unsafe {
            let attr = CFString::new(attr);
            let err = AXUIElementSetAttributeValue(self.get_ref(), attr.as_concrete_TypeRef(), value.as_void_ptr());

            if err == 0 { Ok(()) } else { Err(err) }
        }
    }

    pub fn enable_screen_reader_accessibility(&self) -> Result<()> {
        unsafe {
            let res = self.set_attr::<CFBoolean>(kAXManualAccessibilityAttribute, kCFBooleanTrue);

            if matches!(res, Err(kAXErrorAttributeUnsupported)) {
                self.set_attr::<CFBoolean>(kAXEnhancedUserInterfaceAttribute, kCFBooleanTrue)
            } else {
                res
            }
        }
    }

    pub fn is_focused(&self) -> Result<bool> {
        let focused = self.get_attr::<CFBoolean>(kAXFocusedAttribute)?;
        Ok(focused.into())
    }

    pub fn role(&self) -> Result<CFString> {
        self.get_attr::<CFString>(kAXRoleAttribute)
    }

    pub fn subrole(&self) -> Result<CFString> {
        self.get_attr::<CFString>(kAXSubroleAttribute)
    }

    pub fn parent(&self) -> Result<Self> {
        let parent = self.get_attr::<AXUIElement>(kAXParentAttribute)?;
        Ok(parent.into())
    }

    pub fn is_fullscreen(&self) -> Result<bool> {
        self.get_attr::<CFBoolean>(kAXFullScreenAttribute).map(|res| res.into())
    }

    pub fn frame(&self) -> Result<CGRect> {
        self.get_attr::<AXValue>(kAXFrameAttribute)?.as_rect().ok_or(-1)
    }

    pub fn focused_window(&self) -> Result<Self> {
        let window = self.get_attr::<AXUIElement>(kAXFocusedWindowAttribute)?;
        Ok(window.into())
    }

    pub fn dom_class_list(&self) -> Result<Vec<String>> {
        let class_list = self.get_attr::<CFArray>(kAXDOMClassListAttribute)?;
        let filtered: Vec<_> = class_list
            .iter()
            .filter_map(|attr| unsafe {
                let x = NSStringRef::new(*attr as *mut objc::runtime::Object);
                x.as_str().map(|x| x.to_owned())
            })
            .collect();

        Ok(filtered)
    }

    fn attribute_list(&self) -> Result<Vec<String>> {
        let attrs: CFArray<CFString> = unsafe {
            CFArray::wrap_under_create_rule(ax_call(|names: *mut CFArrayRef| {
                AXUIElementCopyAttributeNames(self.get_ref(), names)
            })?)
        };
        Ok(attrs.iter().map(|attr| attr.to_string()).collect())
    }

    #[allow(dead_code)]
    pub fn print_all_attribute_values(&self) {
        if let Ok(attrs) = self.attribute_list() {
            for key in attrs {
                if let Ok(value) = self.get_attr_ref(key.as_str()) {
                    let value_str = if let Some(s) = value.downcast::<CFString>() {
                        format!("{s:?}")
                    } else if let Some(cf_b) = value.downcast::<CFBoolean>() {
                        let b: bool = cf_b.into();
                        format!("{b:?}")
                    } else if let Some(ax) = value.downcast::<AXValue>() {
                        format!("{ax:?}")
                    } else {
                        format!("Unknown {{ type_id: {:?} }}", value.type_of())
                    };

                    warn!("{key}: {value_str}");
                }
            }
        }
    }

    fn children(&self) -> Result<Vec<UIElement>> {
        let children: Vec<_> = self
            .get_attr::<CFArray<*const c_void>>(kAXChildrenAttribute)?
            .iter()
            .map(|child| UIElement::from(unsafe { AXUIElementRef::from_void_ptr(*child) }))
            .collect();

        Ok(children)
    }

    pub fn find_x_term_caret_tree(&self) -> Result<Vec<UIElement>> {
        if self.role().map(|role| role == kAXTextFieldRole).unwrap_or(false) && self.is_focused().unwrap_or(false) {
            if let Ok(classes) = self.dom_class_list() {
                if classes.iter().any(|cls| cls == "xterm-helper-textarea") {
                    // self.print_all_attribute_values();
                    return Ok(vec![self.clone()]);
                }
            }
        }

        let mut children_with_cursor: Vec<_> = self
            .children()?
            .into_iter()
            .filter_map(|elem| {
                elem.role().ok().and_then(|role| {
                    let role: std::borrow::Cow<'_, str> = (&role).into();
                    if XTERM_ROLES.contains(&role.as_ref()) {
                        elem.find_x_term_caret_tree().ok()
                    } else {
                        None
                    }
                })
            })
            .collect();

        if children_with_cursor.len() > 1 {
            warn!("Found multiple candidate cursors");
        }

        let mut tree = children_with_cursor.pop().ok_or(-1)?;
        tree.push(self.clone());
        Ok(tree)
    }

    pub fn window_info(&self, all_windows: bool) -> Option<CGWindowInfo> {
        unsafe {
            let window_id = self.get_window_id().ok()?;
            let windows = if all_windows {
                CFArray::<CFDictionary>::wrap_under_create_rule(display::CGWindowListCopyWindowInfo(
                    kCGWindowListOptionAll,
                    kCGNullWindowID,
                ))
            } else {
                CFArray::<CFDictionary>::wrap_under_create_rule(display::CGWindowListCopyWindowInfo(
                    kCGWindowListOptionOnScreenOnly
                        | kCGWindowListExcludeDesktopElements
                        | kCGWindowListOptionIncludingWindow,
                    window_id,
                ))
            };

            let window = windows.iter().find(|window| {
                get_num(window, kCGWindowNumber)
                    .map(|id| id == (window_id as i64))
                    .unwrap_or(false)
            })?;

            let owner_pid = get_num(&window, kCGWindowOwnerPID)?;
            let bounds = get_value::<CFDictionary>(&window, kCGWindowBounds)?;
            let bounds_rect = CGRect::from_dict_representation(&bounds)?;
            let level = get_num(&window, kCGWindowLayer)?;

            Some(CGWindowInfo {
                owner_pid: owner_pid as u64,
                window_id,
                bounds: bounds_rect,
                level,
            })
        }
    }
}

fn get_value<T: ConcreteCFType>(dict: &CFDictionary, key: CFStringRef) -> Option<T> {
    let val_ref = dict.find(key as CFTypeRef)?;
    let cf_type = unsafe { CFType::wrap_under_get_rule(*val_ref) };
    cf_type.downcast::<T>()
}

fn get_num(dict: &CFDictionary, key: CFStringRef) -> Option<i64> {
    let num = get_value::<CFNumber>(dict, key)?;
    match num.to_i32() {
        Some(num) => Some(num as i64),
        None => num.to_i64(),
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub fn CGWindowLevelForKey(key: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AXUIElementCreateApplication` needs no Accessibility grant, so the two constructors
    /// can be checked against real retain counts: the owning one takes the +1 as is, the
    /// borrowing one adds and later removes its own.
    #[test]
    fn owned_and_borrowed_constructors_balance_their_retains() {
        let owned = UIElement::application(std::process::id() as pid_t);
        assert_eq!(owned.0.retain_count(), 1, "create-rule wrapper must not retain again");

        let borrowed = UIElement::from(owned.get_ref());
        assert_eq!(owned.0.retain_count(), 2, "borrowing wrapper must retain");
        drop(borrowed);
        assert_eq!(owned.0.retain_count(), 1, "borrowing wrapper must release on drop");

        let cloned = owned.clone();
        assert_eq!(owned.0.retain_count(), 2);
        drop(cloned);
        assert_eq!(owned.0.retain_count(), 1);
    }

    fn body_of(source: &str, signature: &str) -> String {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("`{signature}` not found"));
        let rest = &source[start..];
        let end = rest.find("\n    }\n").expect("function end");
        rest[..end].to_string()
    }

    /// Reading an attribute needs a trusted process and a live AX tree, which a test binary
    /// never has, so the ownership of `AXUIElementCopyAttributeValue`'s result is pinned at
    /// the source. It is a `Copy` call: the value is already +1, and re-retaining it leaked
    /// every child element the xterm caret walk touched — 211k of them in one session.
    #[test]
    fn attribute_reads_take_the_copied_value_as_owned() {
        for (source, signature) in [
            (include_str!("ui_element.rs"), "fn get_attr_ref("),
            (include_str!("ui_element.rs"), "fn attribute_list("),
        ] {
            let body = body_of(source, signature);
            let code: Vec<&str> = body
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect();
            assert!(
                code.iter().any(|line| line.contains("wrap_under_create_rule")),
                "{signature} must take ownership of the copied value"
            );
            assert!(
                !code.iter().any(|line| line.contains("wrap_under_get_rule")),
                "{signature} must not retain a value that is already +1"
            );
        }
    }

    /// Same rule for the caret query: both the range it builds (`AXValueCreate`) and the
    /// bounds it copies back are +1 and must be wrapped so the frame releases them.
    #[test]
    fn caret_query_releases_the_values_it_creates_and_copies() {
        let source = include_str!("../caret_position.rs");
        let code: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        assert!(
            !code.iter().any(|line| line.contains("as_void_ptr()")),
            "a created AXValue must not be passed on as a raw pointer with no owner"
        );
        assert!(
            code.iter()
                .filter(|line| line.contains("wrap_under_create_rule"))
                .count()
                >= 2,
            "both the created range and the copied bounds must be owned"
        );
    }
}

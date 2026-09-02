use std::boxed::Box;
use std::ffi::c_void;
use std::pin::Pin;

use accessibility::util::ax_call;
use accessibility_sys::{
    AXError, AXObserverAddNotification, AXObserverCallback, AXObserverCreate, AXObserverGetRunLoopSource,
    AXObserverRef, kAXErrorSuccess, pid_t,
};
use core_foundation::base::{CFRelease, TCFType};
use core_foundation::runloop::{CFRunLoop, CFRunLoopAddSource, CFRunLoopRemoveSource, kCFRunLoopDefaultMode};
use core_foundation::string::{CFString, CFStringRef};

use super::UIElement;

pub struct AXObserver<T> {
    inner: AXObserverRef,
    /// The application element the notifications are registered on. Owned here so it lives
    /// exactly as long as the subscriptions that name it.
    element: UIElement,
    /// The run loop the observer's source was scheduled on. Kept so `drop` removes the source
    /// from *that* loop even when it runs on another thread; removing from the current loop
    /// would leave the source live on the original one and the released observer with it.
    run_loop: CFRunLoop,
    callback_data: Pin<Box<T>>,
}

// SAFETY: Pointers AXObserverRef, AXUIElementRef is send + sync safe
unsafe impl<T> Send for AXObserver<T> {}
unsafe impl<T> Sync for AXObserver<T> {}

impl<T> AXObserver<T> {
    pub unsafe fn create(
        pid: pid_t,
        element: UIElement,
        data: T,
        callback: AXObserverCallback,
    ) -> Result<Self, AXError> {
        let observer = ax_call(|x: *mut AXObserverRef| AXObserverCreate(pid, callback, x))?;

        let run_loop = CFRunLoop::get_current();
        CFRunLoopAddSource(
            run_loop.as_concrete_TypeRef(),
            AXObserverGetRunLoopSource(observer),
            kCFRunLoopDefaultMode,
        );

        Ok(Self {
            inner: observer,
            element,
            run_loop,
            callback_data: Box::pin(data),
        })
    }

    pub unsafe fn subscribe(&mut self, ax_event: &str) -> Result<(), AXError> {
        // Deliberately not `ax_call`: that helper is for APIs that report their result
        // through an out-parameter, so it ends with `MaybeUninit::<V>::assume_init()`.
        // `AXObserverAddNotification` writes no such value, and asserting an initialised
        // `c_void` is undefined behaviour that optimised builds fold into an
        // unconditional `Err`, leaving every subscription looking rejected.
        let callback_data: *const T = &*self.callback_data;
        let err = AXObserverAddNotification(
            self.inner,
            self.element.get_ref(),
            CFString::from(ax_event).as_CFTypeRef() as CFStringRef,
            callback_data as *const _ as *mut c_void,
        );

        if err == kAXErrorSuccess { Ok(()) } else { Err(err) }
    }
}

impl<T> Drop for AXObserver<T> {
    fn drop(&mut self) {
        unsafe {
            CFRunLoopRemoveSource(
                self.run_loop.as_concrete_TypeRef(),
                AXObserverGetRunLoopSource(self.inner),
                kCFRunLoopDefaultMode,
            );
            // `AXObserverCreate` follows the create rule. Every app activation re-registers
            // its observer, so without this release each switch between apps leaked an
            // observer and the mach port behind it.
            CFRelease(self.inner.cast());
        }
    }
}

//! Whether the displays are asleep, from the workspace's screen sleep and wake notifications.
//!
//! Answers: are the screens asleep?
//! Must not know: what is drawn, or what the app does when the answer changes.
//!
//! AppKit does not reliably report display sleep as window occlusion (measured: one run got
//! no `Occluded` for 4 s of `pmset displaysleepnow`, another got it at once), and while the
//! displays sleep `nextDrawable` stops pacing frames, so an animation would spin unpaced.

use std::sync::atomic::{AtomicBool, Ordering};

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_app_kit::{NSWorkspace, NSWorkspaceScreensDidSleepNotification, NSWorkspaceScreensDidWakeNotification};
use objc2_foundation::NSNotification;
use winit::event_loop::EventLoopProxy;

pub struct Ivars {
    asleep: AtomicBool,
    /// Woken on each change, so the app looks again.
    proxy: EventLoopProxy,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Observer` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = Ivars]
    pub struct Observer;

    impl Observer {
        #[unsafe(method(screensDidSleep:))]
        fn screens_did_sleep(&self, _: &NSNotification) {
            self.set(true);
        }

        #[unsafe(method(screensDidWake:))]
        fn screens_did_wake(&self, _: &NSNotification) {
            self.set(false);
        }
    }
);

impl Observer {
    fn set(&self, asleep: bool) {
        self.ivars().asleep.store(asleep, Ordering::Relaxed);
        self.ivars().proxy.wake_up();
    }
}

pub struct Screens(Retained<Observer>);

impl Screens {
    /// Starts watching; `proxy` is woken whenever the screens sleep or wake.
    /// @see https://developer.apple.com/documentation/appkit/nsworkspace/screensdidsleepnotification
    pub fn watch(proxy: EventLoopProxy) -> Screens {
        let this = Observer::alloc().set_ivars(Ivars { asleep: AtomicBool::new(false), proxy });
        // SAFETY: NSObject's `init` takes no arguments and returns an initialized object.
        let observer: Retained<Observer> = unsafe { msg_send![super(this), init] };
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        // SAFETY: each selector names a method of `Observer` taking one NSNotification. Since
        // macOS 10.11 the center drops an observer once it is deallocated, so it need not be
        // removed. @see https://developer.apple.com/documentation/foundation/notificationcenter/addobserver(_:selector:name:object:)
        unsafe {
            center.addObserver_selector_name_object(
                &observer,
                sel!(screensDidSleep:),
                Some(NSWorkspaceScreensDidSleepNotification),
                None,
            );
            center.addObserver_selector_name_object(
                &observer,
                sel!(screensDidWake:),
                Some(NSWorkspaceScreensDidWakeNotification),
                None,
            );
        }
        Screens(observer)
    }

    pub fn asleep(&self) -> bool {
        self.0.ivars().asleep.load(Ordering::Relaxed)
    }
}

// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The window behind `show_window`: AppKit, reached through the Objective-C runtime. Shared by
// the JIT and the AOT runtime, so the two draw the same window. Nothing is linked: AppKit is
// loaded with `dlopen` on the first call and every Objective-C entry point is found with
// `dlsym` (loading AppKit brings the Objective-C runtime in with it), so a program that never
// opens a window starts exactly as fast as one built without this, and the linker needs no
// framework flag. Only `core` and libc are used, which is all the AOT runtime has.

use core::ffi::{c_char, c_int, c_void};

extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

const RTLD_NOW: c_int = 2;
// `RTLD_DEFAULT` on macOS: search every image already loaded.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

// An Objective-C object, class or selector.
type Id = *mut c_void;
type Selector = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Rect {
    origin: Point,
    size: Size,
}

// The Objective-C runtime entry points `show` needs.
struct Objc {
    get_class: unsafe extern "C" fn(*const c_char) -> Id,
    register_selector: unsafe extern "C" fn(*const c_char) -> Selector,
    // `objc_msgSend`. Its real signature is whatever the selector's method has, so each call
    // casts it to that: see `send!`.
    send: *const c_void,
    pool_push: unsafe extern "C" fn() -> *mut c_void,
    pool_pop: unsafe extern "C" fn(*mut c_void),
}

// Send `$selector` to `$receiver`: `[receiver selector:args]`. `[$arg_type, ..] -> $ret` is
// the method's signature, which the caller must get right: nothing checks it.
macro_rules! send {
    ($objc:expr, $receiver:expr, $selector:literal, [$($arg_type:ty),*] -> $ret:ty $(, $arg:expr)*) => {{
        let objc: &Objc = &$objc;
        let selector = (objc.register_selector)(concat!($selector, "\0").as_ptr() as *const c_char);
        let function: unsafe extern "C" fn(Id, Selector $(, $arg_type)*) -> $ret =
            core::mem::transmute(objc.send);
        function($receiver, selector $(, $arg)*)
    }};
}

unsafe fn symbol(name: &'static [u8]) -> *mut c_void {
    dlsym(RTLD_DEFAULT, name.as_ptr() as *const c_char)
}

unsafe fn load() -> Option<Objc> {
    let appkit = dlopen(
        b"/System/Library/Frameworks/AppKit.framework/AppKit\0".as_ptr() as *const c_char,
        RTLD_NOW,
    );
    if appkit.is_null() {
        return None;
    }
    let get_class = symbol(b"objc_getClass\0");
    let register_selector = symbol(b"sel_registerName\0");
    let send = symbol(b"objc_msgSend\0");
    let pool_push = symbol(b"objc_autoreleasePoolPush\0");
    let pool_pop = symbol(b"objc_autoreleasePoolPop\0");
    if get_class.is_null()
        || register_selector.is_null()
        || send.is_null()
        || pool_push.is_null()
        || pool_pop.is_null()
    {
        return None;
    }
    Some(Objc {
        get_class: core::mem::transmute(get_class),
        register_selector: core::mem::transmute(register_selector),
        send,
        pool_push: core::mem::transmute(pool_push),
        pool_pop: core::mem::transmute(pool_pop),
    })
}

unsafe fn class(objc: &Objc, name: &'static [u8]) -> Id {
    (objc.get_class)(name.as_ptr() as *const c_char)
}

// An `NSString` holding `bytes` (UTF-8), released with the enclosing autorelease pool.
unsafe fn string(objc: &Objc, bytes: &[u8]) -> Id {
    const UTF8_ENCODING: u64 = 4;
    let allocated = send!(*objc, class(objc, b"NSString\0"), "alloc", [] -> Id);
    let initialized = send!(
        *objc, allocated, "initWithBytes:length:encoding:", [*const u8, u64, u64] -> Id,
        bytes.as_ptr(), bytes.len() as u64, UTF8_ENCODING
    );
    send!(*objc, initialized, "autorelease", [] -> Id)
}

// Open a window titled `title` showing `text` centered in it, and run until it is closed.
// `false` when AppKit cannot be had: it is missing, or there is no window server to talk to
// (a session with no display answers `NSApplication` with an abort that nothing here can
// intercept, so that case is the caller's to avoid).
pub fn show_window(title: &[u8], text: &[u8]) -> bool {
    unsafe { show(title, text).is_some() }
}

unsafe fn show(title: &[u8], text: &[u8]) -> Option<()> {
    const REGULAR_ACTIVATION_POLICY: i64 = 0;
    // Titled, closable, miniaturizable and resizable.
    const WINDOW_STYLE: u64 = 1 | 2 | 4 | 8;
    const BUFFERED_BACKING: u64 = 2;
    const EVERY_EVENT: u64 = u64::MAX;

    let objc = load()?;
    let pool = (objc.pool_push)();

    let application = send!(objc, class(&objc, b"NSApplication\0"), "sharedApplication", [] -> Id);
    if application.is_null() {
        (objc.pool_pop)(pool);
        return None;
    }
    send!(objc, application, "setActivationPolicy:", [i64] -> i8, REGULAR_ACTIVATION_POLICY);

    let window = send!(objc, class(&objc, b"NSWindow\0"), "alloc", [] -> Id);
    let window = send!(
        objc, window, "initWithContentRect:styleMask:backing:defer:",
        [Rect, u64, u64, i8] -> Id,
        Rect { origin: Point { x: 0.0, y: 0.0 }, size: Size { width: 480.0, height: 240.0 } },
        WINDOW_STYLE, BUFFERED_BACKING, 0
    );
    let window = send!(objc, window, "autorelease", [] -> Id);
    // The pool releases the window; closing it must not release it a second time.
    send!(objc, window, "setReleasedWhenClosed:", [i8] -> (), 0);
    send!(objc, window, "setTitle:", [Id] -> (), string(&objc, title));

    let label = send!(
        objc, class(&objc, b"NSTextField\0"), "labelWithString:", [Id] -> Id,
        string(&objc, text)
    );
    let font = send!(objc, class(&objc, b"NSFont\0"), "systemFontOfSize:", [f64] -> Id, 32.0);
    send!(objc, label, "setFont:", [Id] -> (), font);
    send!(objc, label, "setTranslatesAutoresizingMaskIntoConstraints:", [i8] -> (), 0);
    let content = send!(objc, window, "contentView", [] -> Id);
    send!(objc, content, "addSubview:", [Id] -> (), label);
    // Centered by constraints: the label's size is the text's, which a frame would have to measure.
    let across = send!(
        objc, send!(objc, label, "centerXAnchor", [] -> Id), "constraintEqualToAnchor:",
        [Id] -> Id, send!(objc, content, "centerXAnchor", [] -> Id)
    );
    send!(objc, across, "setActive:", [i8] -> (), 1);
    let down = send!(
        objc, send!(objc, label, "centerYAnchor", [] -> Id), "constraintEqualToAnchor:",
        [Id] -> Id, send!(objc, content, "centerYAnchor", [] -> Id)
    );
    send!(objc, down, "setActive:", [i8] -> (), 1);

    send!(objc, window, "center", [] -> ());
    send!(objc, window, "makeKeyAndOrderFront:", [Id] -> (), core::ptr::null_mut());
    send!(objc, application, "activateIgnoringOtherApps:", [i8] -> (), 1);
    send!(objc, application, "finishLaunching", [] -> ());

    // `NSDefaultRunLoopMode` is a global holding the mode's name.
    let mode_symbol = symbol(b"NSDefaultRunLoopMode\0") as *const Id;
    if mode_symbol.is_null() {
        (objc.pool_pop)(pool);
        return None;
    }
    let mode = *mode_symbol;
    let forever = send!(objc, class(&objc, b"NSDate\0"), "distantFuture", [] -> Id);
    // A window that is minimized is not visible, but is not closed either.
    while send!(objc, window, "isVisible", [] -> i8) != 0
        || send!(objc, window, "isMiniaturized", [] -> i8) != 0
    {
        let event = send!(
            objc, application, "nextEventMatchingMask:untilDate:inMode:dequeue:",
            [u64, Id, Id, i8] -> Id, EVERY_EVENT, forever, mode, 1
        );
        if !event.is_null() {
            send!(objc, application, "sendEvent:", [Id] -> (), event);
        }
    }

    (objc.pool_pop)(pool);
    Some(())
}

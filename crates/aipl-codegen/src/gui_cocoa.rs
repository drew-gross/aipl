// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The windows behind `show_window` and `show_view`: AppKit, reached through the Objective-C
// runtime. Shared by the JIT and the AOT runtime, so the two draw the same window. Nothing is
// linked: AppKit is loaded with `dlopen` on the first call and every Objective-C entry point is
// found with `dlsym` (loading AppKit brings the Objective-C runtime in with it), so a program that
// never opens a window starts exactly as fast as one built without this, and the linker needs no
// framework flag. Only `core` and libc are used, which is all the AOT runtime has: nothing here
// allocates beyond one scratch buffer, so the view's text form is read as a stream.
//
// # The text form of a view
//
// What `show_view` is given: one element a line, a child indented two spaces under its parent.
//
//     column spacing=8 padding=16
//       label "Todo" size=24 bold
//       row spacing=8
//         checkbox "Buy milk" checked
//         label "(done)" muted strike
//       field placeholder="What needs doing?" width=220
//       separator
//       button "Clear done"
//
// A line is the element's name, then in any order: its text in double quotes (a label, button or
// checkbox has one), `name=number` and `name="text"` attributes, and flags. In a quoted text `\\`,
// `\"`, `\n` and `\t` stand for a backslash, a quote, a newline and a tab. The view is one `column`
// or `row` and what is under it.
//
//     column, row      children; `spacing=` between them, `padding=` around them (points)
//     label "text"     `size=` points; flags `bold`, `muted` (the secondary color), `strike`
//     button "text"    shown, and not yet answering a click
//     checkbox "text"  flag `checked`
//     field            a text field: `value="text"`, `placeholder="text"`
//     separator        a thin line across its column
//
// Any element takes `key="text"` and `width=` (points); the key is accepted and not yet used. A
// line the reader cannot make sense of stops the view, and the error says which line and why.

use core::ffi::{c_char, c_int, c_void};

extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn malloc(size: usize) -> *mut c_void;
    fn free(pointer: *mut c_void);
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

#[repr(C)]
#[derive(Clone, Copy)]
struct EdgeInsets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

// ---------- The view's text form ----------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Column,
    Row,
    Label,
    Button,
    Checkbox,
    Field,
    Separator,
}

impl Kind {
    fn is_container(self) -> bool {
        matches!(self, Kind::Column | Kind::Row)
    }

    fn has_text(self) -> bool {
        matches!(self, Kind::Label | Kind::Button | Kind::Checkbox)
    }
}

// One line of a view, read: its depth, what it is, and what it says. A text is kept as written,
// escapes and all, and `unescape` reads it where it is used.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Item<'a> {
    depth: usize,
    kind: Kind,
    text: Option<&'a [u8]>,
    value: Option<&'a [u8]>,
    placeholder: Option<&'a [u8]>,
    spacing: f64,
    padding: f64,
    size: f64,
    width: f64,
    bold: bool,
    muted: bool,
    strike: bool,
    checked: bool,
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte == b'_'
}

// The quoted text that starts at `at` (a `"`): what is between the quotes as written, and where
// the line goes on.
fn read_string(line: &[u8], at: usize) -> Result<(&[u8], usize), &'static str> {
    let mut end = at + 1;
    while end < line.len() {
        match line[end] {
            b'\\' => end += 2,
            b'"' => return Ok((&line[at + 1..end], end + 1)),
            _ => end += 1,
        }
    }
    Err("a quoted text is not closed")
}

// The whole number that starts at `at`, and where the line goes on.
fn read_number(line: &[u8], at: usize) -> Result<(f64, usize), &'static str> {
    let mut end = at;
    let mut number = 0.0;
    while end < line.len() && line[end].is_ascii_digit() {
        number = number * 10.0 + f64::from(line[end] - b'0');
        end += 1;
    }
    if end == at || end - at > 6 {
        return Err("expected a whole number of up to six digits");
    }
    Ok((number, end))
}

// A line of a view: `None` for a blank one.
fn parse_line(line: &[u8]) -> Result<Option<Item<'_>>, &'static str> {
    let mut at = 0;
    while at < line.len() && line[at] == b' ' {
        at += 1;
    }
    if at == line.len() {
        return Ok(None);
    }
    if at % 2 != 0 {
        return Err("indent each level by two spaces");
    }
    let depth = at / 2;
    let start = at;
    while at < line.len() && is_name_byte(line[at]) {
        at += 1;
    }
    let kind =
        match &line[start..at] {
            b"column" => Kind::Column,
            b"row" => Kind::Row,
            b"label" => Kind::Label,
            b"button" => Kind::Button,
            b"checkbox" => Kind::Checkbox,
            b"field" => Kind::Field,
            b"separator" => Kind::Separator,
            _ => return Err(
                "not an element: expected column, row, label, button, checkbox, field or separator",
            ),
        };
    let mut item = Item {
        depth,
        kind,
        text: None,
        value: None,
        placeholder: None,
        spacing: 8.0,
        padding: 0.0,
        size: 0.0,
        width: 0.0,
        bold: false,
        muted: false,
        strike: false,
        checked: false,
    };
    loop {
        let gap = at;
        while at < line.len() && line[at] == b' ' {
            at += 1;
        }
        if at == line.len() {
            break;
        }
        if at == gap {
            return Err("expected a space");
        }
        if line[at] == b'"' {
            let (text, next) = read_string(line, at)?;
            if !kind.has_text() || item.text.is_some() {
                return Err("this element takes no text in quotes, or only one");
            }
            item.text = Some(text);
            at = next;
            continue;
        }
        let start = at;
        while at < line.len() && is_name_byte(line[at]) {
            at += 1;
        }
        if at == start {
            return Err("unexpected character");
        }
        let name = &line[start..at];
        if at < line.len() && line[at] == b'=' {
            at += 1;
            if at < line.len() && line[at] == b'"' {
                let (text, next) = read_string(line, at)?;
                at = next;
                match name {
                    b"key" => {}
                    b"value" if kind == Kind::Field => item.value = Some(text),
                    b"placeholder" if kind == Kind::Field => item.placeholder = Some(text),
                    _ => return Err("this element has no attribute of that name with a text"),
                }
            } else {
                let (number, next) = read_number(line, at)?;
                at = next;
                match name {
                    b"spacing" if kind.is_container() => item.spacing = number,
                    b"padding" if kind.is_container() => item.padding = number,
                    b"size" if kind == Kind::Label => item.size = number,
                    b"width" => item.width = number,
                    _ => return Err("this element has no attribute of that name with a number"),
                }
            }
        } else {
            match name {
                b"bold" if kind == Kind::Label => item.bold = true,
                b"muted" if kind == Kind::Label => item.muted = true,
                b"strike" if kind == Kind::Label => item.strike = true,
                b"checked" if kind == Kind::Checkbox => item.checked = true,
                _ => return Err("this element has no flag of that name"),
            }
        }
    }
    if kind.has_text() && item.text.is_none() {
        return Err("this element needs its text in quotes");
    }
    Ok(Some(item))
}

// Each line of `view` in turn, with the number of the line an error is in.
fn parse_view<'a>(
    view: &'a [u8],
    each: &mut dyn FnMut(Item<'a>) -> Result<(), &'static str>,
) -> Result<(), (usize, &'static str)> {
    for (index, line) in view.split(|byte| *byte == b'\n').enumerate() {
        match parse_line(line) {
            Ok(Some(item)) => each(item).map_err(|why| (index + 1, why))?,
            Ok(None) => {}
            Err(why) => return Err((index + 1, why)),
        }
    }
    Ok(())
}

// A quoted text with its escapes read, into `out` (which has room: the text is never longer written
// than read), and how long it came to.
fn unescape(raw: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
    let mut length = 0;
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'\\' {
            out[length] = match raw.get(at + 1) {
                Some(b'\\') => b'\\',
                Some(b'"') => b'"',
                Some(b'n') => b'\n',
                Some(b't') => b'\t',
                _ => return Err("a backslash in a text stands for \\\\, \\\", \\n or \\t"),
            };
            at += 2;
        } else {
            out[length] = raw[at];
            at += 1;
        }
        length += 1;
    }
    Ok(length)
}

// ---------- Objective-C ----------

// The Objective-C runtime entry points the windows need.
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

// The value of a global `NSString` constant AppKit exports, such as `NSFontAttributeName`.
unsafe fn constant(name: &'static [u8]) -> Option<Id> {
    let at = symbol(name) as *const Id;
    if at.is_null() {
        None
    } else {
        Some(*at)
    }
}

// ---------- Building a view ----------

// The most containers one inside another.
const MAX_DEPTH: usize = 32;

// Makes the views a view's lines describe, a line at a time.
struct Builder<'o> {
    objc: &'o Objc,
    // Room for a text with its escapes read.
    scratch: *mut u8,
    scratch_length: usize,
    // The containers a line may go under: the one at each depth above the line last added.
    open: [Id; MAX_DEPTH],
    // How much padding each of them has, which a separator is kept inside.
    padding: [f64; MAX_DEPTH],
    count: usize,
    root: Id,
}

impl Builder<'_> {
    unsafe fn text(&self, written: &[u8]) -> Result<Id, &'static str> {
        let room = core::slice::from_raw_parts_mut(self.scratch, self.scratch_length);
        let length = unescape(written, room)?;
        Ok(string(self.objc, &room[..length]))
    }

    unsafe fn label(&self, item: &Item<'_>) -> Result<Id, &'static str> {
        let objc = self.objc;
        let text = self.text(item.text.unwrap_or(b""))?;
        let size = if item.size > 0.0 {
            item.size
        } else {
            send!(*objc, class(objc, b"NSFont\0"), "systemFontSize", [] -> f64)
        };
        let font = if item.bold {
            send!(*objc, class(objc, b"NSFont\0"), "boldSystemFontOfSize:", [f64] -> Id, size)
        } else {
            send!(*objc, class(objc, b"NSFont\0"), "systemFontOfSize:", [f64] -> Id, size)
        };
        let color = if item.muted {
            send!(*objc, class(objc, b"NSColor\0"), "secondaryLabelColor", [] -> Id)
        } else {
            send!(*objc, class(objc, b"NSColor\0"), "labelColor", [] -> Id)
        };
        // The look is the string's own attributes, so a struck-through label and a plain one are
        // made the same way.
        let attributes =
            send!(*objc, class(objc, b"NSMutableDictionary\0"), "dictionary", [] -> Id);
        if let Some(name) = constant(b"NSFontAttributeName\0") {
            send!(*objc, attributes, "setObject:forKey:", [Id, Id] -> (), font, name);
        }
        if let Some(name) = constant(b"NSForegroundColorAttributeName\0") {
            send!(*objc, attributes, "setObject:forKey:", [Id, Id] -> (), color, name);
        }
        if item.strike {
            if let Some(name) = constant(b"NSStrikethroughStyleAttributeName\0") {
                let single =
                    send!(*objc, class(objc, b"NSNumber\0"), "numberWithInt:", [i32] -> Id, 1);
                send!(*objc, attributes, "setObject:forKey:", [Id, Id] -> (), single, name);
            }
        }
        let styled = send!(*objc, class(objc, b"NSAttributedString\0"), "alloc", [] -> Id);
        let styled = send!(
            *objc, styled, "initWithString:attributes:", [Id, Id] -> Id, text, attributes
        );
        let styled = send!(*objc, styled, "autorelease", [] -> Id);
        let empty = string(objc, b"");
        let label =
            send!(*objc, class(objc, b"NSTextField\0"), "labelWithString:", [Id] -> Id, empty);
        send!(*objc, label, "setAttributedStringValue:", [Id] -> (), styled);
        Ok(label)
    }

    unsafe fn make(&self, item: &Item<'_>) -> Result<Id, &'static str> {
        let objc = self.objc;
        Ok(match item.kind {
            Kind::Column | Kind::Row => {
                let stack = send!(*objc, class(objc, b"NSStackView\0"), "alloc", [] -> Id);
                let stack = send!(*objc, stack, "init", [] -> Id);
                let stack = send!(*objc, stack, "autorelease", [] -> Id);
                let column = item.kind == Kind::Column;
                // Vertical or horizontal; children along the leading edge of a column, and
                // centered on a row's middle.
                send!(*objc, stack, "setOrientation:", [i64] -> (), i64::from(column));
                send!(*objc, stack, "setAlignment:", [i64] -> (), if column { 5 } else { 12 });
                send!(*objc, stack, "setSpacing:", [f64] -> (), item.spacing);
                let all = item.padding;
                let insets = EdgeInsets {
                    top: all,
                    left: all,
                    bottom: all,
                    right: all,
                };
                send!(*objc, stack, "setEdgeInsets:", [EdgeInsets] -> (), insets);
                stack
            }
            Kind::Label => self.label(item)?,
            Kind::Button => {
                let title = self.text(item.text.unwrap_or(b""))?;
                send!(
                    *objc, class(objc, b"NSButton\0"), "buttonWithTitle:target:action:",
                    [Id, Id, Selector] -> Id, title, core::ptr::null_mut(), core::ptr::null_mut()
                )
            }
            Kind::Checkbox => {
                let title = self.text(item.text.unwrap_or(b""))?;
                let box_ = send!(
                    *objc, class(objc, b"NSButton\0"), "checkboxWithTitle:target:action:",
                    [Id, Id, Selector] -> Id, title, core::ptr::null_mut(), core::ptr::null_mut()
                );
                if item.checked {
                    send!(*objc, box_, "setState:", [i64] -> (), 1);
                }
                box_
            }
            Kind::Field => {
                let value = self.text(item.value.unwrap_or(b""))?;
                let field = send!(
                    *objc, class(objc, b"NSTextField\0"), "textFieldWithString:", [Id] -> Id, value
                );
                if let Some(placeholder) = item.placeholder {
                    let placeholder = self.text(placeholder)?;
                    send!(*objc, field, "setPlaceholderString:", [Id] -> (), placeholder);
                }
                field
            }
            Kind::Separator => {
                let line = send!(*objc, class(objc, b"NSBox\0"), "alloc", [] -> Id);
                let line = send!(*objc, line, "init", [] -> Id);
                let line = send!(*objc, line, "autorelease", [] -> Id);
                // `NSBoxSeparator`.
                send!(*objc, line, "setBoxType:", [u64] -> (), 2);
                line
            }
        })
    }

    // Add the view the line `item` describes, under the container it is indented beneath.
    unsafe fn add(&mut self, item: &Item<'_>) -> Result<(), &'static str> {
        let objc = self.objc;
        if item.depth >= MAX_DEPTH {
            return Err("nested too deeply");
        }
        if item.depth == 0 {
            if !self.root.is_null() {
                return Err("a view has one root: indent the other lines under it");
            }
            if !item.kind.is_container() {
                return Err("a view starts with a column or a row");
            }
        } else if item.depth > self.count {
            return Err("indented deeper than a container above it");
        }
        let view = self.make(item)?;
        if item.width > 0.0 || item.kind == Kind::Separator {
            send!(*objc, view, "setTranslatesAutoresizingMaskIntoConstraints:", [i8] -> (), 0);
        }
        if item.width > 0.0 {
            let anchor = send!(*objc, view, "widthAnchor", [] -> Id);
            let fixed = send!(
                *objc, anchor, "constraintEqualToConstant:", [f64] -> Id, item.width
            );
            send!(*objc, fixed, "setActive:", [i8] -> (), 1);
        }
        if item.depth == 0 {
            self.root = view;
        } else {
            let parent = self.open[item.depth - 1];
            send!(*objc, parent, "addArrangedSubview:", [Id] -> (), view);
            if item.kind == Kind::Separator {
                // As wide as the container's inside, and a point thick.
                let across = send!(*objc, send!(*objc, view, "widthAnchor", [] -> Id),
                    "constraintEqualToAnchor:constant:", [Id, f64] -> Id,
                    send!(*objc, parent, "widthAnchor", [] -> Id),
                    -2.0 * self.padding[item.depth - 1]);
                send!(*objc, across, "setActive:", [i8] -> (), 1);
                let thin = send!(
                    *objc, send!(*objc, view, "heightAnchor", [] -> Id),
                    "constraintEqualToConstant:", [f64] -> Id, 1.0
                );
                send!(*objc, thin, "setActive:", [i8] -> (), 1);
            }
        }
        if item.kind.is_container() {
            self.open[item.depth] = view;
            self.padding[item.depth] = item.padding;
            self.count = item.depth + 1;
        } else {
            self.count = item.depth;
        }
        Ok(())
    }
}

// ---------- The window ----------

// What opening a window came to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shown {
    // The window was shown, and has been closed.
    Closed,
    // AppKit is not there to open one.
    Unavailable,
    // The view's text was not one: the line, and what is wrong with it.
    Refused { line: usize, why: &'static str },
}

// Open a window titled `title` whose contents `fill` puts in, and run until it is closed.
unsafe fn open(
    title: &[u8],
    fill: &mut dyn FnMut(&Objc, Id) -> Result<(), (usize, &'static str)>,
) -> Shown {
    const REGULAR_ACTIVATION_POLICY: i64 = 0;
    // Titled, closable, miniaturizable and resizable.
    const WINDOW_STYLE: u64 = 1 | 2 | 4 | 8;
    const BUFFERED_BACKING: u64 = 2;
    const EVERY_EVENT: u64 = u64::MAX;

    let Some(objc) = load() else {
        return Shown::Unavailable;
    };
    let pool = (objc.pool_push)();

    let application = send!(objc, class(&objc, b"NSApplication\0"), "sharedApplication", [] -> Id);
    if application.is_null() {
        (objc.pool_pop)(pool);
        return Shown::Unavailable;
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

    if let Err((line, why)) = fill(&objc, window) {
        (objc.pool_pop)(pool);
        return Shown::Refused { line, why };
    }

    send!(objc, window, "center", [] -> ());
    send!(objc, window, "makeKeyAndOrderFront:", [Id] -> (), core::ptr::null_mut());
    send!(objc, application, "activateIgnoringOtherApps:", [i8] -> (), 1);
    send!(objc, application, "finishLaunching", [] -> ());

    // `NSDefaultRunLoopMode` is a global holding the mode's name.
    let Some(mode) = constant(b"NSDefaultRunLoopMode\0") else {
        (objc.pool_pop)(pool);
        return Shown::Unavailable;
    };
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
    Shown::Closed
}

// Open a window titled `title` showing `text` centered in it, and run until it is closed. `false`
// when AppKit cannot be had: it is missing, or there is no window server to talk to (a session with
// no display answers `NSApplication` with an abort that nothing here can intercept, so that case is
// the caller's to avoid).
pub fn show_window(title: &[u8], text: &[u8]) -> bool {
    unsafe {
        let mut centered = |objc: &Objc, window: Id| -> Result<(), (usize, &'static str)> {
            let label = send!(
                *objc, class(objc, b"NSTextField\0"), "labelWithString:", [Id] -> Id,
                string(objc, text)
            );
            let font =
                send!(*objc, class(objc, b"NSFont\0"), "systemFontOfSize:", [f64] -> Id, 32.0);
            send!(*objc, label, "setFont:", [Id] -> (), font);
            send!(*objc, label, "setTranslatesAutoresizingMaskIntoConstraints:", [i8] -> (), 0);
            let content = send!(*objc, window, "contentView", [] -> Id);
            send!(*objc, content, "addSubview:", [Id] -> (), label);
            // Centered by constraints: the label's size is the text's, which a frame would have to
            // measure.
            let across = send!(
                *objc, send!(*objc, label, "centerXAnchor", [] -> Id), "constraintEqualToAnchor:",
                [Id] -> Id, send!(*objc, content, "centerXAnchor", [] -> Id)
            );
            send!(*objc, across, "setActive:", [i8] -> (), 1);
            let down = send!(
                *objc, send!(*objc, label, "centerYAnchor", [] -> Id), "constraintEqualToAnchor:",
                [Id] -> Id, send!(*objc, content, "centerYAnchor", [] -> Id)
            );
            send!(*objc, down, "setActive:", [i8] -> (), 1);
            Ok(())
        };
        open(title, &mut centered) != Shown::Unavailable
    }
}

// Open a window titled `title` showing the view `view` (its text form, described at the top of this
// file), and run until it is closed.
pub fn show_view(title: &[u8], view: &[u8]) -> Shown {
    unsafe {
        let scratch_length = view.len() + 1;
        let scratch = malloc(scratch_length) as *mut u8;
        if scratch.is_null() {
            return Shown::Unavailable;
        }
        let mut build = |objc: &Objc, window: Id| -> Result<(), (usize, &'static str)> {
            let mut builder = Builder {
                objc,
                scratch,
                scratch_length,
                open: [core::ptr::null_mut(); MAX_DEPTH],
                padding: [0.0; MAX_DEPTH],
                count: 0,
                root: core::ptr::null_mut(),
            };
            parse_view(view, &mut |item| builder.add(&item))?;
            if builder.root.is_null() {
                return Err((1, "a view is empty: it starts with a column or a row"));
            }
            send!(*objc, window, "setContentView:", [Id] -> (), builder.root);
            // The window opens as big as its contents need.
            let fitting = send!(*objc, builder.root, "fittingSize", [] -> Size);
            send!(*objc, window, "setContentSize:", [Size] -> (), fitting);
            Ok(())
        };
        let shown = open(title, &mut build);
        free(scratch as *mut c_void);
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every line of `view`, read.
    fn items(view: &str) -> Result<std::vec::Vec<Item<'_>>, (usize, &'static str)> {
        let mut read = std::vec::Vec::new();
        parse_view(view.as_bytes(), &mut |item| {
            read.push(item);
            Ok(())
        })?;
        Ok(read)
    }

    fn why(view: &str) -> (usize, &'static str) {
        items(view).expect_err(view)
    }

    #[test]
    fn a_view_is_read_a_line_at_a_time_with_its_depth() {
        let read = items(
            "column spacing=4 padding=16\n  label \"Todo\" size=24 bold\n  row\n    checkbox \"a\" checked\n",
        )
        .unwrap();
        let shape: std::vec::Vec<(usize, Kind)> = read.iter().map(|i| (i.depth, i.kind)).collect();
        assert_eq!(
            shape,
            [
                (0, Kind::Column),
                (1, Kind::Label),
                (1, Kind::Row),
                (2, Kind::Checkbox)
            ]
        );
        assert_eq!((read[0].spacing, read[0].padding), (4.0, 16.0));
        assert_eq!(read[1].text, Some(&b"Todo"[..]));
        assert_eq!(
            (read[1].size, read[1].bold, read[1].muted),
            (24.0, true, false)
        );
        assert!(read[3].checked);
        // A container with nothing said has the spacing it is given by default and no padding.
        assert_eq!((read[2].spacing, read[2].padding), (8.0, 0.0));
    }

    #[test]
    fn every_element_and_attribute_is_read() {
        let read = items(
            "column\n  label \"x\" muted strike key=\"k\"\n  button \"b\"\n  field value=\"v\" placeholder=\"p\" width=220\n  separator\n",
        )
        .unwrap();
        assert!(read[1].muted && read[1].strike);
        assert_eq!(read[2].kind, Kind::Button);
        assert_eq!(read[3].value, Some(&b"v"[..]));
        assert_eq!(read[3].placeholder, Some(&b"p"[..]));
        assert_eq!(read[3].width, 220.0);
        assert_eq!(read[4].kind, Kind::Separator);
    }

    #[test]
    fn blank_lines_are_skipped_and_errors_name_their_line() {
        assert_eq!(items("column\n\n  label \"x\"\n").unwrap().len(), 2);
        assert_eq!(why("column\n  label").0, 2);
        assert_eq!(why("column\n  frob \"x\"").0, 2);
    }

    #[test]
    fn what_cannot_be_read_is_refused_with_a_reason() {
        assert_eq!(
            why("column\n label \"x\"").1,
            "indent each level by two spaces"
        );
        assert_eq!(
            why("column\n  label \"unclosed").1,
            "a quoted text is not closed"
        );
        assert_eq!(
            why("column\n  label").1,
            "this element needs its text in quotes"
        );
        assert_eq!(
            why("column\n  separator \"x\"").1,
            "this element takes no text in quotes, or only one"
        );
        assert_eq!(
            why("column\n  label \"a\" \"b\"").1,
            "this element takes no text in quotes, or only one"
        );
        assert_eq!(
            why("column\n  label \"a\" checked").1,
            "this element has no flag of that name"
        );
        assert_eq!(
            why("column\n  label \"a\" spacing=3").1,
            "this element has no attribute of that name with a number"
        );
        assert_eq!(
            why("column\n  label \"a\" size=big").1,
            "expected a whole number of up to six digits"
        );
        assert_eq!(why("column\n  label \"a\"\"b\"").1, "expected a space");
        assert!(why("frob").1.starts_with("not an element"));
    }

    #[test]
    fn a_text_has_its_escapes_read() {
        let mut room = [0u8; 32];
        let n = unescape(br#"a \"b\" \\ \n\t"#, &mut room).unwrap();
        assert_eq!(&room[..n], b"a \"b\" \\ \n\t");
        assert!(unescape(br"bad \x", &mut room).is_err());
        assert!(unescape(br"trailing \", &mut room).is_err());
    }

    #[test]
    fn a_quote_inside_a_text_does_not_end_it() {
        let read = items("column\n  label \"say \\\"hi\\\"\" bold\n").unwrap();
        assert_eq!(read[1].text, Some(&br#"say \"hi\""#[..]));
        assert!(read[1].bold);
    }
}

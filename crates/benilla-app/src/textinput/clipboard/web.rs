//! The browser's pasteboard, wasm32 only. The page clipboard reaches us through two seams, because
//! the synchronous [`Pasteboard`] read cannot await `navigator.clipboard.readText()`, which is also
//! permission-gated:
//!
//! - Paste: the browser hands the clipboard to a trusted `paste` event without a prompt, but only
//!   when the paste chord's `keydown` is not `preventDefault`ed, and winit cancels every key on our
//!   canvas. So a capture listener takes the paste chord before winit, lets the browser fire
//!   `paste`, stores its text, and then re-dispatches the keydown to the canvas. winit and the
//!   chord table then see an ordinary paste chord whose [`Pasteboard::read_text`] finds the text
//!   already stored, whatever order Bevy's update and the DOM events run in.
//! - Copy and cut: the chord runs in the frame after the key, inside the key's user activation,
//!   so `navigator.clipboard.writeText` is allowed; its promise rejects asynchronously and is
//!   logged then.

use std::cell::{Cell, RefCell};

use bevy::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{ClipboardEvent, Element, KeyboardEvent, KeyboardEventInit};

use super::Pasteboard;

/// The canvas `boot.rs` binds the primary window to.
const CANVAS_ID: &str = "benilla";

thread_local! {
    /// The text of the last `paste` event, taken by the next paste chord's read.
    static PASTED: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Set between a held-back paste keydown and its re-dispatch, so a stray `paste` (a context
    /// menu over the page) is not stored for a later chord.
    static AWAITING: Cell<bool> = const { Cell::new(false) };
}

/// Whether the browser runs on a Mac, whose text fields use the Cmd chords.
pub(crate) fn mac_host() -> bool {
    thread_local! {
        static MAC: bool = web_sys::window()
            .and_then(|w| w.navigator().platform().ok())
            .is_some_and(|p| p.starts_with("Mac") || p.starts_with("iP"));
    }
    MAC.with(|m| *m)
}

/// The browser's clipboard, as one [`Pasteboard`]; its listeners are [`install`]ed at boot.
pub(super) struct WebPasteboard;

/// Installs the paste listeners for the whole run, before the first key can reach winit.
pub(crate) fn install() {
    let window = web_sys::window().expect("clipboard: the web build runs in a window");
    let on_key = Closure::<dyn FnMut(KeyboardEvent)>::new(hold_paste_key);
    window
        .add_event_listener_with_callback_and_bool("keydown", on_key.as_ref().unchecked_ref(), true)
        .expect("clipboard: keydown listener");
    on_key.forget();
    let on_paste = Closure::<dyn FnMut(ClipboardEvent)>::new(store_paste);
    window
        .add_event_listener_with_callback_and_bool("paste", on_paste.as_ref().unchecked_ref(), true)
        .expect("clipboard: paste listener");
    on_paste.forget();
}

impl Pasteboard for WebPasteboard {
    fn read_text(&mut self) -> Result<Option<String>, String> {
        Ok(PASTED
            .with_borrow_mut(Option::take)
            .filter(|t| !t.is_empty()))
    }

    fn write_text(&mut self, text: &str) -> Result<(), String> {
        let window = web_sys::window().ok_or("no window")?;
        thread_local! {
            static REJECTED: Closure<dyn FnMut(JsValue)> = Closure::new(|e: JsValue| {
                warn!("clipboard: browser refused the write — {e:?}");
            });
        }
        let written = window.navigator().clipboard().write_text(text);
        // A rejection is logged by the handler; the promise `catch` chains after it carries
        // nothing more.
        let _after_handler = REJECTED.with(|rejected| written.catch(rejected));
        Ok(())
    }

    fn name(&self) -> &'static str {
        "browser paste event / navigator.clipboard"
    }
}

/// The paste chords of [`super::super::keymap::chord`], read off the DOM event: Cmd+V on a Mac,
/// Ctrl+V and Shift+Insert elsewhere.
fn is_paste_chord(e: &KeyboardEvent) -> bool {
    let v = e.code() == "KeyV" || e.key().eq_ignore_ascii_case("v");
    if mac_host() {
        e.meta_key() && v
    } else {
        (e.ctrl_key() && !e.alt_key() && !e.meta_key() && v)
            || (e.shift_key() && !e.ctrl_key() && e.code() == "Insert")
    }
}

/// Window capture listener: holds a trusted paste keydown on the canvas back from winit, so the
/// browser fires `paste`, and re-sends it once that has run.
fn hold_paste_key(e: KeyboardEvent) {
    let on_canvas = e
        .target()
        .and_then(|t| t.dyn_into::<Element>().ok())
        .is_some_and(|el| el.id() == CANVAS_ID);
    if !e.is_trusted() || !on_canvas || !is_paste_chord(&e) {
        return;
    }
    e.stop_immediate_propagation();
    let Some(target) = e.target() else { return };
    PASTED.with_borrow_mut(|p| *p = None);
    AWAITING.set(true);
    let init = KeyboardEventInit::new();
    init.set_key(&e.key());
    init.set_code(&e.code());
    init.set_location(e.location());
    init.set_repeat(e.repeat());
    init.set_ctrl_key(e.ctrl_key());
    init.set_shift_key(e.shift_key());
    init.set_alt_key(e.alt_key());
    init.set_meta_key(e.meta_key());
    init.set_bubbles(true);
    init.set_cancelable(true);
    // The browser fires `paste` in this keydown's default action, before any timer runs.
    let resend = Closure::once_into_js(move || {
        AWAITING.set(false);
        let key = KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
            .expect("clipboard: keydown init");
        if let Err(e) = target.dispatch_event(&key) {
            error!("clipboard: re-sending the paste key failed — {e:?}");
        }
    });
    web_sys::window()
        .expect("clipboard: window")
        .set_timeout_with_callback_and_timeout_and_arguments_0(resend.unchecked_ref(), 0)
        .expect("clipboard: setTimeout");
}

/// Window capture listener: keeps the text of the paste a held-back chord caused.
fn store_paste(e: ClipboardEvent) {
    if !AWAITING.get() {
        return;
    }
    e.prevent_default();
    let text = e
        .clipboard_data()
        .and_then(|d| d.get_data("text/plain").ok())
        .unwrap_or_default();
    PASTED.with_borrow_mut(|p| *p = Some(text));
}

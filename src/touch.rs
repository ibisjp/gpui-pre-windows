// Added by ibis inc. (#209; not part of upstream gpui-pre-windows; Apache-2.0).
//! Touch screen fingers on Windows.
//!
//! Windows turns the first finger into mouse messages by itself (promotion: `WM_LBUTTONDOWN` etc., marked by the
//! signature in `GetMessageExtraInfo`), so a single finger already works like the mouse. This adds what the
//! promotion cannot do:
//!
//! * `WM_POINTER*` messages of touch pointers (`PT_TOUCH`) are followed through gpui's shared `PinchTracker`.
//! * A second finger takes the first finger's press back (`TouchSessionEvent` with `Cancelled`, no mouse
//!   release), sends the two fingers as `PinchEvent`s, and the promoted mouse messages of the touch are dropped
//!   until every finger has been lifted.
//! * This works the same in tablet mode: `WM_POINTER*` messages are sent for touch whatever the mode, and the
//!   promotion to the mouse does not depend on it. If a touch is not promoted at all, its session ends with the finger.
//! * A single finger is wrapped in `TouchSessionEvent` `Started` (before the promoted `WM_LBUTTONDOWN`) and
//!   `Ended` (after the promoted `WM_LBUTTONUP`).

use std::cell::{Cell, RefCell};

use gpui::*;
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::ScreenToClient,
    UI::{
        Input::{KeyboardAndMouse::ReleaseCapture, Pointer::*},
        WindowsAndMessaging::*,
    },
};

use crate::*;

/// `GetMessageExtraInfo` of a mouse message that Windows made of a pen or a touch: the signature in the high 24
/// bits, and 0x80 for a touch (https://learn.microsoft.com/windows/win32/tablet/system-events-and-mouse-messages).
const MOUSE_SIGNATURE_MASK: isize = 0xFFFF_FF00u32 as i32 as isize;
const MOUSE_SIGNATURE: isize = 0xFF51_5700u32 as i32 as isize;
const MOUSE_TOUCH_FLAG: isize = 0x80;

/// Whether the mouse message being handled was made by Windows from a touch (not a mouse or a pen).
pub(crate) fn mouse_message_is_from_touch() -> bool {
    let extra = unsafe { GetMessageExtraInfo() }.0;
    extra & MOUSE_SIGNATURE_MASK == MOUSE_SIGNATURE && extra & MOUSE_TOUCH_FLAG != 0
}

#[derive(Default)]
pub(crate) struct TouchState {
    pub pinch: RefCell<PinchTracker>,
    /// Promoted mouse messages of the touch are dropped (a pinch took the fingers over, or the touch was cancelled)
    /// until the next touch's first finger goes down or a real mouse message comes.
    pub swallow_mouse: Cell<bool>,
    /// A touch session is open: `Started` was sent and `Ended` or `Cancelled` has not been.
    pub session_open: Cell<bool>,
    /// A mouse message promoted from this touch came in (the session then ends after its release, not with the finger).
    pub mouse_seen: Cell<bool>,
    /// Where the first finger was last seen (for the `Cancelled` event).
    pub last_position: Cell<Point<Pixels>>,
}

/// One of the `WM_POINTER*` messages of a touch pointer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerStage {
    Down,
    Update,
    Up,
    /// The pointer was cancelled (`WM_POINTERCAPTURECHANGED`, or an up with the cancelled flag).
    Cancel,
}

impl WindowsWindowInner {
    fn send_input(&self, input: PlatformInput) {
        let Some(mut func) = self.state.callbacks.input.take() else {
            return;
        };
        func(input);
        self.state.callbacks.input.set(Some(func));
    }

    pub(crate) fn send_touch_session(&self, phase: TouchPhase, position: Point<Pixels>) {
        match phase {
            TouchPhase::Started => self.state.touch.session_open.set(true),
            TouchPhase::Ended | TouchPhase::Cancelled => {
                if !self.state.touch.session_open.replace(false) {
                    return;
                }
            }
            TouchPhase::Moved => return,
        }
        self.send_input(PlatformInput::TouchSession(TouchSessionEvent { phase, position }));
    }

    /// The logical window position of a pointer, `None` if Windows cannot tell.
    fn pointer_position(&self, handle: HWND, pointer_id: u32) -> Option<Point<Pixels>> {
        let mut info = POINTER_INFO::default();
        unsafe { GetPointerInfo(pointer_id, &mut info).ok()? };
        let mut at = info.ptPixelLocation;
        if !unsafe { ScreenToClient(handle, &mut at) }.as_bool() {
            return None;
        }
        Some(logical_point(at.x as f32, at.y as f32, self.state.scale_factor.get()))
    }

    /// A `WM_POINTER*` message. Returns `None` (let Windows do its default: the mouse promotion) for anything
    /// that is not a touch.
    pub(crate) fn handle_pointer_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        stage: PointerStage,
    ) -> Option<isize> {
        let pointer_id = wparam.loword() as u32;
        let mut pointer_type = POINTER_INPUT_TYPE::default();
        if unsafe { GetPointerType(pointer_id, &mut pointer_type) }.is_err() || pointer_type != PT_TOUCH {
            return None;
        }
        let id = i64::from(pointer_id);
        let touch = &self.state.touch;
        let position = self.pointer_position(handle, pointer_id).unwrap_or(touch.last_position.get());
        let stage = if stage == PointerStage::Up && wparam.hiword() as u32 & POINTER_FLAG_CANCELED.0 != 0 {
            PointerStage::Cancel
        } else {
            stage
        };
        match stage {
            PointerStage::Down => {
                let down = touch.pinch.borrow_mut().down(id, position);
                match down {
                    PinchDown::Single => {
                        touch.last_position.set(position);
                        touch.swallow_mouse.set(false);
                        touch.mouse_seen.set(false);
                        // Before the promoted WM_LBUTTONDOWN that follows when Windows handles this message.
                        self.send_touch_session(TouchPhase::Started, position);
                    }
                    PinchDown::Begin { cancelled, center } => {
                        touch.swallow_mouse.set(true);
                        // The first finger's press is taken back: no mouse release comes.
                        self.send_touch_session(TouchPhase::Cancelled, cancelled.1);
                        unsafe { ReleaseCapture().ok() };
                        self.send_input(PlatformInput::Pinch(PinchEvent {
                            position: center,
                            delta: 0.0,
                            modifiers: current_modifiers(),
                            phase: TouchPhase::Started,
                        }));
                        // The second finger is not promoted to the mouse here (it is not the primary pointer).
                    }
                    PinchDown::Ignored => {}
                }
            }
            PointerStage::Update => {
                let moved = touch.pinch.borrow_mut().moved(id, position);
                match moved {
                    PinchMove::Single => touch.last_position.set(position),
                    PinchMove::Update { center, delta } => {
                        self.send_input(PlatformInput::Pinch(PinchEvent {
                            position: center,
                            delta,
                            modifiers: current_modifiers(),
                            phase: TouchPhase::Moved,
                        }));
                    }
                    PinchMove::Ignored => {}
                }
            }
            PointerStage::Up | PointerStage::Cancel => {
                let up = touch.pinch.borrow_mut().up(id, position);
                match up {
                    PinchUp::Single => {
                        if stage == PointerStage::Cancel {
                            // The system took the touch: nothing was released, the application undoes the press.
                            self.send_touch_session(TouchPhase::Cancelled, position);
                            unsafe { ReleaseCapture().ok() };
                            touch.swallow_mouse.set(true);
                        }
                        // Up: with promoted mouse input the session ends after its WM_LBUTTONUP
                        // (`touch_mouse_released`); with none (nothing was made of the touch) it ends with the finger.
                        if stage == PointerStage::Up && !touch.mouse_seen.get() {
                            self.send_touch_session(TouchPhase::Ended, position);
                        }
                    }
                    PinchUp::End { center } => {
                        self.send_input(PlatformInput::Pinch(PinchEvent {
                            position: center,
                            delta: 0.0,
                            modifiers: current_modifiers(),
                            phase: TouchPhase::Ended,
                        }));
                    }
                    PinchUp::Ignored => {}
                }
            }
        }
        None
    }
}

impl WindowsWindowInner {
    /// Whether the mouse message being handled is to be dropped: it was promoted from a touch that a pinch (or
    /// the system) took over. A real mouse message ends the dropping.
    pub(crate) fn touch_mouse_swallowed(&self) -> bool {
        let touch = &self.state.touch;
        if !touch.swallow_mouse.get() {
            if touch.session_open.get() && mouse_message_is_from_touch() {
                touch.mouse_seen.set(true);
            }
            return false;
        }
        if mouse_message_is_from_touch() {
            return true;
        }
        touch.swallow_mouse.set(false);
        false
    }

    /// After the promoted `WM_LBUTTONUP` of a single finger was handled: the touch is over.
    pub(crate) fn touch_mouse_released(&self, position: Point<Pixels>) {
        if self.state.touch.session_open.get() && mouse_message_is_from_touch() {
            self.send_touch_session(TouchPhase::Ended, position);
        }
    }
}

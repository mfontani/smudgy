//! Native window chrome for the borderless Windows main window: a
//! `WM_NCHITTEST` subclass that tells the OS which parts of the client area
//! are the caption and the resize borders.
//!
//! The toolbar acts as the titlebar and thin edge strips act as resize
//! grips. Routing them through iced (`window::drag` / `window::drag_resize`)
//! rides winit's synthesized `WM_NCLBUTTONDOWN`, which is guarded by a latch
//! (`window_state.dragging`) that only `WM_EXITSIZEMOVE` clears. Any drag
//! request the OS declines to turn into a modal move/size loop — a touch or
//! pen press (no mouse button is down), a press already released by the time
//! the posted message is pumped — leaves the latch set forever, and every
//! later move/resize request returns as a silent no-op (winit #2999: "you
//! can drag with a touch and thus button event doesn't even make sense").
//!
//! Answering `WM_NCHITTEST` instead hands the gesture to `DefWindowProc`
//! before iced ever sees a press: moves, edge resizes, Aero Snap, native
//! double-click maximize (`WM_NCLBUTTONDBLCLK`), drag-to-restore of a
//! maximized window, the caption's right-click system menu, and touch/pen
//! drags (`WM_NCPOINTERDOWN`) all behave natively, and the latch path is
//! never entered.
//!
//! Geometry flows in from the iced side: the toolbar's drag strip reports
//! its laid-out bounds every draw (`BoundsProbe`, at most one frame stale —
//! the `pane_drag` mirror pattern), and the window's maximize/fullscreen
//! mirrors gate the zones exactly as the Linux grips are gated (no borders
//! while maximized, nothing while fullscreen). Bounds are stored logical
//! and compared physical against the window's live DPI, so per-monitor
//! scale changes need no replumbing.
//!
//! Windows-only; the stubs keep call sites unconditional elsewhere.

#[cfg(windows)]
pub use imp::{hook_window, set_caption_bounds, set_fullscreen, set_maximized};

#[cfg(not(windows))]
pub use stub::{hook_window, set_caption_bounds, set_fullscreen, set_maximized};

#[cfg(not(windows))]
mod stub {
    use iced::{Rectangle, window};

    pub fn hook_window(_id: window::Id, _raw_id: u64) {}
    pub fn set_caption_bounds(_id: window::Id, _bounds: Rectangle) {}
    pub fn set_maximized(_id: window::Id, _maximized: bool) {}
    pub fn set_fullscreen(_id: window::Id, _fullscreen: bool) {}
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    use iced::{Point, Rectangle, window};
    use winapi::shared::basetsd::{DWORD_PTR, UINT_PTR};
    use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
    use winapi::shared::windef::{HWND, POINT, RECT};
    use winapi::um::commctrl::{DefSubclassProc, SetWindowSubclass};
    use winapi::um::winuser::{
        GetClientRect, GetDpiForWindow, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTLEFT,
        HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, ScreenToClient, WM_NCDESTROY, WM_NCHITTEST,
    };

    use crate::components::resize_grips::{CORNER, GRIP};

    /// This module's subclass identity (`win_rm` holds id 1).
    const SUBCLASS_ID: UINT_PTR = 2;

    /// One window's chrome geometry, in logical pixels / mirror state.
    #[derive(Debug, Clone, Copy, Default)]
    struct Chrome {
        caption: Rectangle,
        maximized: bool,
        fullscreen: bool,
    }

    #[derive(Default)]
    struct Registry {
        // Keyed by iced id so the mirrors can seed state (a restored
        // maximized window) before the async raw-id round trip links the
        // HWND.
        by_id: HashMap<window::Id, Chrome>,
        hwnd_to_id: HashMap<usize, window::Id>,
    }

    static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

    /// Link a main window's HWND to its iced id and install the hit-test
    /// subclass. Idempotent per window (the subclass identity is fixed).
    pub fn hook_window(id: window::Id, raw_id: u64) {
        let hwnd = raw_id as usize;
        {
            let mut registry = REGISTRY.lock().unwrap();
            registry.hwnd_to_id.insert(hwnd, id);
            registry.by_id.entry(id).or_default();
        }
        // SAFETY: `raw_id` is the HWND iced/winit reported for a live window.
        // SetWindowSubclass chains our proc ahead of winit's without
        // disturbing it — messages we do not handle fall through via
        // DefSubclassProc.
        unsafe {
            let _ = SetWindowSubclass(hwnd as HWND, Some(subclass_proc), SUBCLASS_ID, 0);
        }
    }

    /// Record the toolbar drag strip's laid-out bounds (logical window
    /// coordinates), called from its `BoundsProbe` every draw.
    pub fn set_caption_bounds(id: window::Id, bounds: Rectangle) {
        REGISTRY
            .lock()
            .unwrap()
            .by_id
            .entry(id)
            .or_default()
            .caption = bounds;
    }

    /// Mirror the window's maximized state (border zones are suppressed
    /// while maximized, exactly like the Linux grips).
    pub fn set_maximized(id: window::Id, maximized: bool) {
        REGISTRY
            .lock()
            .unwrap()
            .by_id
            .entry(id)
            .or_default()
            .maximized = maximized;
    }

    /// Mirror the window's fullscreen state (no chrome at all while
    /// fullscreen).
    pub fn set_fullscreen(id: window::Id, fullscreen: bool) {
        REGISTRY
            .lock()
            .unwrap()
            .by_id
            .entry(id)
            .or_default()
            .fullscreen = fullscreen;
    }

    /// Classify a `WM_NCHITTEST` point, or `None` to fall through to the
    /// subclass chain (which resolves to `HTCLIENT` for the client area).
    fn hit_test(hwnd: HWND, lparam: LPARAM) -> Option<LRESULT> {
        let chrome = {
            let registry = REGISTRY.lock().unwrap();
            let id = registry.hwnd_to_id.get(&(hwnd as usize))?;
            *registry.by_id.get(id)?
        };
        if chrome.fullscreen {
            return None;
        }

        // Screen coordinates, sign-extended (negative on mixed-monitor
        // layouts left/above the primary).
        let sx = (lparam & 0xFFFF) as u16 as i16 as i32;
        let sy = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
        let mut pt = POINT { x: sx, y: sy };
        // SAFETY: hwnd is live (subclass callbacks stop at WM_NCDESTROY);
        // both calls write only into the locals passed to them.
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            if ScreenToClient(hwnd, &mut pt) == 0 || GetClientRect(hwnd, &mut rc) == 0 {
                return None;
            }
        }
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };

        // Border zones mirror the Linux grips' geometry. The point can sit
        // just outside the client rect (winit's 1px WM_NCCALCSIZE top
        // tweak); the signed comparisons fold that into the top band.
        if !chrome.maximized {
            let grip = ((GRIP * scale).round() as i32).max(1);
            let corner = (CORNER * scale).round() as i32;
            let (w, h) = (rc.right, rc.bottom);
            let (near_left, near_right) = (pt.x < corner, pt.x >= w - corner);
            let (near_top, near_bottom) = (pt.y < corner, pt.y >= h - corner);
            if pt.y < grip {
                return Some(if near_left {
                    HTTOPLEFT
                } else if near_right {
                    HTTOPRIGHT
                } else {
                    HTTOP
                });
            }
            if pt.y >= h - grip {
                return Some(if near_left {
                    HTBOTTOMLEFT
                } else if near_right {
                    HTBOTTOMRIGHT
                } else {
                    HTBOTTOM
                });
            }
            if pt.x < grip {
                return Some(if near_top {
                    HTTOPLEFT
                } else if near_bottom {
                    HTBOTTOMLEFT
                } else {
                    HTLEFT
                });
            }
            if pt.x >= w - grip {
                return Some(if near_top {
                    HTTOPRIGHT
                } else if near_bottom {
                    HTBOTTOMRIGHT
                } else {
                    HTRIGHT
                });
            }
        }

        let logical = Point::new(pt.x as f32 / scale, pt.y as f32 / scale);
        // Caption drags stay available while maximized: DefWindowProc
        // restores the window and carries the drag (native titlebar
        // behavior the client-side path never had).
        if chrome.caption.contains(logical) {
            return Some(HTCAPTION);
        }
        None
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        msg: UINT,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: UINT_PTR,
        _ref_data: DWORD_PTR,
    ) -> LRESULT {
        match msg {
            WM_NCHITTEST => {
                if let Some(code) = hit_test(hwnd, lparam) {
                    return code;
                }
            }
            WM_NCDESTROY => {
                let mut registry = REGISTRY.lock().unwrap();
                if let Some(id) = registry.hwnd_to_id.remove(&(hwnd as usize)) {
                    registry.by_id.remove(&id);
                }
            }
            _ => {}
        }
        // SAFETY: forwarding the same window-proc arguments to the next
        // handler in the subclass chain, as required for messages we do not
        // consume.
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }
}

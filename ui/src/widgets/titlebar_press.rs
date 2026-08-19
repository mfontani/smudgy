//! The titlebar's drag surface: press-and-move starts an OS window drag, a
//! double press toggles maximize — never both from one gesture.
//!
//! `mouse_area` publishes `on_press` for every press, including the second
//! press of a double-click, so wiring window-drag to `on_press` and maximize
//! to `on_double_click` fires both from one physical press: the OS begins a
//! caption drag under a window that is simultaneously maximizing. This
//! surface disambiguates the way native titlebars do:
//!
//! - A press **arms** at its press point; nothing is published yet.
//! - Motion past [`DRAG_DEADBAND`] hands the window to the OS drag — and
//!   breaks the click chain, so grabbing the window again right after a drag
//!   can never read as a double-click (the press point is window-relative,
//!   which a completed drag leaves unchanged).
//! - A release inside the deadband is a plain click; a second press within
//!   double-click range then toggles maximize instead of arming a drag.
//!
//! Deferring the drag to the deadband crossing also means a bare click on
//! empty titlebar no longer starts (and instantly abandons) an OS move loop.
//!
//! The main window's toolbar uses this on macOS and Linux, where window
//! moves go through `window::drag`. On Windows the strip is inert: the
//! `WM_NCHITTEST` chrome (`win_chrome`) answers caption hit-tests before
//! iced ever sees a press there.

use iced::advanced::widget::tree;
// `iced::mouse` is the trimmed public facade; `Click` (press-chain
// disambiguation) only ships in the advanced re-export.
use iced::advanced::{Widget, layout, mouse};
use iced::{Event as IcedEvent, Length, Point, Rectangle, Size, touch, window};

use crate::pane_drag::DRAG_DEADBAND;

/// The press surface. A leaf widget (renders nothing), sized like the
/// `Space` it replaces.
pub struct TitlebarPress<Message> {
    width: Length,
    on_drag: Message,
    on_double: Message,
}

impl<Message> TitlebarPress<Message> {
    pub fn new(on_drag: Message, on_double: Message) -> Self {
        Self {
            width: Length::Fill,
            on_drag,
            on_double,
        }
    }

    /// Fixed width instead of the default fill (the macOS traffic-light
    /// inset).
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }
}

#[derive(Debug, Clone, Copy, Default)]
enum Phase {
    #[default]
    Idle,
    /// A press landed on the surface; the gesture is a click until motion
    /// crosses the deadband.
    Armed { at: Point },
}

#[derive(Default)]
struct State {
    phase: Phase,
    previous_click: Option<mouse::Click>,
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for TitlebarPress<Message>
where
    Message: Clone,
    Renderer: iced::advanced::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: Length::Fill,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut tree::Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(self.width, Length::Fill, Size::ZERO))
    }

    fn update(
        &mut self,
        tree: &mut tree::Tree,
        event: &IcedEvent,
        layout: layout::Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn iced::advanced::Clipboard,
        shell: &mut iced::advanced::Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        match event {
            IcedEvent::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | IcedEvent::Touch(touch::Event::FingerPressed { .. }) => {
                let Some(position) = cursor.position_over(layout.bounds()) else {
                    return;
                };
                let click = mouse::Click::new(position, mouse::Button::Left, state.previous_click);
                state.previous_click = Some(click);
                if click.kind() == mouse::click::Kind::Double {
                    shell.publish(self.on_double.clone());
                    state.phase = Phase::Idle;
                } else {
                    // Single (or a post-double third) press: arm a drag
                    // candidate at its true press point.
                    state.phase = Phase::Armed { at: position };
                }
                shell.capture_event();
            }
            IcedEvent::Mouse(mouse::Event::CursorMoved { .. })
            | IcedEvent::Touch(touch::Event::FingerMoved { .. }) => {
                let Phase::Armed { at } = state.phase else {
                    return;
                };
                // The press has the implicit capture, so positions keep
                // arriving even outside the surface's bounds.
                let Some(position) = cursor.position() else {
                    return;
                };
                if position.distance(at) > DRAG_DEADBAND {
                    state.phase = Phase::Idle;
                    // The gesture became a drag: the next grab must start a
                    // fresh click chain, never read as a double-click.
                    state.previous_click = None;
                    shell.publish(self.on_drag.clone());
                    shell.capture_event();
                }
            }
            IcedEvent::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | IcedEvent::Touch(touch::Event::FingerLifted { .. })
            | IcedEvent::Touch(touch::Event::FingerLost { .. }) => {
                // Release inside the deadband: a plain click. The click chain
                // stands, so a quick second press reads as a double.
                state.phase = Phase::Idle;
            }
            // Focus loss with a press armed (the OS took the gesture, or the
            // user alt-tabbed mid-press): stand down.
            IcedEvent::Window(window::Event::Unfocused) => {
                state.phase = Phase::Idle;
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        _tree: &tree::Tree,
        _renderer: &mut Renderer,
        _theme: &Theme,
        _style: &iced::advanced::renderer::Style,
        _layout: layout::Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
    }
}

impl<'a, Message, Theme, Renderer> From<TitlebarPress<Message>>
    for iced::Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(surface: TitlebarPress<Message>) -> Self {
        iced::Element::new(surface)
    }
}

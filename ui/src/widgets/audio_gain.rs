//! Focusable keyboard surface for one visible audio gain row.
//!
//! iced 0.14's stock sliders/buttons do not participate in the focusable
//! operation tree. This wrapper keeps normal visible content while exposing a
//! real [`Focusable`](iced::advanced::widget::operation::Focusable) state.

use iced::advanced::widget::{Operation, Tree, operation::Focusable, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay};
use iced::keyboard::{Key, key::Named};
use iced::widget::Id;
use iced::{Element, Event, Length, Rectangle, Size};

/// Semantic action emitted by a focused audio row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Focus,
    SetVolume(u8),
    ToggleMuted,
}

/// Fixed keyboard step in whole persisted percentage points.
pub const VOLUME_STEP: u8 = 5;

fn key_action(key: &Key, volume: u8) -> Option<Action> {
    match key {
        Key::Named(Named::ArrowLeft | Named::ArrowDown) => {
            Some(Action::SetVolume(volume.saturating_sub(VOLUME_STEP)))
        }
        Key::Named(Named::ArrowRight | Named::ArrowUp) => Some(Action::SetVolume(
            volume.saturating_add(VOLUME_STEP).min(100),
        )),
        Key::Named(Named::Home) => Some(Action::SetVolume(0)),
        Key::Named(Named::End) => Some(Action::SetVolume(100)),
        Key::Named(Named::Space) => Some(Action::ToggleMuted),
        // Some platform IMEs surface Space as committed text instead of a
        // named key; retain the fallback without relying on it.
        Key::Character(value) if value.as_str() == " " => Some(Action::ToggleMuted),
        _ => None,
    }
}

#[derive(Default)]
struct State {
    focused: bool,
}

impl Focusable for State {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn focus(&mut self) {
        self.focused = true;
    }

    fn unfocus(&mut self) {
        self.focused = false;
    }
}

/// A focusable wrapper over already-renderable visible row content.
pub struct AudioGain<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    id: Id,
    volume: u8,
    focus_color: iced::Color,
    on_action: Box<dyn Fn(Action) -> Message + 'a>,
}

impl<'a, Message, Theme, Renderer> AudioGain<'a, Message, Theme, Renderer> {
    pub fn new(
        content: impl Into<Element<'a, Message, Theme, Renderer>>,
        id: Id,
        volume: u8,
        on_action: impl Fn(Action) -> Message + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            id,
            volume: volume.min(100),
            focus_color: iced::Color::from_rgb(0.25, 0.55, 0.95),
            on_action: Box::new(on_action),
        }
    }

    pub fn focus_color(mut self, color: iced::Color) -> Self {
        self.focus_color = color;
        self
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for AudioGain<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.focusable(
            Some(&self.id),
            layout.bounds(),
            tree.state.downcast_mut::<State>(),
        );
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                operation,
            );
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        let state = tree.state.downcast_mut::<State>();
        match event {
            Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left))
                if cursor.is_over(layout.bounds()) =>
            {
                shell.publish((self.on_action)(Action::Focus));
                shell.capture_event();
            }
            Event::Keyboard(iced::keyboard::Event::KeyPressed { key, repeat, .. })
                if state.focused =>
            {
                // Tab is deliberately absent: the daemon's window-scoped
                // listener runs iced focus_next/focus_previous.
                if let Some(action) = key_action(key, self.volume)
                    && !(*repeat && action == Action::ToggleMuted)
                {
                    shell.publish((self.on_action)(action));
                    shell.capture_event();
                }
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &iced::advanced::renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        if tree.state.downcast_ref::<State>().focused {
            renderer.fill_quad(
                iced::advanced::renderer::Quad {
                    bounds: layout.bounds(),
                    border: iced::Border {
                        color: self.focus_color,
                        width: 2.0,
                        radius: 4.0.into(),
                    },
                    ..Default::default()
                },
                iced::Color::TRANSPARENT,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Pointer
        } else {
            self.content.as_widget().mouse_interaction(
                &tree.children[0],
                layout,
                cursor,
                viewport,
                renderer,
            )
        }
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: iced::Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<AudioGain<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: AudioGain<'a, Message, Theme, Renderer>) -> Self {
        Self::new(widget)
    }
}

/// Whole-main-window event owner for the reserved audio keyboard chord
/// (command+shift+A, which opens the Settings window's Audio pane). It sees
/// the chord before stock text inputs can interpret command+A and reserves it
/// before children; plain Tab remains a post-child ignored-event route so
/// focused inputs keep their own Tab behavior.
pub struct AudioPanelRoot<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    on_open: Box<dyn Fn() -> Message + 'a>,
}

impl<'a, Message, Theme, Renderer> AudioPanelRoot<'a, Message, Theme, Renderer> {
    pub fn new(
        content: impl Into<Element<'a, Message, Theme, Renderer>>,
        on_open: impl Fn() -> Message + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            on_open: Box::new(on_open),
        }
    }
}

fn is_panel_shortcut(key: &Key, modifiers: iced::keyboard::Modifiers) -> bool {
    modifiers == (iced::keyboard::Modifiers::COMMAND | iced::keyboard::Modifiers::SHIFT)
        && matches!(key, Key::Character(value) if value.eq_ignore_ascii_case("a"))
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for AudioPanelRoot<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key,
            modifiers,
            repeat,
            ..
        }) = event
            && is_panel_shortcut(key, *modifiers)
        {
            if !repeat {
                shell.publish((self.on_open)());
            }
            shell.capture_event();
            return;
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &iced::advanced::renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: iced::Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<AudioPanelRoot<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(widget: AudioPanelRoot<'a, Message, Theme, Renderer>) -> Self {
        Self::new(widget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_contract_is_bounded_and_keeps_mute_independent() {
        assert_eq!(
            key_action(&Key::Named(Named::ArrowLeft), 3),
            Some(Action::SetVolume(0))
        );
        assert_eq!(
            key_action(&Key::Named(Named::ArrowDown), 55),
            Some(Action::SetVolume(50))
        );
        assert_eq!(
            key_action(&Key::Named(Named::ArrowRight), 98),
            Some(Action::SetVolume(100))
        );
        assert_eq!(
            key_action(&Key::Named(Named::Home), 50),
            Some(Action::SetVolume(0))
        );
        assert_eq!(
            key_action(&Key::Named(Named::End), 50),
            Some(Action::SetVolume(100))
        );
        assert_eq!(
            key_action(&Key::Named(Named::Space), 50),
            Some(Action::ToggleMuted)
        );
        assert_eq!(key_action(&Key::Named(Named::Tab), 50), None);
    }

    #[test]
    fn focus_state_is_a_real_focusable() {
        let mut state = State::default();
        assert!(!state.is_focused());
        state.focus();
        assert!(state.is_focused());
        state.unfocus();
        assert!(!state.is_focused());
    }
}

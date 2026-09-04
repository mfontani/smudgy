//! A feature-local focus surface for the trigger editor's color controls.
//!
//! iced's stock buttons and checkboxes retain their pointer behavior as
//! children. This wrapper contributes one stable focus stop, forwards the
//! complete child widget contract, and owns the composite control's keyboard
//! behavior and visible focus outline.

use iced::advanced::widget::{Operation, Tree, operation::Focusable, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay};
use iced::keyboard::{Key, key::Named};
use iced::widget::Id;
use iced::{Element, Event, Length, Rectangle, Size};

/// Result of offering a focused control one key press.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum KeyAction<Message> {
    /// Leave the event available to the window-level keyboard owner.
    Ignored,
    /// Consume the event without changing the model.
    Captured,
    /// Consume the event and publish an existing editor message.
    Publish(Message),
}

/// A bounded selection change requested by a navigation key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectionMove {
    Ignored,
    Captured,
    Select(usize),
}

/// Maps Space/Enter to one activation and consumes their key-repeat events.
pub(super) fn activation<Message>(key: &Key, repeat: bool, message: Message) -> KeyAction<Message> {
    let activates = matches!(key, Key::Named(Named::Space | Named::Enter))
        || matches!(key, Key::Character(value) if value.as_str() == " ");
    if !activates {
        KeyAction::Ignored
    } else if repeat {
        KeyAction::Captured
    } else {
        KeyAction::Publish(message)
    }
}

/// Moves within a one-dimensional group without wrapping at either edge.
pub(super) fn linear_selection(key: &Key, current: usize, len: usize) -> SelectionMove {
    if len == 0 || current >= len {
        return SelectionMove::Ignored;
    }
    let target = match key {
        Key::Named(Named::ArrowLeft | Named::ArrowUp) => current.checked_sub(1),
        Key::Named(Named::ArrowRight | Named::ArrowDown) => {
            (current + 1 < len).then_some(current + 1)
        }
        Key::Named(Named::Home) => Some(0),
        Key::Named(Named::End) => Some(len - 1),
        _ => return SelectionMove::Ignored,
    };
    target.map_or(SelectionMove::Captured, |target| {
        if target == current {
            SelectionMove::Captured
        } else {
            SelectionMove::Select(target)
        }
    })
}

/// Moves within a row-major grid without wrapping across a row or outer edge.
pub(super) fn grid_selection(
    key: &Key,
    current: usize,
    columns: usize,
    len: usize,
) -> SelectionMove {
    if columns == 0 || len == 0 || current >= len {
        return SelectionMove::Ignored;
    }
    let target = match key {
        Key::Named(Named::ArrowLeft) => (!current.is_multiple_of(columns)).then_some(current - 1),
        Key::Named(Named::ArrowRight) => {
            (current % columns + 1 < columns && current + 1 < len).then_some(current + 1)
        }
        Key::Named(Named::ArrowUp) => current.checked_sub(columns),
        Key::Named(Named::ArrowDown) => (current + columns < len).then_some(current + columns),
        Key::Named(Named::Home) => Some(0),
        Key::Named(Named::End) => Some(len - 1),
        _ => return SelectionMove::Ignored,
    };
    target.map_or(SelectionMove::Captured, |target| {
        if target == current {
            SelectionMove::Captured
        } else {
            SelectionMove::Select(target)
        }
    })
}

/// Converts a selection result into the editor message for its destination.
pub(super) fn publish_selection<Message>(
    selection: SelectionMove,
    message: impl FnOnce(usize) -> Message,
) -> KeyAction<Message> {
    match selection {
        SelectionMove::Ignored => KeyAction::Ignored,
        SelectionMove::Captured => KeyAction::Captured,
        SelectionMove::Select(index) => KeyAction::Publish(message(index)),
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

type KeyHandler<'a, Message> = dyn Fn(&Key, bool) -> KeyAction<Message> + 'a;

/// A single focus stop over already-renderable child content.
pub(super) struct KeyboardControl<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    id: Id,
    focus_color: iced::Color,
    on_focus: Box<dyn Fn() -> Message + 'a>,
    on_key: Box<KeyHandler<'a, Message>>,
}

impl<'a, Message, Theme, Renderer> KeyboardControl<'a, Message, Theme, Renderer> {
    pub(super) fn new(
        content: impl Into<Element<'a, Message, Theme, Renderer>>,
        id: Id,
        on_focus: impl Fn() -> Message + 'a,
        on_key: impl Fn(&Key, bool) -> KeyAction<Message> + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            id,
            focus_color: iced::Color::from_rgb(0.25, 0.55, 0.95),
            on_focus: Box::new(on_focus),
            on_key: Box::new(on_key),
        }
    }

    pub(super) fn focus_color(mut self, color: iced::Color) -> Self {
        self.focus_color = color;
        self
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for KeyboardControl<'_, Message, Theme, Renderer>
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

        let pointer_pressed = match event {
            Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)) => {
                Some(cursor.is_over(layout.bounds()))
            }
            Event::Touch(iced::touch::Event::FingerPressed { position, .. }) => {
                Some(layout.bounds().contains(*position))
            }
            _ => None,
        };
        match pointer_pressed {
            Some(true) => {
                shell.publish((self.on_focus)());
                shell.capture_event();
                return;
            }
            // A press anywhere else hands the keyboard to whatever was pressed, the way a text
            // input gives up its caret: the wrapper must stop swallowing arrow keys for it.
            Some(false) => {
                tree.state.downcast_mut::<State>().focused = false;
            }
            None => {}
        }

        if shell.is_event_captured() {
            return;
        }

        let state = tree.state.downcast_ref::<State>();
        if let Event::Keyboard(iced::keyboard::Event::KeyPressed { key, repeat, .. }) = event
            && state.focused
        {
            match (self.on_key)(key, *repeat) {
                KeyAction::Ignored => {}
                KeyAction::Captured => shell.capture_event(),
                KeyAction::Publish(message) => {
                    shell.publish(message);
                    shell.capture_event();
                }
            }
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

impl<'a, Message, Theme, Renderer> From<KeyboardControl<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(control: KeyboardControl<'a, Message, Theme, Renderer>) -> Self {
        Self::new(control)
    }
}

#[cfg(test)]
mod tests {
    use iced::advanced::widget::operation::{Operation as _, Outcome, focusable};
    use iced::advanced::{Layout, Widget, layout};
    use iced::keyboard::{Key, key::Named};
    use iced::widget::{Column, Row, Space, button};
    use iced::{Element, Event, Length, Point, Size};

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum TestMessage {
        Focus,
        Activate,
        Select(usize),
        Child,
    }

    type TestElement<'a> = Element<'a, TestMessage, iced::Theme, ()>;

    fn key_event(key: Key, repeat: bool) -> Event {
        Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::empty(),
            text: None,
            repeat,
        })
    }

    fn update(
        control: &mut KeyboardControl<'_, TestMessage, iced::Theme, ()>,
        tree: &mut Tree,
        node: &layout::Node,
        event: &Event,
        cursor: mouse::Cursor,
    ) -> (Vec<TestMessage>, iced::event::Status) {
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        let mut clipboard = iced::advanced::clipboard::Null;
        control.update(
            tree,
            event,
            Layout::new(node),
            cursor,
            &(),
            &mut clipboard,
            &mut shell,
            &node.bounds(),
        );
        let status = shell.event_status();
        drop(shell);
        (messages, status)
    }

    #[test]
    fn navigation_is_bounded_and_does_not_wrap() {
        assert_eq!(
            linear_selection(&Key::Named(Named::ArrowLeft), 0, 5),
            SelectionMove::Captured
        );
        assert_eq!(
            linear_selection(&Key::Named(Named::ArrowRight), 0, 5),
            SelectionMove::Select(1)
        );
        assert_eq!(
            linear_selection(&Key::Named(Named::End), 2, 5),
            SelectionMove::Select(4)
        );
        assert_eq!(
            grid_selection(&Key::Named(Named::ArrowLeft), 16, 16, 256),
            SelectionMove::Captured
        );
        assert_eq!(
            grid_selection(&Key::Named(Named::ArrowRight), 15, 16, 256),
            SelectionMove::Captured
        );
        assert_eq!(
            grid_selection(&Key::Named(Named::ArrowDown), 239, 16, 256),
            SelectionMove::Select(255)
        );
        assert_eq!(
            grid_selection(&Key::Named(Named::ArrowDown), 240, 16, 256),
            SelectionMove::Captured
        );
        assert_eq!(
            grid_selection(&Key::Named(Named::Home), 0, 16, 256),
            SelectionMove::Captured
        );
        assert_eq!(
            grid_selection(&Key::Named(Named::End), 17, 16, 256),
            SelectionMove::Select(255)
        );
        assert_eq!(
            grid_selection(&Key::Named(Named::Tab), 17, 16, 256),
            SelectionMove::Ignored
        );
    }

    #[test]
    fn activation_consumes_repeat_without_republishing() {
        assert_eq!(
            activation(&Key::Named(Named::Space), false, TestMessage::Activate),
            KeyAction::Publish(TestMessage::Activate)
        );
        assert_eq!(
            activation(&Key::Named(Named::Enter), true, TestMessage::Activate),
            KeyAction::Captured
        );
        assert_eq!(
            activation(&Key::Named(Named::Tab), false, TestMessage::Activate),
            KeyAction::Ignored
        );
    }

    #[test]
    fn focused_wrapper_routes_keys_and_leaves_tab_for_window_traversal() {
        let id = Id::from("keyboard-control-test".to_string());
        let content: TestElement<'_> = Space::new().width(40).height(20).into();
        let mut control = KeyboardControl::new(
            content,
            id.clone(),
            || TestMessage::Focus,
            |key, repeat| {
                if matches!(key, Key::Named(Named::ArrowRight)) {
                    KeyAction::Publish(TestMessage::Select(1))
                } else {
                    activation(key, repeat, TestMessage::Activate)
                }
            },
        );
        let mut tree = Tree::new(&control as &dyn Widget<TestMessage, iced::Theme, ()>);
        let node = control.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 100.0)),
        );
        let mut focus = focusable::focus::<()>(id);
        control.operate(&mut tree, Layout::new(&node), &(), &mut focus);

        let (messages, status) = update(
            &mut control,
            &mut tree,
            &node,
            &key_event(Key::Named(Named::ArrowRight), true),
            mouse::Cursor::Unavailable,
        );
        assert_eq!(messages, vec![TestMessage::Select(1)]);
        assert_eq!(status, iced::event::Status::Captured);

        let (messages, status) = update(
            &mut control,
            &mut tree,
            &node,
            &key_event(Key::Named(Named::Space), true),
            mouse::Cursor::Unavailable,
        );
        assert!(messages.is_empty());
        assert_eq!(status, iced::event::Status::Captured);

        let (messages, status) = update(
            &mut control,
            &mut tree,
            &node,
            &key_event(Key::Named(Named::Tab), false),
            mouse::Cursor::Unavailable,
        );
        assert!(messages.is_empty());
        assert_eq!(status, iced::event::Status::Ignored);
    }

    #[test]
    fn pointer_press_outside_the_wrapper_releases_its_keys() {
        let id = Id::from("keyboard-control-outside-test".to_string());
        let content: TestElement<'_> = Space::new().width(40).height(20).into();
        let mut control = KeyboardControl::new(
            content,
            id.clone(),
            || TestMessage::Focus,
            |key, repeat| {
                if matches!(key, Key::Named(Named::ArrowRight)) {
                    KeyAction::Publish(TestMessage::Select(1))
                } else {
                    activation(key, repeat, TestMessage::Activate)
                }
            },
        );
        let mut tree = Tree::new(&control as &dyn Widget<TestMessage, iced::Theme, ()>);
        let node = control.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 100.0)),
        );
        let mut focus = focusable::focus::<()>(id);
        control.operate(&mut tree, Layout::new(&node), &(), &mut focus);

        let (messages, _) = update(
            &mut control,
            &mut tree,
            &node,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            mouse::Cursor::Available(Point::new(300.0, 300.0)),
        );
        assert!(
            messages.is_empty(),
            "a press elsewhere is not a focus request"
        );

        let (messages, status) = update(
            &mut control,
            &mut tree,
            &node,
            &key_event(Key::Named(Named::ArrowRight), true),
            mouse::Cursor::Unavailable,
        );
        assert!(messages.is_empty());
        assert_eq!(
            status,
            iced::event::Status::Ignored,
            "arrow keys belong to whatever was pressed"
        );
    }

    #[test]
    fn pointer_press_focuses_wrapper_without_replacing_child_activation() {
        let id = Id::from("keyboard-control-pointer-test".to_string());
        let content: TestElement<'_> = button(Space::new().width(40).height(20))
            .on_press(TestMessage::Child)
            .into();
        let mut control = KeyboardControl::new(
            content,
            id,
            || TestMessage::Focus,
            |_key, _repeat| KeyAction::Ignored,
        );
        let mut tree = Tree::new(&control as &dyn Widget<TestMessage, iced::Theme, ()>);
        let node = control.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 100.0)),
        );
        let cursor = mouse::Cursor::Available(Point::new(
            node.bounds().center_x(),
            node.bounds().center_y(),
        ));
        let press = Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left));
        let (messages, status) = update(&mut control, &mut tree, &node, &press, cursor);
        assert_eq!(messages, vec![TestMessage::Focus]);
        assert_eq!(status, iced::event::Status::Captured);

        let release = Event::Mouse(iced::mouse::Event::ButtonReleased(
            iced::mouse::Button::Left,
        ));
        let (messages, status) = update(&mut control, &mut tree, &node, &release, cursor);
        assert_eq!(messages, vec![TestMessage::Child]);
        assert_eq!(status, iced::event::Status::Captured);
    }

    #[test]
    fn xterm_sized_child_grid_contributes_exactly_one_focus_stop() {
        let mut grid = Column::new().spacing(1);
        for _ in 0..16 {
            let mut row = Row::new().spacing(1);
            for _ in 0..16 {
                row = row.push(
                    button(
                        Space::new()
                            .width(Length::Fixed(2.0))
                            .height(Length::Fixed(2.0)),
                    )
                    .padding(0)
                    .on_press(TestMessage::Child),
                );
            }
            grid = grid.push(row);
        }
        let content: TestElement<'_> = grid.into();
        let mut control = KeyboardControl::new(
            content,
            Id::from("keyboard-control-grid-test".to_string()),
            || TestMessage::Focus,
            |_key, _repeat| KeyAction::Ignored,
        );
        let mut tree = Tree::new(&control as &dyn Widget<TestMessage, iced::Theme, ()>);
        let node = control.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 200.0)),
        );
        let mut count = focusable::count();
        {
            let mut erased =
                iced::advanced::widget::operation::black_box::<focusable::Count, ()>(&mut count);
            control.operate(&mut tree, Layout::new(&node), &(), &mut erased);
        }
        let Outcome::Some(count) = count.finish() else {
            panic!("focus-count operation must finish in one pass");
        };
        assert_eq!(
            count,
            focusable::Count {
                focused: None,
                total: 1,
            }
        );
    }
}

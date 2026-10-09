use crate::{ipc::UserWindowEvent, window};
use dioxus_core::RenderTargetId;
use slab::Slab;
use std::cell::RefCell;
use tao::{event::Event, event_loop::EventLoopWindowTarget, window::WindowId};

/// The unique identifier of a window event handler. This can be used to later remove the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WryEventHandler(pub(crate) usize);

impl WryEventHandler {
    /// Unregister this event handler from the window
    pub fn remove(&self) {
        window().app_context().event_handlers.remove(*self)
    }
}

#[derive(Default)]
pub struct WindowEventHandlers {
    handlers: RefCell<Slab<WryWindowEventHandlerInner>>,
}

struct WryWindowEventHandlerInner {
    /// The logical window whose native window events reach this handler.
    target: RenderTargetId,

    #[allow(clippy::type_complexity)]
    handler:
        Box<dyn FnMut(&Event<UserWindowEvent>, &EventLoopWindowTarget<UserWindowEvent>) + 'static>,
}

impl WindowEventHandlers {
    pub(crate) fn add(
        &self,
        target: RenderTargetId,
        handler: impl FnMut(&Event<UserWindowEvent>, &EventLoopWindowTarget<UserWindowEvent>) + 'static,
    ) -> WryEventHandler {
        WryEventHandler(
            self.handlers
                .borrow_mut()
                .insert(WryWindowEventHandlerInner {
                    target,
                    handler: Box::new(handler),
                }),
        )
    }

    pub(crate) fn remove(&self, id: WryEventHandler) {
        self.handlers.borrow_mut().try_remove(id.0);
    }

    /// Run every handler, giving a window event only to handlers of the window `target_of` returns.
    pub fn apply_event(
        &self,
        event: &Event<UserWindowEvent>,
        target: &EventLoopWindowTarget<UserWindowEvent>,
        target_of: impl Fn(WindowId) -> Option<RenderTargetId>,
    ) {
        let event_target = match event {
            Event::WindowEvent { window_id, .. } => Some(target_of(*window_id)),
            _ => None,
        };
        for (_, handler) in self.handlers.borrow_mut().iter_mut() {
            if event_target.is_some_and(|event_target| event_target != Some(handler.target)) {
                continue;
            }

            (handler.handler)(event, target)
        }
    }
}

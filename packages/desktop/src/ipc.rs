use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tao::window::WindowId;
use wry::{RequestAsyncResponder, http::Request};

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum UserWindowEvent {
    /// A global hotkey event
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    GlobalHotKeyEvent(global_hotkey::GlobalHotKeyEvent),

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    MudaMenuEvent(muda::MenuEvent),

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    TrayIconEvent(tray_icon::TrayIconEvent),

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    TrayMenuEvent(tray_icon::menu::MenuEvent),

    /// Poll the shared VirtualDom.
    Poll,

    /// Handle an ipc message eminating from the window.postMessage of a given webview
    Ipc {
        id: WindowId,
        msg: IpcMessage,
    },

    /// Handle a hotreload event, basically telling us to update our templates
    #[cfg(all(feature = "devtools", debug_assertions))]
    HotReloadEvent(dioxus_devtools::DevserverMsg),

    // Windows-only drag-n-drop fix events.
    WindowsDragDrop(WindowId),
    WindowsDragOver(WindowId, i32, i32),
    WindowsDragLeave(WindowId),

    /// Create a new window
    NewWindow,

    /// Request that a given window close, honoring its close behavior and component lifecycle.
    RequestWindowClose(WindowId),

    /// Serve a custom protocol request on the event loop thread, where the window's handlers live.
    ProtocolRequest {
        id: WindowId,
        webview_id: String,
        protocol: String,
        request: PendingProtocolRequest,
    },

    /// Destroy a native window after its Dioxus owner has released the portal.
    DestroyWindow(WindowId),

    /// Gracefully shutdown the entire app
    Shutdown,
}

type ProtocolExchange = (Request<Vec<u8>>, RequestAsyncResponder);

/// A protocol request and the responder that answers it, taken exactly once on the event loop.
#[derive(Clone)]
pub struct PendingProtocolRequest(Arc<Mutex<Option<ProtocolExchange>>>);

impl PendingProtocolRequest {
    pub(crate) fn new(request: Request<Vec<u8>>, responder: RequestAsyncResponder) -> Self {
        Self(Arc::new(Mutex::new(Some((request, responder)))))
    }

    /// The request and its responder, or `None` if another copy of this event took them.
    pub(crate) fn take(&self) -> Option<ProtocolExchange> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

impl std::fmt::Debug for PendingProtocolRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingProtocolRequest")
            .finish_non_exhaustive()
    }
}

/// A message struct that manages the communication between the webview and the eventloop code
///
/// This needs to be serializable across the JS boundary, so the method names and structs are sensitive.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct IpcMessage {
    method: String,
    params: serde_json::Value,
}

/// A set of known messages that we need to respond to
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum IpcMethod<'a> {
    UserEvent,
    Query,
    BrowserOpen,
    Initialize,
    Other(&'a str),
}

impl IpcMessage {
    pub(crate) fn method(&self) -> IpcMethod<'_> {
        match self.method.as_str() {
            "user_event" => IpcMethod::UserEvent,
            "query" => IpcMethod::Query,
            "browser_open" => IpcMethod::BrowserOpen,
            "initialize" => IpcMethod::Initialize,
            _ => IpcMethod::Other(&self.method),
        }
    }

    pub(crate) fn params(self) -> serde_json::Value {
        self.params
    }
}

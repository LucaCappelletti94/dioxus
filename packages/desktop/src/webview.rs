use crate::WeakDesktopContext;
use crate::desktop_context::{PendingDesktopWindow, PendingWindowCancellation};
use crate::desktop_state::{DesktopAppContext, NativeWindow};
use crate::file_upload::{DesktopFileData, DesktopFileDragEvent};
use crate::menubar::DioxusMenu;
use crate::{
    DesktopContext, DesktopService, WindowCloseBehaviour, WindowConfig,
    assets::AssetHandlerRegistry,
    config::{
        AsyncProtocolHandler, MenuBuilderState, NavigationHandler, OnWindow, ProtocolHandler,
    },
    edits::WryQueue,
    file_upload::NativeFileHover,
    ipc::{PendingProtocolRequest, UserWindowEvent},
    protocol,
};
use crate::{element::DesktopElement, file_upload::DesktopFormData};
use base64::prelude::BASE64_STANDARD;
use dioxus_core::{RenderTargetId, Runtime, VirtualDom};
use dioxus_hooks::to_owned;
use dioxus_html::{FileData, FormValue, HtmlEvent, PlatformEventData, SerializedFileData};
use rustc_hash::FxHashMap;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::{
    cell::{OnceCell, RefCell},
    path::PathBuf,
    time::Duration,
};
use tao::{
    event_loop::EventLoopProxy,
    window::{WindowBuilder, WindowId},
};
use wry::{
    DragDropEvent, RequestAsyncResponder, WebContext, WebViewBuilder, WebViewId,
    http::{Request, Response, status::StatusCode},
};

fn restore_window_state(
    window: &tao::window::Window,
    explicit_inner_size: Option<tao::dpi::Size>,
    explicit_window_position: Option<tao::dpi::Position>,
) {
    if cfg!(target_os = "android") || cfg!(target_os = "ios") || !cfg!(debug_assertions) {
        return;
    }

    let Ok(state) = std::fs::read_to_string(crate::app::restore_file()) else {
        return;
    };
    let Ok(state) = serde_json::from_str::<crate::app::PreservedWindowState>(&state) else {
        return;
    };

    let position = (state.x, state.y);
    let size = (state.width, state.height);

    if explicit_window_position.is_none() {
        if cfg!(target_os = "macos") {
            window.set_outer_position(tao::dpi::LogicalPosition::new(position.0, position.1));
        } else {
            window.set_outer_position(tao::dpi::PhysicalPosition::new(position.0, position.1));
        }
    }

    if explicit_inner_size.is_none() {
        if cfg!(target_os = "macos") {
            window.set_inner_size(tao::dpi::LogicalSize::new(size.0, size.1));
        } else {
            window.set_inner_size(tao::dpi::PhysicalSize::new(size.0, size.1));
        }
    }
}

#[derive(Clone)]
pub(crate) struct WebviewEdits {
    runtime: Rc<Runtime>,
    target_id: RenderTargetId,
    pub wry_queue: WryQueue,
    desktop_context: Rc<OnceCell<WeakDesktopContext>>,
    /// How many index documents this webview has been served, which numbers its pages.
    served_pages: Arc<AtomicU32>,
}

impl WebviewEdits {
    fn new(runtime: Rc<Runtime>, target_id: RenderTargetId, wry_queue: WryQueue) -> Self {
        Self {
            runtime,
            target_id,
            wry_queue,
            desktop_context: Default::default(),
            served_pages: Default::default(),
        }
    }

    /// Number a newly served index document.
    pub(crate) fn serve_page(&self) -> u32 {
        self.served_pages.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Whether a newer page than `page` has been served, so `page` is being replaced.
    pub(crate) fn is_replaced(&self, page: u32) -> bool {
        page < self.served_pages.load(Ordering::Relaxed)
    }

    pub fn handle_event(
        &self,
        request: wry::http::Request<Vec<u8>>,
        responder: wry::RequestAsyncResponder,
    ) {
        let body = self
            .try_handle_event(request)
            .expect("Writing bodies to succeed");
        responder.respond(wry::http::Response::new(body))
    }

    pub fn try_handle_event(
        &self,
        request: wry::http::Request<Vec<u8>>,
    ) -> Result<Vec<u8>, serde_json::Error> {
        use serde::de::Error;

        // todo(jon):
        //
        // I'm a small bit worried about the size of the header being too big on some platforms.
        // It's unlikely we'll hit the 256k limit (from 2010 browsers...) but it's important to think about
        // https://stackoverflow.com/questions/3326210/can-http-headers-be-too-big-for-browsers
        //
        // Also important to remember here that we don't pass a body from the JavaScript side of things
        let data = request
            .headers()
            .get("dioxus-data")
            .ok_or_else(|| Error::custom("dioxus-data header not set"))?;

        let as_utf = std::str::from_utf8(data.as_bytes())
            .map_err(|_| Error::custom("dioxus-data header is not a valid (utf-8) string"))?;

        let data_from_header = base64::Engine::decode(&BASE64_STANDARD, as_utf)
            .map_err(|_| Error::custom("dioxus-data header is not a base64 string"))?;

        let response = match serde_json::from_slice(&data_from_header) {
            Ok(event) => self.handle_html_event(event),
            Err(err) => {
                tracing::error!(
                    "Error parsing user_event: {:?}. \n Contents: {:?}, \nraw: {:#?}",
                    err,
                    String::from_utf8(request.body().to_vec()),
                    request
                );
                SynchronousEventResponse::new(false)
            }
        };

        serde_json::to_vec(&response).inspect_err(|err| {
            tracing::error!("failed to serialize SynchronousEventResponse: {err:?}");
        })
    }

    pub fn handle_html_event(&self, event: HtmlEvent) -> SynchronousEventResponse {
        let HtmlEvent {
            element,
            name,
            bubbles,
            data,
        } = event;
        let Some(desktop_context) = self.desktop_context.get() else {
            tracing::error!(
                "Tried to handle event before setting the desktop context on the event handler"
            );
            return Default::default();
        };

        let desktop_context = desktop_context.upgrade().unwrap();

        let query = desktop_context.query.clone();
        let hovered_file = desktop_context.file_hover.clone();

        // check for a mounted event placeholder and replace it with a desktop specific element
        let as_any = match data {
            dioxus_html::EventData::Mounted => {
                let element = DesktopElement::new(element, desktop_context.clone(), query.clone());
                Rc::new(PlatformEventData::new(Box::new(element)))
            }
            dioxus_html::EventData::Form(form) => {
                Rc::new(PlatformEventData::new(Box::new(DesktopFormData {
                    value: form.value,
                    valid: form.valid,
                    values: form
                        .values
                        .into_iter()
                        .map(|obj| {
                            if let Some(text) = obj.text {
                                return (obj.key, FormValue::Text(text));
                            }

                            if let Some(file_data) = obj.file {
                                if file_data.path.capacity() == 0 {
                                    return (obj.key, FormValue::File(None));
                                }

                                return (
                                    obj.key,
                                    FormValue::File(Some(FileData::new(DesktopFileData(
                                        file_data.path,
                                    )))),
                                );
                            };

                            (obj.key, FormValue::Text(String::new()))
                        })
                        .collect(),
                })))
            }
            // Which also includes drops...
            dioxus_html::EventData::Drag(ref drag) => {
                // we want to override this with a native file engine, provided by the most recent drag event
                let full_file_paths = hovered_file.current_paths();

                let xfer_data = drag.data_transfer.clone();
                let new_file_data = xfer_data
                    .files
                    .iter()
                    .map(|f| {
                        let new_path = full_file_paths
                            .iter()
                            .find(|p| p.ends_with(&f.path))
                            .unwrap_or(&f.path);
                        SerializedFileData {
                            path: new_path.clone(),
                            ..f.clone()
                        }
                    })
                    .collect::<Vec<_>>();
                let new_xfer_data = dioxus_html::SerializedDataTransfer {
                    files: new_file_data,
                    ..xfer_data
                };

                Rc::new(PlatformEventData::new(Box::new(DesktopFileDragEvent {
                    mouse: drag.mouse.clone(),
                    data_transfer: new_xfer_data,
                    files: full_file_paths,
                })))
            }
            _ => data.into_any(),
        };

        let event = dioxus_core::Event::new(as_any, bubbles);
        self.runtime
            .handle_event_for_target(self.target_id, &name, event.clone(), element);

        // Get the response from the event
        SynchronousEventResponse::new(!event.default_action_enabled())
    }
}

/// A `Send + Sync` wry handler that posts each `protocol` request to the event loop.
fn forward_protocol(
    proxy: EventLoopProxy<UserWindowEvent>,
    window: WindowId,
    protocol: String,
) -> impl Fn(WebViewId, Request<Vec<u8>>, RequestAsyncResponder) + Send + Sync + 'static {
    move |webview_id, request, responder| {
        _ = proxy.send_event(UserWindowEvent::ProtocolRequest {
            id: window,
            webview_id: webview_id.to_string(),
            protocol: protocol.clone(),
            request: PendingProtocolRequest::new(request, responder),
        });
    }
}

/// Everything needed to build the window's native host again, since Android can move it to another `Activity`.
pub(crate) struct WindowRecipe {
    window: WindowBuilder,
    explicit_size: Option<tao::dpi::Size>,
    explicit_position: Option<tao::dpi::Position>,
    on_window: RefCell<Option<OnWindow>>,
    data_dir: Option<PathBuf>,
    headless: bool,
    navigation_handler: Option<NavigationHandler>,
    disable_file_drop_handler: bool,
    background_color: Option<(u8, u8, u8, u8)>,
    disable_context_menu: bool,
    #[cfg_attr(
        not(any(
            target_os = "windows",
            target_os = "macos",
            target_os = "ios",
            target_os = "android"
        )),
        expect(dead_code, reason = "GTK webviews are never child windows")
    )]
    as_child_window: bool,
    #[cfg_attr(
        not(target_os = "windows"),
        expect(dead_code, reason = "only WebView2 takes browser arguments")
    )]
    additional_windows_args: Option<String>,
    protocols: FxHashMap<String, ProtocolHandler>,
    async_protocols: FxHashMap<String, AsyncProtocolHandler>,
    custom_head: Option<String>,
    custom_index: Option<String>,
    root_name: String,
}

impl WindowRecipe {
    fn new(cfg: WindowConfig) -> (Self, MenuBuilderState, WindowCloseBehaviour) {
        let explicit_size = cfg.window.window.inner_size;
        let explicit_position = cfg.window.window.position;
        let mut window = cfg.window;

        // tao makes small desktop windows, and on mobile a `None` size fills the screen.
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        if explicit_size.is_none() {
            window = window.with_inner_size(tao::dpi::LogicalSize::new(800.0, 600.0));
        }

        if window.window.window_icon.is_none() {
            window = window.with_window_icon(crate::default_icon().ok());
        }

        let recipe = Self {
            headless: !window.window.visible,
            window,
            explicit_size,
            explicit_position,
            on_window: RefCell::new(cfg.on_window),
            data_dir: cfg.data_dir,
            navigation_handler: cfg.navigation_handler,
            disable_file_drop_handler: cfg.disable_file_drop_handler,
            background_color: cfg.background_color,
            disable_context_menu: cfg.disable_context_menu,
            as_child_window: cfg.as_child_window,
            additional_windows_args: cfg.additional_windows_args,
            protocols: cfg.protocols.into_iter().collect(),
            async_protocols: cfg.asynchronous_protocols.into_iter().collect(),
            custom_head: cfg.custom_head,
            custom_index: cfg.custom_index,
            root_name: cfg.root_name,
        };
        (recipe, cfg.menu, cfg.window_close_behavior)
    }
}

/// A freshly built native window and webview for one logical window.
struct BuiltHost {
    native: NativeWindow,
    edits: WebviewEdits,
    web_context: WebContext,
}

/// Build a native window and webview for `target_id` from `recipe`.
fn build_host(
    recipe: &Rc<WindowRecipe>,
    file_hover: &NativeFileHover,
    target_id: RenderTargetId,
    dom: &mut VirtualDom,
    app_context: &Rc<DesktopAppContext>,
) -> Result<BuiltHost, tao::error::OsError> {
    let window = Arc::new(recipe.window.clone().build(&app_context.target)?);
    restore_window_state(&window, recipe.explicit_size, recipe.explicit_position);
    if let Some(on_build) = recipe.on_window.borrow_mut().as_mut() {
        on_build(window.clone(), dom);
    }

    // https://developer.apple.com/documentation/appkit/nswindowcollectionbehavior/nswindowcollectionbehaviormanaged
    #[cfg(target_os = "macos")]
    {
        use objc2::rc::Retained;
        use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};
        use tao::platform::macos::WindowExtMacOS;
        let ns_window: Retained<NSWindow> =
            unsafe { Retained::retain(window.ns_window().cast()) }.unwrap();
        ns_window.setCollectionBehavior(NSWindowCollectionBehavior::Managed)
    }

    let mut web_context = WebContext::new(recipe.data_dir.clone().or_else(|| {
        // On Windows, WebView2 defaults to storing its data next to the executable.
        // This fails on certain drives (e.g. ReFS dev drives, Program Files) where the
        // directory may not be writable. Fall back to %LOCALAPPDATA%/<exe_name> automatically.
        if cfg!(windows) {
            let exe = std::env::current_exe().ok()?;
            let name = exe.file_stem()?.to_str()?;
            let local_app_data = std::env::var("LOCALAPPDATA").ok()?;
            Some(std::path::PathBuf::from(local_app_data).join(name))
        } else {
            None
        }
    }));
    let edits = WebviewEdits::new(
        dom.runtime(),
        target_id,
        app_context.websocket.create_queue(),
    );

    let request_handler = forward_protocol(
        app_context.proxy.clone(),
        window.id(),
        String::from("dioxus"),
    );

    let ipc_handler = {
        let window_id = window.id();
        to_owned![app_context.proxy];
        move |payload: wry::http::Request<String>| {
            // defer the event to the main thread
            let body = payload.into_body();
            if let Ok(msg) = serde_json::from_str(&body) {
                _ = proxy.send_event(UserWindowEvent::Ipc { id: window_id, msg });
            }
        }
    };

    let file_drop_handler = {
        to_owned![file_hover];
        let (proxy, window_id) = (app_context.proxy.to_owned(), window.id());
        move |evt: DragDropEvent| {
            if cfg!(not(windows)) {
                // Update the most recent file drop event - when the event comes in from the webview we can use the
                // most recent event to build a new event with the files in it.
                file_hover.set(evt);
            } else {
                // Windows webview blocks HTML-native events when the drop handler is provided.
                // The problem is that the HTML-native events don't provide the file, so we need this.
                // Solution: this glue code to mimic drag drop events.
                file_hover.set(evt.clone());
                match evt {
                    wry::DragDropEvent::Drop {
                        paths: _,
                        position: _,
                    } => {
                        _ = proxy.send_event(UserWindowEvent::WindowsDragDrop(window_id));
                    }
                    wry::DragDropEvent::Over { position } => {
                        _ = proxy.send_event(UserWindowEvent::WindowsDragOver(
                            window_id, position.0, position.1,
                        ));
                    }
                    wry::DragDropEvent::Leave => {
                        _ = proxy.send_event(UserWindowEvent::WindowsDragLeave(window_id));
                    }
                    _ => {}
                }
            }

            false
        }
    };

    let page_loaded = AtomicBool::new(false);
    let navigation_recipe = recipe.clone();

    let mut webview = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_bounds(wry::Rect {
            position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(0.0, 0.0)),
            size: wry::dpi::Size::Physical(wry::dpi::PhysicalSize::new(
                window.inner_size().width,
                window.inner_size().height,
            )),
        })
        .with_transparent(recipe.window.window.transparent)
        .with_url("dioxus://index.html/")
        .with_ipc_handler(ipc_handler)
        .with_navigation_handler(move |var| {
            // Serve the index and assets.
            if var.starts_with("dioxus://")
                || var.starts_with("http://dioxus.")
                || var.starts_with("https://dioxus.")
            {
                // Android loads the index again into the webview of a recreated activity, which is
                // redrawn on `initialize`. Other navigations, such as a form submission, would replace the app.
                let page_loaded = page_loaded.swap(true, std::sync::atomic::Ordering::SeqCst);
                return (cfg!(target_os = "android") && var == crate::protocol::BASE_URI)
                    || !page_loaded;
            }

            // External links always open somewhere else. Prevents the webview from navigating
            if var.starts_with("http://")
                || var.starts_with("https://")
                || var.starts_with("mailto:")
            {
                _ = webbrowser::open(&var);
                return false;
            }

            // By default, external links are allowed. This keeps things like iframes working.
            // However, users can customize this to allow/disallow domains/routes/patterns.
            navigation_recipe
                .navigation_handler
                .as_ref()
                .map(|f| f(&var))
                .unwrap_or(true)
        })
        .with_asynchronous_custom_protocol(String::from("dioxus"), request_handler);

    // Enable https scheme on android, needed for secure context API, like the geolocation API
    #[cfg(target_os = "android")]
    {
        use wry::WebViewBuilderExtAndroid as _;

        webview = webview.with_https_scheme(true);
    };

    // Disable the webview default shortcuts to disable the reload shortcut
    #[cfg(target_os = "windows")]
    {
        use wry::WebViewBuilderExtWindows;
        webview = webview.with_browser_accelerator_keys(false);
    }

    if !recipe.disable_file_drop_handler {
        webview = webview.with_drag_drop_handler(file_drop_handler);
    }

    if let Some(color) = recipe.background_color {
        webview = webview.with_background_color(color);
    }

    for name in recipe.protocols.keys().chain(recipe.async_protocols.keys()) {
        let forward = forward_protocol(app_context.proxy.clone(), window.id(), name.clone());
        webview = webview.with_asynchronous_custom_protocol(name.clone(), forward);
    }

    const INITIALIZATION_SCRIPT: &str = r#"
    if (document.addEventListener) {
        document.addEventListener('contextmenu', function(e) {
            e.preventDefault();
        }, false);
    } else {
        document.attachEvent('oncontextmenu', function() {
            window.event.returnValue = false;
        });
    }
    "#;

    if recipe.disable_context_menu {
        // in release mode, we don't want to show the dev tool or reload menus
        webview = webview.with_initialization_script(INITIALIZATION_SCRIPT)
    } else {
        // in debug, we are okay with the reload menu showing and dev tool
        webview = webview.with_devtools(true);
    }

    #[cfg(target_os = "windows")]
    {
        use wry::WebViewBuilderExtWindows;
        if let Some(additional_windows_args) = &recipe.additional_windows_args {
            webview = webview.with_additional_browser_args(additional_windows_args);
        }
    }

    #[cfg(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "android"
    ))]
    let webview = if recipe.as_child_window {
        webview.build_as_child(&window)
    } else {
        webview.build(&window)
    };

    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "android"
    )))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().unwrap();
        webview.build_gtk(vbox)
    };

    Ok(BuiltHost {
        native: NativeWindow {
            window,
            webview: Rc::new(webview.unwrap()),
        },
        edits,
        web_context,
    })
}

/// One native window and webview, the host of a logical window while it is bound to it.
pub(crate) struct WebviewInstance {
    pub edits: WebviewEdits,
    pub(crate) native: NativeWindow,
    recipe: Rc<WindowRecipe>,

    pub desktop_context: DesktopContext,
    /// The page that last reported `initialize`, so a later page is a reload.
    pub(crate) initialized_page: Option<u32>,
    /// Redraw the whole tree into the first page, because this host replaced another.
    pub(crate) redraw_first_page: bool,

    // Wry assumes the webcontext is alive for the lifetime of the webview.
    // We need to keep the webcontext alive, otherwise the webview will crash
    _web_context: WebContext,

    // Same with the menu.
    // Currently it's a DioxusMenu because 1) we don't touch it and 2) we support a number of platforms
    // like ios where muda does not give us a menu type. It sucks but alas.
    //
    // This would be a good thing for someone looking to contribute to fix.
    _menu: Option<DioxusMenu>,
}

impl WebviewInstance {
    /// The render target this webview draws. Stored once in [`WebviewEdits`];
    /// this exposes it as the webview's identity to the app layer.
    pub(crate) fn target_id(&self) -> RenderTargetId {
        self.desktop_context.target_id
    }

    /// The native window ID of this host.
    pub(crate) fn id(&self) -> WindowId {
        self.native.window.id()
    }

    /// Make the page that reported `initialize` open the edits connection at its current location.
    pub(crate) fn connect_initialized_page(&self) {
        if let Some(page) = self.initialized_page {
            let connect = self.edits.wry_queue.connect_script(page);
            _ = self.native.webview.evaluate_script(&connect);
        }
    }

    fn from_built(
        built: BuiltHost,
        recipe: Rc<WindowRecipe>,
        desktop_context: DesktopContext,
        menu: Option<DioxusMenu>,
        redraw_first_page: bool,
    ) -> Self {
        _ = built
            .edits
            .desktop_context
            .set(Rc::downgrade(&desktop_context));
        built.native.window.request_redraw();
        Self {
            edits: built.edits,
            native: built.native,
            recipe,
            desktop_context,
            initialized_page: None,
            redraw_first_page,
            _web_context: built.web_context,
            _menu: menu,
        }
    }

    /// Build a new host for the window behind `desktop_context`.
    pub(crate) fn rebuild(
        desktop_context: &DesktopContext,
        dom: &mut VirtualDom,
        app_context: &Rc<DesktopAppContext>,
    ) -> Result<Self, tao::error::OsError> {
        let recipe = desktop_context.recipe.clone();
        let built = build_host(
            &recipe,
            &desktop_context.file_hover,
            desktop_context.target_id,
            dom,
            app_context,
        )?;
        desktop_context.replace_native(built.native.clone());
        Ok(Self::from_built(
            built,
            recipe,
            desktop_context.clone(),
            None,
            true,
        ))
    }

    /// Render the window here again through a reloaded page.
    pub(crate) fn rebind(&self) {
        self.desktop_context.replace_native(self.native.clone());
        _ = self.native.webview.reload();
    }

    /// Answer a protocol request on the event loop thread, where the window's `Rc` handlers live.
    pub(crate) fn handle_protocol_request(
        &self,
        webview_id: &str,
        protocol: &str,
        request: Request<Vec<u8>>,
        responder: RequestAsyncResponder,
    ) {
        #[cfg(feature = "tokio_runtime")]
        let _guard = tokio::runtime::Handle::current().enter();

        if let Some(handler) = self.recipe.protocols.get(protocol) {
            responder.respond(handler(webview_id, request));
            return;
        }

        if let Some(handler) = self.recipe.async_protocols.get(protocol) {
            handler(webview_id, request, responder);
            return;
        }

        if protocol == "dioxus" {
            protocol::desktop_handler(
                request,
                self.desktop_context.asset_handlers.clone(),
                responder,
                &self.edits,
                self.recipe.custom_head.clone(),
                self.recipe.custom_index.clone(),
                &self.recipe.root_name,
                self.recipe.headless,
            );
            return;
        }

        responder.respond(
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(String::from("Unknown protocol").into_bytes())
                .unwrap(),
        );
    }

    #[cfg(all(feature = "devtools", debug_assertions))]
    pub fn kick_stylsheets(&self) {
        // run eval in the webview to kick the stylesheets by appending a query string
        // we should do something less clunky than this
        _ = self
            .native
            .webview
            .evaluate_script("window.interpreter.kickAllStylesheetsOnPage()");
    }

    /// Displays a toast to the developer.
    pub(crate) fn show_toast(
        &self,
        header_text: &str,
        message: &str,
        level: &str,
        duration: Duration,
        after_reload: bool,
    ) {
        let as_ms = duration.as_millis();

        let js_fn_name = match after_reload {
            true => "scheduleDXToast",
            false => "showDXToast",
        };

        _ = self.native.webview.evaluate_script(&format!(
            r#"
                if (typeof {js_fn_name} !== "undefined") {{
                    window.{js_fn_name}("{header_text}", "{message}", "{level}", {as_ms});
                }}
                "#,
        ));
    }
}

/// A synchronous response to a browser event which may prevent the default browser's action
#[derive(serde::Serialize, Default)]
pub struct SynchronousEventResponse {
    #[serde(rename = "preventDefault")]
    prevent_default: bool,
}

impl SynchronousEventResponse {
    /// Create a new SynchronousEventResponse
    #[allow(unused)]
    pub fn new(prevent_default: bool) -> Self {
        Self { prevent_default }
    }
}

/// A webview that is queued to be created. We can't spawn webviews outside of the main event loop because it may
/// block on windows, so the app context queues them until the main event loop is ready.
pub(crate) struct PendingWebview {
    target_id: RenderTargetId,
    recipe: Rc<WindowRecipe>,
    menu: MenuBuilderState,
    close_behaviour: WindowCloseBehaviour,
    sender: futures_channel::oneshot::Sender<DesktopContext>,
}

impl PendingWebview {
    pub(crate) fn new(
        target_id: RenderTargetId,
        cfg: WindowConfig,
    ) -> (Self, PendingDesktopWindow) {
        let (sender, receiver) = futures_channel::oneshot::channel();
        let (recipe, menu, close_behaviour) = WindowRecipe::new(cfg);
        let webview = Self {
            target_id,
            recipe: Rc::new(recipe),
            menu,
            close_behaviour,
            sender,
        };
        let pending = PendingDesktopWindow {
            target_id,
            receiver,
            cancellation: PendingWindowCancellation::default(),
        };
        (webview, pending)
    }

    /// Build the window, or give the pending window back with the reason it cannot be built yet.
    pub(crate) fn create_window(
        self,
        dom: &mut VirtualDom,
        app_context: &Rc<DesktopAppContext>,
    ) -> Result<WebviewInstance, Box<(Self, tao::error::OsError)>> {
        let file_hover = NativeFileHover::default();
        let built = match build_host(&self.recipe, &file_hover, self.target_id, dom, app_context) {
            Ok(built) => built,
            Err(error) => return Err(Box::new((self, error))),
        };

        let menu = if cfg!(not(any(target_os = "android", target_os = "ios"))) {
            let menu: Option<DioxusMenu> = self.menu.into();
            if let Some(menu) = &menu {
                crate::menubar::init_menu_bar(menu, &built.native.window);
            }
            menu
        } else {
            None
        };

        let desktop_context = Rc::new(DesktopService::new(
            built.native.clone(),
            app_context.clone(),
            self.target_id,
            self.recipe.clone(),
            AssetHandlerRegistry::new(),
            file_hover,
            self.close_behaviour,
        ));
        let window = WebviewInstance::from_built(built, self.recipe, desktop_context, menu, false);

        _ = self.sender.send(window.desktop_context.clone());

        Ok(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus_core::{Element, VNode};
    use futures_util::FutureExt;

    fn empty_app() -> Element {
        VNode::empty()
    }

    #[test]
    fn canceling_pending_webview_cancels_receiver_before_reusing_target() {
        let dom = VirtualDom::new(empty_app);

        dom.in_runtime(|| {
            let target_id = Runtime::current().create_render_target();
            let (pending_webview, pending_window) =
                PendingWebview::new(target_id, WindowConfig::new());
            let cancellation = pending_window.cancellation();

            cancellation.cancel();
            assert!(cancellation.is_canceled());

            drop(pending_webview);

            assert!(Runtime::current().remove_render_target(target_id));
            assert_eq!(Runtime::current().create_render_target(), target_id);
            assert!(
                pending_window
                    .try_resolve()
                    .now_or_never()
                    .expect("dropped pending webview should cancel immediately")
                    .is_err()
            );
        });
    }

    /// The page-numbering invariant `redraw_reloaded_page` relies on: each served page gets a
    /// strictly increasing number, and a page is replaced exactly when a later one has been served.
    #[test]
    fn serve_page_numbers_pages_and_detects_replacement() {
        let dom = VirtualDom::new(empty_app);

        dom.in_runtime(|| {
            let target_id = Runtime::current().create_render_target();
            let websocket = crate::edits::EditWebsocket::start();
            let wry_queue = websocket.create_queue();
            let edits = WebviewEdits::new(Runtime::current(), target_id, wry_queue);

            let first = edits.serve_page();
            let second = edits.serve_page();
            assert_eq!(first, 1);
            assert_eq!(second, 2);

            assert!(
                edits.is_replaced(first),
                "an earlier page is replaced by a later one"
            );
            assert!(
                !edits.is_replaced(second),
                "the latest page has not been replaced"
            );
        });
    }
}

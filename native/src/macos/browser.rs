//! One page: an invisible window with Wry's WKWebView, and the ways its frames reach the game.

use super::navigation::NavigationDelegate;
use crate::api::{self, BrowserId, BrowserOptions, EventKind, Frame, Key, MouseButton, Pointer, MOD_ALT, MOD_CTRL,
    MOD_GUI, MOD_SHIFT};
use crate::{keys, script};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSImage, NSResponder, NSScreen,
    NSView, NSWindow, NSWindowStyleMask};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDataProvider, CGEvent, CGEventType, CGImage, CGImageByteOrderInfo, CGMouseButton,
    CGScrollEventUnit};
use objc2_foundation::{NSDictionary, NSError, NSNotFound, NSNumber, NSObject, NSPoint, NSProcessInfo, NSRange, NSRect, NSSize,
    NSString};
use objc2_io_surface::{IOSurface, IOSurfacePropertyKey, IOSurfacePropertyKeyBytesPerElement,
    IOSurfacePropertyKeyHeight, IOSurfacePropertyKeyPixelFormat, IOSurfacePropertyKeyWidth, IOSurfaceRef};
use objc2_metal::{MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLDevice, MTLLoadAction,
    MTLPixelFormat, MTLRenderPassDescriptor, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage};
use objc2_quartz_core::{kCARendererMetalCommandQueue, CACurrentMediaTime, CALayer, CARenderer};
use objc2_web_kit::{WKSnapshotConfiguration, WKWebView};
use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wry::{WebViewBuilder, WebViewExtMacOS};

define_class!(
    // Reports itself as the key window so WebKit shows focus and takes keys, without taking it from the game
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LiquidBounceWryWindow"]
    pub struct PageWindow;

    impl PageWindow {
        #[unsafe(method(isKeyWindow))]
        fn is_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }
    }
);

struct AppKitParent(NonNull<c_void>);

impl raw_window_handle::HasWindowHandle for AppKitParent {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let handle = raw_window_handle::AppKitWindowHandle::new(self.0);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::AppKit(handle)) })
    }
}

type Device = Retained<ProtocolObject<dyn MTLDevice>>;
type Queue = Retained<ProtocolObject<dyn MTLCommandQueue>>;

/// The page's layer tree rendered into an IOSurface.
struct Layers {
    device: Device,
    queue: Queue,
    surface: Retained<IOSurface>,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    renderer: Retained<CARenderer>,
    fresh: bool,
}

#[derive(Default)]
struct Snapshot {
    requested: bool,
    /// Pixels, width, height, stride and whether they are BGRA.
    pixels: Option<(Vec<u8>, u32, u32, u32, bool)>,
}

pub struct Browser {
    id: BrowserId,
    window: Retained<PageWindow>,
    webview: wry::WebView,
    wk: Retained<WKWebView>,
    _delegate: Retained<NavigationDelegate>,
    width: u32,
    height: u32,
    pub fps: u32,
    last_frame: Option<Instant>,
    last_url: String,
    layers: Option<Layers>,
    /// Whether WebKit hands out its layers, found out once the page has any.
    remote_layers: Option<bool>,
    snapshot: Rc<RefCell<Snapshot>>,
    front: Option<Vec<u8>>,
    buttons: u32,
    entered: bool,
}

fn io_surface(width: u32, height: u32) -> Option<Retained<IOSurface>> {
    let values = [width as usize, height as usize, 4, u32::from_be_bytes(*b"BGRA") as usize].map(NSNumber::new_usize);
    let objects: Vec<&AnyObject> = values.iter().map(|value| &**value as &AnyObject).collect();
    let keys: [&IOSurfacePropertyKey; 4] = unsafe {
        [IOSurfacePropertyKeyWidth, IOSurfacePropertyKeyHeight, IOSurfacePropertyKeyBytesPerElement,
            IOSurfacePropertyKeyPixelFormat]
    };
    let properties = NSDictionary::<IOSurfacePropertyKey, AnyObject>::from_slices(&keys, &objects);
    IOSurface::initWithProperties(IOSurface::alloc(), &properties)
}

impl Layers {
    fn new(device: Device, queue: Queue, layer: &CALayer, width: u32, height: u32) -> Option<Self> {
        let surface = io_surface(width, height)?;
        let surface_ref: &IOSurfaceRef = unsafe { &*(Retained::as_ptr(&surface) as *const IOSurfaceRef) };
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm, width as usize, height as usize, false)
        };
        descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        let texture = device.newTextureWithDescriptor_iosurface_plane(&descriptor, surface_ref, 0)?;
        let queue_object: &AnyObject = unsafe { &*(Retained::as_ptr(&queue) as *const AnyObject) };
        let options = NSDictionary::<NSString, AnyObject>::from_slices(&[unsafe { kCARendererMetalCommandQueue }],
            &[queue_object]);
        let renderer = unsafe {
            CARenderer::rendererWithMTLTexture_options(&texture, Some(&*(Retained::as_ptr(&options) as *const NSDictionary)))
        };
        renderer.setLayer(Some(layer));
        renderer.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width as f64, height as f64)));
        Some(Self { device, queue, surface, texture, renderer, fresh: false })
    }

    /// Draws what changed, returns whether anything did.
    fn render(&mut self, width: u32, height: u32) -> bool {
        unsafe { self.renderer.beginFrameAtTime_timeStamp(CACurrentMediaTime(), std::ptr::null_mut()) };
        let dirty = self.renderer.updateBounds();
        let changed = dirty.size.width > 0.0 && dirty.size.height > 0.0;
        if changed {
            // CARenderer draws over what the texture holds, so transparent parts would keep old frames
            self.clear();
            self.renderer.addUpdateRect(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width as f64, height as f64)));
            self.renderer.render();
        }
        self.renderer.endFrame();
        if changed {
            // The game reads the surface right after
            if let Some(buffer) = self.queue.commandBuffer() {
                buffer.commit();
                buffer.waitUntilCompleted();
            }
            self.fresh = true;
        }
        changed
    }

    fn clear(&self) {
        let pass = MTLRenderPassDescriptor::renderPassDescriptor();
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&self.texture));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        attachment.setClearColor(MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 0.0 });
        if let Some(buffer) = self.queue.commandBuffer() {
            if let Some(encoder) = buffer.renderCommandEncoderWithDescriptor(&pass) {
                encoder.endEncoding();
            }
            buffer.commit();
        }
    }

    fn surface_id(&self) -> i64 {
        Retained::as_ptr(&self.surface) as i64
    }
}

/// Whether WebKit shows the page through the window server, where no renderer of this process can reach it.
fn hosts_remotely(layer: &CALayer, depth: usize) -> bool {
    if layer.class().name().to_string_lossy().contains("LayerHost") {
        return true;
    }
    depth < 12 && unsafe { layer.sublayers() }
        .is_some_and(|sublayers| sublayers.iter().any(|sublayer| hosts_remotely(&sublayer, depth + 1)))
}

fn has_content(layer: &CALayer) -> bool {
    unsafe { layer.sublayers() }.is_some_and(|sublayers| sublayers.count() > 0)
}

impl Browser {
    pub fn create(id: BrowserId, options: &BrowserOptions, gpu: Option<(Device, Queue)>, mtm: MainThreadMarker)
        -> api::Result<Self> {
        let (width, height) = (options.width, options.height);
        // On screen for WebKit, which stops pages nobody could see, but invisible and click-through for the player
        let top = NSScreen::mainScreen(mtm).map_or(height as f64, |screen| screen.frame().size.height);
        let rect = NSRect::new(NSPoint::new(0.0, top - height as f64), NSSize::new(width as f64, height as f64));
        let window: Retained<PageWindow> = unsafe {
            msg_send![PageWindow::alloc(mtm), initWithContentRect: rect, styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered, defer: false]
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setAlphaValue(0.0);
        window.setIgnoresMouseEvents(true);
        window.orderFrontRegardless();

        let content = window.contentView().ok_or("The page's window has no view")?;
        let parent = AppKitParent(NonNull::new(Retained::as_ptr(&content) as *mut c_void).unwrap());
        let webview = WebViewBuilder::new()
            .with_url(&options.url)
            .with_transparent(true)
            .with_incognito(options.incognito)
            // The first click would otherwise only activate the window, which never becomes key
            .with_accept_first_mouse(true)
            .with_initialization_script(script::INIT_SCRIPT)
            .with_ipc_handler(move |request| script::dispatch(id, request.body()))
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_bounds(wry::Rect {
                position: wry::dpi::LogicalPosition::new(0, 0).into(),
                size: wry::dpi::LogicalSize::new(width, height).into(),
            })
            .build_as_child(&parent)
            .map_err(|e| e.to_string())?;
        let wk: Retained<WKWebView> = Retained::into_super(webview.webview());
        unsafe { wk.setPageZoom(options.zoom) };

        let inner = unsafe { wk.navigationDelegate() }.ok_or("Wry set no navigation delegate")?;
        let delegate = NavigationDelegate::new(id, inner, mtm);
        unsafe { wk.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate))) };

        let layers = gpu.and_then(|(device, queue)| {
            let layer = wk.layer()?;
            Layers::new(device, queue, &layer, width, height)
        });
        Ok(Self {
            id,
            window,
            webview,
            wk,
            _delegate: delegate,
            width,
            height,
            fps: options.fps.max(1),
            last_frame: None,
            last_url: String::new(),
            layers,
            remote_layers: None,
            snapshot: Rc::default(),
            front: None,
            buttons: 0,
            entered: false,
        })
    }

    pub fn is_due(&self) -> bool {
        let interval = Duration::from_secs_f64(0.95 / self.fps as f64);
        self.last_frame.is_none_or(|last| last.elapsed() >= interval)
    }

    pub fn update(&mut self, mtm: MainThreadMarker) {
        let url = unsafe { self.wk.URL() }.and_then(|url| url.absoluteString()).map(|url| url.to_string())
            .unwrap_or_default();
        if url != self.last_url {
            api::push_event(self.id, EventKind::Url, 0, url.clone(), "");
            self.last_url = url;
        }
        if !self.is_due() {
            return;
        }
        self.last_frame = Some(Instant::now());

        if self.layers.is_some() && self.remote_layers.is_none() {
            if let Some(layer) = self.wk.layer().filter(|layer| has_content(layer)) {
                let remote = !hosts_remotely(&layer, 0);
                if !remote {
                    api::warn("WebKit shows the page through the window server, it is copied through memory");
                    self.layers = None;
                }
                self.remote_layers = Some(remote);
            }
        }
        match &mut self.layers {
            Some(layers) => {
                layers.render(self.width, self.height);
            }
            None => self.request_snapshot(mtm),
        }
    }

    fn request_snapshot(&mut self, mtm: MainThreadMarker) {
        if self.snapshot.borrow().requested {
            return;
        }
        self.snapshot.borrow_mut().requested = true;
        let snapshot = self.snapshot.clone();
        let block = RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
            let mut snapshot = snapshot.borrow_mut();
            snapshot.requested = false;
            let Some(image) = (unsafe { image.as_ref() }) else { return };
            let Some(cg) = (unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) })
            else { return };
            let Some(data) = CGDataProvider::data(CGImage::data_provider(Some(&cg)).as_deref()) else { return };
            let order = CGImage::byte_order_info(Some(&cg));
            snapshot.pixels = Some((
                data.to_vec(),
                CGImage::width(Some(&cg)) as u32,
                CGImage::height(Some(&cg)) as u32,
                CGImage::bytes_per_row(Some(&cg)) as u32,
                order == CGImageByteOrderInfo::Order32Little,
            ));
        });
        let configuration = unsafe { WKSnapshotConfiguration::new(mtm) };
        unsafe { self.wk.takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &block) };
    }

    pub fn take_frame(&mut self) -> Option<Frame> {
        if let Some(layers) = self.layers.as_mut().filter(|layers| layers.fresh) {
            layers.fresh = false;
            return Some(Frame::IoSurface { surface: layers.surface_id(), width: self.width, height: self.height,
                flipped: true });
        }
        let (pixels, width, height, stride, bgra) = self.snapshot.borrow_mut().pixels.take()?;
        let front = self.front.insert(pixels);
        Some(Frame::Pixels { data: front.as_ptr(), len: front.len(), width, height, stride, bgra, flipped: false })
    }

    pub fn navigate(&self, url: &str) {
        self.webview.load_url(url).ok();
    }

    pub fn reload(&self, ignore_cache: bool) {
        unsafe {
            if ignore_cache {
                self.wk.reloadFromOrigin();
            } else {
                self.wk.reload();
            }
        }
    }

    pub fn go_back(&self) {
        unsafe { self.wk.goBack() };
    }

    pub fn go_forward(&self) {
        unsafe { self.wk.goForward() };
    }

    pub fn resize(&mut self, width: u32, height: u32, zoom: f64) {
        self.width = width;
        self.height = height;
        let frame = self.window.frame();
        let top = frame.origin.y + frame.size.height;
        self.window.setFrame_display(
            NSRect::new(NSPoint::new(frame.origin.x, top - height as f64), NSSize::new(width as f64, height as f64)),
            false);
        self.webview.set_bounds(wry::Rect {
            position: wry::dpi::LogicalPosition::new(0, 0).into(),
            size: wry::dpi::LogicalSize::new(width, height).into(),
        }).ok();
        unsafe { self.wk.setPageZoom(zoom) };

        if let Some(old) = self.layers.take() {
            gone(old.surface_id());
            self.layers = self.wk.layer().and_then(|layer| Layers::new(old.device, old.queue, &layer, width, height));
        }
    }

    fn modifier_flags(modifiers: i32) -> NSEventModifierFlags {
        let mut flags = NSEventModifierFlags::empty();
        if modifiers & MOD_SHIFT != 0 {
            flags |= NSEventModifierFlags::Shift;
        }
        if modifiers & MOD_CTRL != 0 {
            flags |= NSEventModifierFlags::Control;
        }
        if modifiers & MOD_ALT != 0 {
            flags |= NSEventModifierFlags::Option;
        }
        if modifiers & MOD_GUI != 0 {
            flags |= NSEventModifierFlags::Command;
        }
        flags
    }

    /// A point of the page in screen coordinates from the top left, where the window sits.
    fn screen_point(&self, x: f64, y: f64) -> CGPoint {
        let frame = self.window.frame();
        let screen_top = NSScreen::mainScreen(self.window.mtm()).map_or(0.0, |screen| screen.frame().size.height);
        CGPoint::new(frame.origin.x + x, screen_top - (frame.origin.y + frame.size.height) + y)
    }

    fn uptime() -> f64 {
        NSProcessInfo::processInfo().systemUptime()
    }

    /// Mouse events go straight to the view under the point, the window server never sends this window any.
    pub fn pointer(&mut self, x: f64, y: f64, pointer: Pointer) {
        let Some(content) = self.window.contentView() else { return };
        let location = NSPoint::new(x, self.height as f64 - y);
        let target: Retained<NSView> = content.hitTest(location).unwrap_or(content);
        let number = self.window.windowNumber();
        let event = |kind: NSEventType| {
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind, location, NSEventModifierFlags::empty(), Self::uptime(), number, None, 0, 1, 1.0)
        };
        match pointer {
            Pointer::Move => {
                let kind = if self.buttons & 1 != 0 { NSEventType::LeftMouseDragged } else { NSEventType::MouseMoved };
                let variant = std::env::var("WRY_MAC_MOVE").unwrap_or_default();
                if variant == "entered" && !self.entered {
                    self.entered = true;
                    let entered = unsafe {
                        NSEvent::enterExitEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_trackingNumber_userData(
                            NSEventType::MouseEntered, location, NSEventModifierFlags::empty(), Self::uptime(), number, None,
                            0, 0, std::ptr::null_mut())
                    };
                    if let Some(entered) = entered {
                        target.mouseEntered(&entered);
                    }
                }
                let event = if variant == "cgevent" {
                    let screen = self.screen_point(x, y);
                    CGEvent::new_mouse_event(None, CGEventType::MouseMoved, screen, CGMouseButton::Left)
                        .and_then(|cg| NSEvent::eventWithCGEvent(&cg))
                } else {
                    event(kind)
                };
                if let Some(event) = event {
                    if variant == "window" {
                        self.window.setAcceptsMouseMovedEvents(true);
                        self.window.sendEvent(&event);
                    } else if kind == NSEventType::LeftMouseDragged {
                        target.mouseDragged(&event);
                    } else {
                        target.mouseMoved(&event);
                    }
                }
            }
            Pointer::Button(button, pressed) => {
                let (flag, kind) = match (button, pressed) {
                    (MouseButton::Left, true) => (1, NSEventType::LeftMouseDown),
                    (MouseButton::Left, false) => (1, NSEventType::LeftMouseUp),
                    (MouseButton::Right, true) => (2, NSEventType::RightMouseDown),
                    (MouseButton::Right, false) => (2, NSEventType::RightMouseUp),
                    (MouseButton::Middle, true) => (4, NSEventType::OtherMouseDown),
                    (MouseButton::Middle, false) => (4, NSEventType::OtherMouseUp),
                };
                if pressed {
                    self.buttons |= flag;
                } else {
                    self.buttons &= !flag;
                }
                let Some(event) = event(kind) else { return };
                match kind {
                    NSEventType::LeftMouseDown => target.mouseDown(&event),
                    NSEventType::LeftMouseUp => target.mouseUp(&event),
                    NSEventType::RightMouseDown => target.rightMouseDown(&event),
                    NSEventType::RightMouseUp => target.rightMouseUp(&event),
                    NSEventType::OtherMouseDown => target.otherMouseDown(&event),
                    _ => target.otherMouseUp(&event),
                }
            }
            Pointer::Scroll(steps) => {
                let Some(scroll) = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 1,
                    steps.round() as i32, 0, 0) else { return };
                CGEvent::set_location(Some(&scroll), self.screen_point(x, y));
                if let Some(event) = NSEvent::eventWithCGEvent(&scroll) {
                    target.scrollWheel(&event);
                }
            }
        }
    }

    pub fn focus(&self) {
        let responder: &NSResponder = &self.wk;
        if self.window.firstResponder().as_deref() != Some(responder) {
            self.window.makeFirstResponder(Some(responder));
        }
    }

    pub fn key(&self, key: &Key) {
        if key.typed_character().is_some() {
            return;
        }
        let by_scancode = keys::lookup(key.scancode);
        // Shortcuts follow the layout: the letter the key types, not where it sits
        let (mac, characters) = match char::from_u32(key.keycode as u32).filter(|c| c.is_ascii_graphic()) {
            Some(c) if by_scancode.is_none_or(|info| info.key.is_empty()) => {
                let info = keys::by_keysym(c.to_ascii_lowercase() as u32).or(by_scancode);
                (info.map_or(0, |info| info.mac), c.to_ascii_lowercase().to_string())
            }
            _ => match by_scancode {
                Some(info) if info.mac_chars != 0 => (info.mac, char::from_u32(info.mac_chars).unwrap().to_string()),
                Some(info) => (info.mac, String::new()),
                None => return,
            },
        };
        let flags = Self::modifier_flags(key.modifiers);
        let text = NSString::from_str(&characters);
        let kind = if keys::is_modifier(key.scancode) {
            NSEventType::FlagsChanged
        } else if key.pressed {
            NSEventType::KeyDown
        } else {
            NSEventType::KeyUp
        };
        let Some(event) = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            kind, NSPoint::new(0.0, 0.0), flags, Self::uptime(), self.window.windowNumber(), None, &text, &text, false, mac)
        else { return };
        let Some(responder) = self.window.firstResponder() else { return };
        match kind {
            NSEventType::FlagsChanged => responder.flagsChanged(&event),
            NSEventType::KeyUp => responder.keyUp(&event),
            _ => {
                responder.keyDown(&event);
                // What the Edit menu does in other apps, which the game has none of
                if key.modifiers & MOD_GUI != 0 && key.modifiers & (MOD_CTRL | MOD_ALT) == 0 {
                    let shift = key.modifiers & MOD_SHIFT != 0;
                    let action = match (characters.as_str(), shift) {
                        ("a", false) => Some(objc2::sel!(selectAll:)),
                        ("c", false) => Some(objc2::sel!(copy:)),
                        ("x", false) => Some(objc2::sel!(cut:)),
                        ("v", false) => Some(objc2::sel!(paste:)),
                        ("z", false) => Some(objc2::sel!(undo:)),
                        ("z", true) => Some(objc2::sel!(redo:)),
                        _ => None,
                    };
                    if let Some(action) = action {
                        let responds: bool = unsafe { msg_send![&*self.wk, respondsToSelector: action] };
                        if responds {
                            let _: () = unsafe { msg_send![&*self.wk, performSelector: action, withObject: std::ptr::null::<AnyObject>()] };
                        }
                    }
                }
            }
        }
    }

    pub fn text(&self, text: &str) {
        let text = NSString::from_str(text);
        let range = NSRange::new(NSNotFound as usize, 0);
        let _: () = unsafe { msg_send![&*self.wk, insertText: &*text, replacementRange: range] };
    }

    pub fn close(self) {
        if let Some(layers) = &self.layers {
            gone(layers.surface_id());
        }
        drop(self.webview);
        self.window.close();
    }
}

fn gone(surface: i64) {
    api::push(api::Event { browser: 0, kind: EventKind::BufferGone, code: 0, value: surface, text: String::new(),
        detail: String::new() });
}

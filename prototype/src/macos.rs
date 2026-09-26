//! WKWebView through Wry: its layer tree rendered by CARenderer into an IOSurface, and WebKit snapshots.

use crate::common::*;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::*;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDataProvider, CGImage};
use objc2_foundation::*;
use objc2_io_surface::*;
use objc2_metal::*;
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
    #[name = "WryProbeWindow"]
    struct ProbeWindow;

    impl ProbeWindow {
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

pub fn run(mode: &str) {
    println!("probe: macos/{mode}, {}", NSProcessInfo::processInfo().operatingSystemVersionString());
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let on_screen = mode.ends_with("-alpha");
    let window = create_window(mtm, on_screen);
    let content = window.contentView().expect("content view");
    let parent = AppKitParent(NonNull::new(Retained::as_ptr(&content) as *mut c_void).unwrap());
    let webview = WebViewBuilder::new()
        .with_html(PAGE)
        .with_transparent(true)
        .with_bounds(wry::Rect {
            position: wry::dpi::LogicalPosition::new(0, 0).into(),
            size: wry::dpi::LogicalSize::new(WIDTH, HEIGHT).into(),
        })
        .build_as_child(&parent)
        .expect("webview");
    let wk: Retained<WKWebView> = Retained::into_super(webview.webview());

    // Lets the page load and paint before measuring
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_millis(1500) {
        pump(&app);
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Some(layer) = wk.layer() {
        println!("layer tree of the WKWebView:");
        dump_layers(&layer, 0, &mut 0);
    }

    match mode {
        "carenderer" | "carenderer-alpha" => carenderer(&app, &window, &wk, mode),
        "snapshot" => snapshot(&app, &wk, mtm),
        _ => panic!("unknown mode {mode}"),
    }
}

fn create_window(mtm: MainThreadMarker, on_screen: bool) -> Retained<ProbeWindow> {
    let x = if on_screen { 0.0 } else { -(WIDTH as f64) - 2000.0 };
    let rect = NSRect::new(NSPoint::new(x, 0.0), NSSize::new(WIDTH as f64, HEIGHT as f64));
    let window: Retained<ProbeWindow> = unsafe {
        msg_send![ProbeWindow::alloc(mtm), initWithContentRect: rect, styleMask: NSWindowStyleMask::Borderless,
            backing: NSBackingStoreType::Buffered, defer: false]
    };
    unsafe { window.setReleasedWhenClosed(false) };
    if on_screen {
        // On screen for WebKit, invisible and click-through for the player
        window.setAlphaValue(0.0);
        window.setIgnoresMouseEvents(true);
    }
    window.orderFrontRegardless();
    window
}

/// What SDL does for the game once a frame: runs the main run loop until no event is left.
fn pump(app: &NSApplication) {
    while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any, Some(&NSDate::distantPast()), unsafe { NSDefaultRunLoopMode }, true) {
        app.sendEvent(&event);
    }
}

fn dump_layers(layer: &CALayer, depth: usize, printed: &mut usize) {
    if *printed > 40 || depth > 8 {
        return;
    }
    *printed += 1;
    let contents = unsafe { layer.contents() }.map(|c| c.class().name().to_string_lossy().into_owned());
    let frame = layer.frame();
    println!("{}{} {}x{} contents={:?}", "  ".repeat(depth), layer.class().name().to_string_lossy(),
        frame.size.width, frame.size.height, contents);
    if let Some(sublayers) = unsafe { layer.sublayers() } {
        for sublayer in sublayers.iter() {
            dump_layers(&sublayer, depth + 1, printed);
        }
    }
}

fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
    t(usage.ru_utime) + t(usage.ru_stime)
}

fn page_fps(wk: &WKWebView) -> Option<u32> {
    let title = unsafe { wk.title() }?.to_string();
    title_field(&title, "fps")?.parse().ok()
}

/// Renders the WKWebView's layer tree with CARenderer into a Metal texture backed by an IOSurface, the kind of
/// surface OpenGL imports with CGLTexImageIOSurface2D.
fn carenderer(app: &NSApplication, window: &ProbeWindow, wk: &WKWebView, mode: &str) {
    let name = format!("macos {mode}");
    let mut stats = Stats::new(&name);
    let Some(device) = MTLCreateSystemDefaultDevice() else {
        println!("== RESULT {name} ==\nFAILED: no Metal device");
        return;
    };
    println!("Metal device: {}", device.name());

    let surface = create_surface();
    let surface_ref: &IOSurfaceRef = unsafe { &*(Retained::as_ptr(&surface) as *const IOSurfaceRef) };
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm, WIDTH as usize, HEIGHT as usize, false)
    };
    descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
    let texture = device.newTextureWithDescriptor_iosurface_plane(&descriptor, surface_ref, 0).expect("texture");
    let queue = device.newCommandQueue().expect("command queue");
    let queue_object: &AnyObject = unsafe { &*(Retained::as_ptr(&queue) as *const AnyObject) };
    let options = NSDictionary::<NSString, AnyObject>::from_slices(&[unsafe { kCARendererMetalCommandQueue }], &[queue_object]);
    let renderer = unsafe {
        CARenderer::rendererWithMTLTexture_options(&texture, Some(&*(Retained::as_ptr(&options) as *const NSDictionary)))
    };
    let layer = wk.layer().expect("layer-backed WKWebView");
    renderer.setLayer(Some(&layer));
    renderer.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(WIDTH as f64, HEIGHT as f64)));

    let mut cpu_start = None;
    let mut last_fps_sample = Instant::now();
    let mut attempts = 0u64;
    while !stats.done() {
        let frame_start = Instant::now();
        pump(app);

        let started = Instant::now();
        unsafe { renderer.beginFrameAtTime_timeStamp(CACurrentMediaTime(), std::ptr::null_mut()) };
        let dirty = renderer.updateBounds();
        let changed = dirty.size.width > 0.0 && dirty.size.height > 0.0;
        if changed {
            renderer.addUpdateRect(dirty);
            renderer.render();
        }
        renderer.endFrame();
        attempts += 1;
        if changed {
            let buffer = queue.commandBuffer().expect("command buffer");
            buffer.commit();
            buffer.waitUntilCompleted();
            stats.frame(ms(started), None);
        }

        if stats.measuring() {
            cpu_start.get_or_insert((Instant::now(), cpu_seconds()));
            if last_fps_sample.elapsed() >= Duration::from_secs(1) {
                last_fps_sample = Instant::now();
                if let Some(fps) = page_fps(wk) {
                    stats.page_fps.push(fps);
                }
            }
        }
        std::thread::sleep(Duration::from_micros(16_667).saturating_sub(frame_start.elapsed()));
    }

    let (pixels, stride) = read_surface(surface_ref);
    stats.notes.push(format!("{attempts} frames asked, IOSurface marker pixel ok: {}, background alpha: {}",
        marker_ok(&pixels, stride, true), background_alpha(&pixels, stride)));
    dump_frame(&mode.replace('-', "_"), &pixels, WIDTH, HEIGHT, stride, true);
    let cpu = cpu_start.map(|(at, start): (Instant, f64)| ((cpu_seconds() - start) / at.elapsed().as_secs_f64() * 100.0, f64::NAN));
    stats.report(cpu);

    input_check(app, window, wk);
}

fn create_surface() -> Retained<IOSurface> {
    let number = |value: usize| NSNumber::new_usize(value);
    let values = [number(WIDTH as usize), number(HEIGHT as usize), number(4), number(u32::from_be_bytes(*b"BGRA") as usize)];
    let objects: Vec<&AnyObject> = values.iter().map(|v| unsafe { &*(Retained::as_ptr(v) as *const AnyObject) }).collect();
    let keys = unsafe {
        [IOSurfacePropertyKeyWidth, IOSurfacePropertyKeyHeight, IOSurfacePropertyKeyBytesPerElement,
            IOSurfacePropertyKeyPixelFormat]
    };
    let properties = NSDictionary::<IOSurfacePropertyKey, AnyObject>::from_slices(&keys, &objects);
    IOSurface::initWithProperties(IOSurface::alloc(), &properties).expect("IOSurface")
}

fn read_surface(surface: &IOSurfaceRef) -> (Vec<u8>, usize) {
    unsafe {
        surface.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
        let stride = surface.bytes_per_row();
        let pixels = std::slice::from_raw_parts(surface.base_address().as_ptr() as *const u8, stride * HEIGHT as usize).to_vec();
        surface.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
        (pixels, stride)
    }
}

/// Clicks the text field, types and clicks the button with events sent straight to the window, which never
/// becomes the real key window.
fn input_check(app: &NSApplication, window: &ProbeWindow, wk: &WKWebView) {
    let uptime = || NSProcessInfo::processInfo().systemUptime();
    let number = window.windowNumber();
    window.makeFirstResponder(Some(wk));
    let click = |(x, y): (f64, f64)| {
        for kind in [NSEventType::LeftMouseDown, NSEventType::LeftMouseUp] {
            let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind, NSPoint::new(x, HEIGHT as f64 - y), NSEventModifierFlags::empty(), uptime(), number, None, 0, 1, 1.0);
            if let Some(event) = event {
                window.sendEvent(&event);
            }
        }
    };
    let settle = |ms: u64| {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            pump(app);
            std::thread::sleep(Duration::from_millis(5));
        }
    };

    click(INPUT_FIELD);
    settle(300);
    for (character, key_code) in [("w", 13u16), ("r", 15), ("y", 16)] {
        let text = NSString::from_str(character);
        for kind in [NSEventType::KeyDown, NSEventType::KeyUp] {
            let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                kind, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(), uptime(), number, None, &text, &text, false, key_code);
            if let Some(event) = event {
                window.sendEvent(&event);
            }
        }
    }
    settle(300);
    click(INPUT_BUTTON);
    settle(500);
    let title = unsafe { wk.title() }.map(|t| t.to_string()).unwrap_or_default();
    println!("input without focus: {} (title '{title}')", if input_ok(&title) { "ok" } else { "FAILED" });
}

struct SnapshotFrame {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
    stride: usize,
}

/// WKWebView.takeSnapshot, asked at most once per 60 Hz frame.
fn snapshot(app: &NSApplication, wk: &WKWebView, mtm: MainThreadMarker) {
    let stats = Rc::new(RefCell::new(Stats::new("macos takeSnapshot (CPU copy)")));
    let last: Rc<RefCell<Option<SnapshotFrame>>> = Rc::new(RefCell::new(None));
    let pending = Rc::new(RefCell::new(false));
    let config = unsafe { WKSnapshotConfiguration::new(mtm) };

    let mut cpu_start = None;
    let mut last_fps_sample = Instant::now();
    let mut last_request = Instant::now() - Duration::from_secs(1);
    while !stats.borrow().done() {
        pump(app);
        if !*pending.borrow() && last_request.elapsed() >= Duration::from_micros(16_667) {
            last_request = Instant::now();
            *pending.borrow_mut() = true;
            let started = Instant::now();
            let (stats, last, pending) = (stats.clone(), last.clone(), pending.clone());
            let block = RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
                *pending.borrow_mut() = false;
                let Some(image) = (unsafe { image.as_ref() }) else { return };
                let Some(cg) = (unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }) else { return };
                let data = CGDataProvider::data(CGImage::data_provider(Some(&cg)).as_deref()).map(|d| d.to_vec()).unwrap_or_default();
                stats.borrow_mut().frame(ms(started), None);
                *last.borrow_mut() = Some(SnapshotFrame {
                    pixels: data,
                    width: CGImage::width(Some(&cg)),
                    height: CGImage::height(Some(&cg)),
                    stride: CGImage::bytes_per_row(Some(&cg)),
                });
            });
            unsafe { wk.takeSnapshotWithConfiguration_completionHandler(Some(&config), &block) };
        }

        let mut s = stats.borrow_mut();
        if s.measuring() {
            cpu_start.get_or_insert((Instant::now(), cpu_seconds()));
            if last_fps_sample.elapsed() >= Duration::from_secs(1) {
                last_fps_sample = Instant::now();
                if let Some(fps) = page_fps(wk) {
                    s.page_fps.push(fps);
                }
            }
        }
        drop(s);
        std::thread::sleep(Duration::from_millis(1));
    }

    let mut s = stats.borrow_mut();
    if let Some(frame) = last.borrow().as_ref() {
        s.notes.push(format!("frame {}x{} (points {}x{}), marker pixel ok: {}", frame.width, frame.height, WIDTH, HEIGHT,
            marker_ok(&frame.pixels, frame.stride, true) || marker_ok(&frame.pixels, frame.stride, false)));
        dump_frame("macos_snapshot", &frame.pixels, frame.width as u32, frame.height as u32, frame.stride, true);
    }
    let cpu = cpu_start.map(|(at, start): (Instant, f64)| ((cpu_seconds() - start) / at.elapsed().as_secs_f64() * 100.0, f64::NAN));
    s.report(cpu);
}

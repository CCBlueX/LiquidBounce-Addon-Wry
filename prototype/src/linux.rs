//! WebKitGTK through Wry: off-screen window readback, WebKit snapshots and an X11 child window.

use crate::common::*;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Instant;
use webkit2gtk::{SettingsExt, SnapshotOptions, SnapshotRegion, WebViewExt};
use wry::{WebViewBuilder, WebViewBuilderExtUnix, WebViewExtUnix};

pub fn run(mode: &str) {
    let version = unsafe {
        (webkit2gtk::ffi::webkit_get_major_version(), webkit2gtk::ffi::webkit_get_minor_version(),
            webkit2gtk::ffi::webkit_get_micro_version())
    };
    println!("probe: linux/{mode}, WebKitGTK {version:?}");
    match mode {
        "offscreen" => offscreen(),
        "snapshot" => snapshot(),
        "child" => child(),
        "nested" => crate::nested::run(),
        _ => panic!("unknown mode {mode}"),
    }
}

struct Frame {
    data: Vec<u8>,
    width: u32,
    height: u32,
    stride: usize,
}

/// A GL context on its own thread, standing in for the game's render thread, which uploads every
/// frame it is handed into a texture.
fn spawn_uploader() -> (mpsc::SyncSender<Frame>, mpsc::Receiver<f64>) {
    let (frames_tx, frames_rx) = mpsc::sync_channel::<Frame>(2);
    let (times_tx, times_rx) = mpsc::channel();

    std::thread::spawn(move || {
        let egl = unsafe { khronos_egl::DynamicInstance::<khronos_egl::EGL1_4>::load_required() }.expect("libEGL");
        let display = unsafe { egl.get_display(khronos_egl::DEFAULT_DISPLAY) }.expect("EGL display");
        egl.initialize(display).expect("eglInitialize");
        egl.bind_api(khronos_egl::OPENGL_API).expect("OpenGL API");
        let config = egl.choose_first_config(display, &[
            khronos_egl::SURFACE_TYPE, khronos_egl::PBUFFER_BIT,
            khronos_egl::RENDERABLE_TYPE, khronos_egl::OPENGL_BIT,
            khronos_egl::RED_SIZE, 8, khronos_egl::GREEN_SIZE, 8, khronos_egl::BLUE_SIZE, 8,
            khronos_egl::NONE,
        ]).unwrap().expect("EGL config");
        let surface = egl.create_pbuffer_surface(display, config,
            &[khronos_egl::WIDTH, 1, khronos_egl::HEIGHT, 1, khronos_egl::NONE]).unwrap();
        let context = egl.create_context(display, config, None, &[khronos_egl::NONE]).unwrap();
        egl.make_current(display, Some(surface), Some(surface), Some(context)).unwrap();

        type GenTextures = extern "system" fn(i32, *mut u32);
        type BindTexture = extern "system" fn(u32, u32);
        type TexImage2D = extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const u8);
        type TexSubImage2D = extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const u8);
        type PixelStorei = extern "system" fn(u32, i32);
        type Finish = extern "system" fn();
        type GetString = extern "system" fn(u32) -> *const i8;
        macro_rules! gl {
            ($name:literal, $ty:ty) => {
                unsafe { std::mem::transmute::<_, $ty>(egl.get_proc_address($name).expect($name)) }
            };
        }
        let gen_textures = gl!("glGenTextures", GenTextures);
        let bind_texture = gl!("glBindTexture", BindTexture);
        let tex_image = gl!("glTexImage2D", TexImage2D);
        let tex_sub_image = gl!("glTexSubImage2D", TexSubImage2D);
        let pixel_store = gl!("glPixelStorei", PixelStorei);
        let finish = gl!("glFinish", Finish);
        let get_string = gl!("glGetString", GetString);
        let renderer = unsafe { std::ffi::CStr::from_ptr(get_string(0x1F01)) }.to_string_lossy().into_owned();
        println!("uploader: GL renderer {renderer}");

        const TEXTURE_2D: u32 = 0x0DE1;
        const RGBA8: i32 = 0x8058;
        const BGRA: u32 = 0x80E1;
        const UNSIGNED_BYTE: u32 = 0x1401;
        const UNPACK_ROW_LENGTH: u32 = 0x0CF2;

        let mut texture = 0;
        gen_textures(1, &mut texture);
        bind_texture(TEXTURE_2D, texture);
        let mut size = (0, 0);
        for frame in frames_rx {
            let started = Instant::now();
            if size != (frame.width, frame.height) {
                size = (frame.width, frame.height);
                tex_image(TEXTURE_2D, 0, RGBA8, frame.width as i32, frame.height as i32, 0, BGRA, UNSIGNED_BYTE,
                    std::ptr::null());
            }
            pixel_store(UNPACK_ROW_LENGTH, (frame.stride / 4) as i32);
            tex_sub_image(TEXTURE_2D, 0, 0, 0, frame.width as i32, frame.height as i32, BGRA, UNSIGNED_BYTE,
                frame.data.as_ptr());
            finish();
            if times_tx.send(ms(started)).is_err() {
                break;
            }
        }
    });

    (frames_tx, times_rx)
}

/// Copies a cairo surface into memory, the way the add-on would hand it to the render thread.
fn read_surface(surface: &cairo::Surface) -> Frame {
    let image = surface.map_to_image(None).expect("map_to_image");
    let (width, height, stride) = (image.width() as u32, image.height() as u32, image.stride() as usize);
    let mut data = Vec::new();
    image.with_data(|pixels| data.extend_from_slice(pixels)).unwrap();
    Frame { data, width, height, stride }
}

fn build_offscreen_webview() -> (gtk::OffscreenWindow, wry::WebView) {
    gtk::init().expect("gtk::init");
    let window = gtk::OffscreenWindow::new();
    // Keeps the page's transparency instead of the theme's window background
    window.set_app_paintable(true);
    if let Some(visual) = WidgetExt::screen(&window).and_then(|screen| screen.rgba_visual()) {
        window.set_visual(Some(&visual));
    }
    window.connect_draw(|_, cr| {
        cr.set_operator(cairo::Operator::Source);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
        cr.paint().ok();
        glib::Propagation::Proceed
    });
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_size_request(WIDTH as i32, HEIGHT as i32);
    window.add(&container);
    let webview = WebViewBuilder::new()
        .with_html(PAGE)
        .with_transparent(true)
        .build_gtk(&container)
        .expect("webview");
    window.show_all();

    let settings = WebViewExt::settings(&webview.webview()).unwrap();
    println!("hardware acceleration policy: {:?}", settings.hardware_acceleration_policy());
    (window, webview)
}

/// Samples the frame rate the page reports in its title.
fn page_fps(webview: &webkit2gtk::WebView) -> Option<u32> {
    title_field(&webview.title()?, "fps")?.parse().ok()
}

fn offscreen() {
    let (window, webview) = build_offscreen_webview();
    let wk = webview.webview();
    let (uploads, upload_times) = spawn_uploader();
    let stats = Rc::new(RefCell::new(Stats::new("linux offscreen-window readback")));
    let last = Rc::new(RefCell::new(None::<Frame>));
    let cpu = Rc::new(RefCell::new(None));

    {
        let stats = stats.clone();
        let last = last.clone();
        window.connect_damage_event(move |window, _| {
            let started = Instant::now();
            let Some(surface) = window.surface() else { return false };
            if stats.borrow().measuring() && last.borrow().is_none() {
                println!("offscreen surface type: {:?}", surface.type_());
            }
            let frame = read_surface(&surface);
            let grab = ms(started);
            let copy = Frame { data: frame.data.clone(), ..frame };
            uploads.try_send(copy).ok();
            stats.borrow_mut().frame(grab, None);
            *last.borrow_mut() = Some(frame);
            false
        });
    }

    let upload_ms = Rc::new(RefCell::new(Vec::new()));
    let window = window.clone();
    glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
        upload_ms.borrow_mut().extend(upload_times.try_iter());
        let mut stats = stats.borrow_mut();
        if stats.measuring() {
            if cpu.borrow().is_none() {
                *cpu.borrow_mut() = Some(CpuSample::now());
            }
            if let Some(fps) = page_fps(&wk) {
                stats.page_fps.push(fps);
            }
        }
        if stats.done() {
            let usage = cpu.borrow().as_ref().map(CpuSample::usage);
            let uploads = upload_ms.borrow();
            let mut sorted = uploads.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if !sorted.is_empty() {
                stats.notes.push(format!("GL upload of each frame on a separate thread: avg {:.2} ms, max {:.2} ms (n={})",
                    sorted.iter().sum::<f64>() / sorted.len() as f64, sorted[sorted.len() - 1], sorted.len()));
            }
            if let Some(frame) = last.borrow().as_ref() {
                stats.notes.push(format!("frame {}x{}, marker pixel ok: {}, background alpha: {}", frame.width,
                    frame.height, marker_ok(&frame.data, frame.stride, true), background_alpha(&frame.data, frame.stride)));
                dump_frame("linux-offscreen", &frame.data, frame.width, frame.height, frame.stride, true);
            }
            stats.report(usage);
            input_check(&window, &wk);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });

    gtk::main();
    drop(webview);
}

fn snapshot() {
    let (_window, webview) = build_offscreen_webview();
    let wk = webview.webview();
    let (uploads, _upload_times) = spawn_uploader();
    let stats = Rc::new(RefCell::new(Stats::new("linux webkit_web_view_get_snapshot")));
    let cpu = Rc::new(RefCell::new(None));

    fn next(wk: webkit2gtk::WebView, stats: Rc<RefCell<Stats>>, uploads: mpsc::SyncSender<Frame>,
            cpu: Rc<RefCell<Option<CpuSample>>>) {
        let started = Instant::now();
        let view = wk.clone();
        wk.snapshot(SnapshotRegion::Visible, SnapshotOptions::TRANSPARENT_BACKGROUND, None::<&gtk::gio::Cancellable>,
            move |result| {
                let surface = result.expect("snapshot");
                let frame = read_surface(&surface);
                let grab = ms(started);
                {
                    let mut s = stats.borrow_mut();
                    s.frame(grab, None);
                    if s.measuring() && cpu.borrow().is_none() {
                        *cpu.borrow_mut() = Some(CpuSample::now());
                    }
                    if s.measuring() && s.page_fps.len() < 10 {
                        if let Some(fps) = page_fps(&view) {
                            s.page_fps.push(fps);
                        }
                    }
                    if s.done() {
                        s.notes.push(format!("frame {}x{}, marker pixel ok: {}, background alpha: {}", frame.width,
                            frame.height, marker_ok(&frame.data, frame.stride, true),
                            background_alpha(&frame.data, frame.stride)));
                        dump_frame("linux-snapshot", &frame.data, frame.width, frame.height, frame.stride, true);
                        s.report(cpu.borrow().as_ref().map(CpuSample::usage));
                        gtk::main_quit();
                        return;
                    }
                }
                uploads.try_send(frame).ok();
                // Paced to the frame rate a game would ask at
                let wait = std::time::Duration::from_micros(16_667).saturating_sub(started.elapsed());
                glib::timeout_add_local_once(wait, move || next(view, stats, uploads, cpu));
            });
    }

    // Starts once the page has painted
    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || next(wk, stats, uploads, cpu));
    gtk::main();
}

struct XlibParent(std::os::raw::c_ulong);

impl raw_window_handle::HasWindowHandle for XlibParent {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let handle = raw_window_handle::XlibWindowHandle::new(self.0);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Xlib(handle)) })
    }
}

/// Wry's own mode: the webview as a child of a window, standing in for the game's window.
fn child() {
    let xlib = x11_dl::xlib::Xlib::open().expect("Xlib");
    let window = unsafe {
        let display = (xlib.XOpenDisplay)(std::ptr::null());
        let root = (xlib.XDefaultRootWindow)(display);
        let window = (xlib.XCreateSimpleWindow)(display, root, 0, 0, WIDTH, HEIGHT, 0, 0, 0x203040);
        (xlib.XMapWindow)(display, window);
        (xlib.XFlush)(display);
        window
    };

    gtk::init().expect("gtk::init");
    let parent = XlibParent(window);
    let webview = WebViewBuilder::new()
        .with_html(PAGE)
        .with_bounds(wry::Rect {
            position: wry::dpi::LogicalPosition::new(0, 0).into(),
            size: wry::dpi::LogicalSize::new(WIDTH, HEIGHT).into(),
        })
        .build_as_child(&parent)
        .expect("child webview");

    glib::timeout_add_local_once(std::time::Duration::from_secs(6), move || {
        std::fs::create_dir_all("out").ok();
        let status = std::process::Command::new("import")
            .args(["-window", "root", "out/linux-child.png"])
            .status();
        println!("== RESULT linux child window ==");
        println!("screenshot of the X screen: {status:?}");
        println!("note: the page is a separate X11 window stacked on the parent; the parent cannot draw over it or blend it");
        gtk::main_quit();
    });
    gtk::main();
    drop(webview);
}

/// CPU time of this process and of the WebKit processes it started.
pub(crate) struct CpuSample {
    at: Instant,
    own: f64,
    children: f64,
}

impl CpuSample {
    pub(crate) fn now() -> Self {
        let pid = std::process::id();
        let own = proc_cpu(pid).unwrap_or(0.0);
        let children = descendants(pid).into_iter().filter_map(proc_cpu).sum();
        Self { at: Instant::now(), own, children }
    }

    /// Percent of one core since the sample was taken.
    pub(crate) fn usage(&self) -> (f64, f64) {
        let now = Self::now();
        let seconds = self.at.elapsed().as_secs_f64();
        ((now.own - self.own) / seconds * 100.0, (now.children - self.children) / seconds * 100.0)
    }
}

fn proc_stat(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    Some(rest.split_whitespace().map(str::to_owned).collect())
}

fn proc_cpu(pid: u32) -> Option<f64> {
    let fields = proc_stat(pid)?;
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    // utime and stime, fields 14 and 15 of the stat line
    Some((fields[11].parse::<f64>().ok()? + fields[12].parse::<f64>().ok()?) / ticks)
}

fn descendants(root: u32) -> Vec<u32> {
    let parents: Vec<(u32, u32)> = std::fs::read_dir("/proc").into_iter().flatten().flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| Some((pid, proc_stat(pid)?[1].parse().ok()?)))
        .collect();
    let mut found = vec![root];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        found.extend(parents.iter().filter(|(_, ppid)| *ppid == parent).map(|(pid, _)| *pid));
        i += 1;
    }
    found.remove(0);
    found
}

/// Clicks the text field, types and clicks the button with GDK events handed to GTK directly, a moment apart
/// like real input. The off-screen window is never focused by the window manager, it is only told it has focus.
fn input_check(window: &gtk::OffscreenWindow, wk: &webkit2gtk::WebView) {
    let wk = wk.clone();
    focus(window, &wk);
    click(&wk, INPUT_FIELD);
    let after = |ms, f: Box<dyn FnOnce()>| glib::timeout_add_local_once(std::time::Duration::from_millis(ms), f);
    let view = wk.clone();
    after(300, Box::new(move || {
        type_text(&view, INPUT_TEXT);
        let view = view.clone();
        after(300, Box::new(move || {
            click(&view, INPUT_BUTTON);
            let view = view.clone();
            after(500, Box::new(move || {
                let title = view.title().map(|t| t.to_string()).unwrap_or_default();
                println!("input without focus: {} (title '{title}')", if input_ok(&title) { "ok" } else { "FAILED" });
                gtk::main_quit();
            }));
        }));
    }));
}

fn send(event: *mut gtk::gdk::ffi::GdkEvent) {
    unsafe {
        // GDK asks the X server about the pointer of the off-screen window, which it doesn't have
        gtk::gdk::ffi::gdk_error_trap_push();
        gtk::ffi::gtk_main_do_event(event);
        gtk::gdk::ffi::gdk_event_free(event);
        gtk::gdk::ffi::gdk_error_trap_pop_ignored();
    }
}

fn seat() -> gtk::gdk::Seat {
    gtk::gdk::Display::default().and_then(|display| display.default_seat()).expect("seat")
}

/// Tells the window it is the focused toplevel, so WebKit shows the caret and takes keys.
fn focus(window: &gtk::OffscreenWindow, wk: &webkit2gtk::WebView) {
    use gtk::gdk::ffi as gdk_ffi;
    use gtk::glib::translate::ToGlibPtr;
    GtkWindowExt::set_focus(window, Some(wk));
    unsafe {
        let focus = gdk_ffi::gdk_event_new(gdk_ffi::GDK_FOCUS_CHANGE);
        (*focus).focus_change.window = WidgetExt::window(window).unwrap().to_glib_full();
        (*focus).focus_change.in_ = 1;
        gdk_ffi::gdk_event_set_device(focus, seat().keyboard().unwrap().to_glib_none().0);
        send(focus);
    }
}

fn click(wk: &webkit2gtk::WebView, (x, y): (f64, f64)) {
    use gtk::gdk::ffi as gdk_ffi;
    use gtk::glib::translate::ToGlibPtr;
    let target = WidgetExt::window(wk).expect("GdkWindow");
    let pointer = seat().pointer().unwrap();
    for kind in [gdk_ffi::GDK_MOTION_NOTIFY, gdk_ffi::GDK_BUTTON_PRESS, gdk_ffi::GDK_BUTTON_RELEASE] {
        unsafe {
            let event = gdk_ffi::gdk_event_new(kind);
            if kind == gdk_ffi::GDK_MOTION_NOTIFY {
                let motion = &mut (*event).motion;
                motion.window = target.to_glib_full();
                (motion.x, motion.y, motion.time) = (x, y, gtk::current_event_time());
            } else {
                let button = &mut (*event).button;
                button.window = target.to_glib_full();
                (button.x, button.y, button.button, button.time) = (x, y, 1, gtk::current_event_time());
            }
            gdk_ffi::gdk_event_set_device(event, pointer.to_glib_none().0);
            send(event);
        }
    }
}

fn type_text(wk: &webkit2gtk::WebView, text: &str) {
    use gtk::gdk::ffi as gdk_ffi;
    use gtk::glib::translate::ToGlibPtr;
    let target = WidgetExt::window(wk).expect("GdkWindow");
    let keyboard = seat().keyboard().unwrap();
    let keymap = gtk::gdk::Keymap::for_display(&target.display()).expect("keymap");
    for character in text.chars() {
        let keyval = unsafe { gdk_ffi::gdk_unicode_to_keyval(character as u32) };
        let keycode = keymap.entries_for_keyval(keyval.into()).first().map(|k| k.keycode()).unwrap_or(0);
        for kind in [gdk_ffi::GDK_KEY_PRESS, gdk_ffi::GDK_KEY_RELEASE] {
            unsafe {
                let event = gdk_ffi::gdk_event_new(kind);
                let key = &mut (*event).key;
                key.window = target.to_glib_full();
                key.keyval = keyval;
                key.hardware_keycode = keycode as u16;
                key.time = gtk::current_event_time();
                gdk_ffi::gdk_event_set_device(event, keyboard.to_glib_none().0);
                send(event);
            }
        }
    }
}

/// Imports a dmabuf into a GL texture on its own EGL context, as the game would on its render thread,
/// and reads the marker back from the GPU.
pub fn import_dmabuf(fds: &[std::os::fd::OwnedFd], offsets: &[u32], strides: &[u32], fourcc: u32, modifier: u64,
                     width: u32, height: u32) -> String {
    use std::os::fd::AsRawFd;
    let egl = match unsafe { khronos_egl::DynamicInstance::<khronos_egl::EGL1_5>::load_required() } {
        Ok(egl) => egl,
        Err(e) => return format!("no EGL 1.5: {e}"),
    };
    const PLATFORM_SURFACELESS_MESA: khronos_egl::Enum = 0x31DD;
    let display = unsafe { egl.get_platform_display(PLATFORM_SURFACELESS_MESA, khronos_egl::DEFAULT_DISPLAY, &[khronos_egl::ATTRIB_NONE]) }
        .or_else(|_| unsafe { egl.get_display(khronos_egl::DEFAULT_DISPLAY) }.ok_or(khronos_egl::Error::BadDisplay));
    let Ok(display) = display else { return "no EGL display".into() };
    if egl.initialize(display).is_err() {
        return "eglInitialize failed".into();
    }
    egl.bind_api(khronos_egl::OPENGL_API).ok();
    let Ok(Some(config)) = egl.choose_first_config(display, &[khronos_egl::RENDERABLE_TYPE, khronos_egl::OPENGL_BIT, khronos_egl::NONE]) else {
        return "no EGL config".into();
    };
    let Ok(context) = egl.create_context(display, config, None, &[khronos_egl::NONE]) else { return "no EGL context".into() };
    if egl.make_current(display, None, None, Some(context)).is_err() {
        return "surfaceless make_current failed".into();
    }

    let mut attribs = vec![0x3057, width as i32, 0x3056, height as i32, 0x3271, fourcc as i32];
    let plane_attribs = [(0x3272, 0x3273, 0x3274, 0x3443, 0x3444), (0x3275, 0x3276, 0x3277, 0x3445, 0x3446)];
    for (i, fd) in fds.iter().enumerate().take(2) {
        let (a_fd, a_offset, a_pitch, a_lo, a_hi) = plane_attribs[i];
        attribs.extend([a_fd, fd.as_raw_fd(), a_offset, offsets[i] as i32, a_pitch, strides[i] as i32]);
        if modifier != 0x00ff_ffff_ffff_ffff {
            attribs.extend([a_lo, modifier as u32 as i32, a_hi, (modifier >> 32) as u32 as i32]);
        }
    }
    attribs.push(0x3038);

    type CreateImage = extern "system" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, u32, *mut std::ffi::c_void, *const i32) -> *mut std::ffi::c_void;
    type TargetTexture = extern "system" fn(u32, *mut std::ffi::c_void);
    type GenObjects = extern "system" fn(i32, *mut u32);
    type Bind = extern "system" fn(u32, u32);
    type FramebufferTexture = extern "system" fn(u32, u32, u32, u32, i32);
    type ReadPixels = extern "system" fn(i32, i32, i32, i32, u32, u32, *mut u8);
    macro_rules! proc {
        ($name:literal, $ty:ty) => {
            match egl.get_proc_address($name) {
                Some(f) => unsafe { std::mem::transmute::<_, $ty>(f) },
                None => return format!("{} missing", $name),
            }
        };
    }
    let create_image = proc!("eglCreateImageKHR", CreateImage);
    let target_texture = proc!("glEGLImageTargetTexture2DOES", TargetTexture);
    let gen_textures = proc!("glGenTextures", GenObjects);
    let bind_texture = proc!("glBindTexture", Bind);
    let gen_framebuffers = proc!("glGenFramebuffers", GenObjects);
    let bind_framebuffer = proc!("glBindFramebuffer", Bind);
    let framebuffer_texture = proc!("glFramebufferTexture2D", FramebufferTexture);
    let read_pixels = proc!("glReadPixels", ReadPixels);

    let started = Instant::now();
    let image = create_image(display.as_ptr(), std::ptr::null_mut(), 0x3270, std::ptr::null_mut(), attribs.as_ptr());
    if image.is_null() {
        return format!("eglCreateImageKHR failed ({:?}), fourcc {fourcc:#x} modifier {modifier:#x}", egl.get_error());
    }
    let (mut texture, mut framebuffer) = (0, 0);
    gen_textures(1, &mut texture);
    bind_texture(0x0DE1, texture);
    target_texture(0x0DE1, image);
    let import_ms = ms(started);
    gen_framebuffers(1, &mut framebuffer);
    bind_framebuffer(0x8D40, framebuffer);
    framebuffer_texture(0x8D40, 0x8CE0, 0x0DE1, texture, 0);
    let mut pixel = [0u8; 4];
    read_pixels(8, 8, 1, 1, 0x1908, 0x1401, pixel.as_mut_ptr());
    format!("imported {width}x{height} fourcc {fourcc:#x} modifier {modifier:#x} in {import_ms:.2} ms without a copy, \
        pixel at the marker {pixel:?} (expect red)")
}

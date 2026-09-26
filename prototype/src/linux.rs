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
            gtk::main_quit();
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
struct CpuSample {
    at: Instant,
    own: f64,
    children: f64,
}

impl CpuSample {
    fn now() -> Self {
        let pid = std::process::id();
        let own = proc_cpu(pid).unwrap_or(0.0);
        let children = descendants(pid).into_iter().filter_map(proc_cpu).sum();
        Self { at: Instant::now(), own, children }
    }

    /// Percent of one core since the sample was taken.
    fn usage(&self) -> (f64, f64) {
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

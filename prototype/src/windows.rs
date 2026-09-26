//! WebView2: composition hosting captured on the GPU, window capture, and CapturePreview.

use crate::common::*;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    take_pwstr, CallDevToolsProtocolMethodCompletedHandler, CapturePreviewCompletedHandler,
    CreateCoreWebView2CompositionControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
};
use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Com::IStream;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Composition::{Compositor, ContainerVisual, Visual};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

pub fn run(mode: &str) {
    println!("probe: windows/{mode}");
    // Chromium stops painting windows it thinks are covered; the probe's window is never on screen
    unsafe { std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", "--disable-features=CalculateNativeWinOcclusion") };
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().expect("CoInitializeEx") };
    if let Ok(version) = wry::webview_version() {
        println!("WebView2 runtime {version}");
    }
    println!("Windows.Graphics.Capture supported: {:?}", GraphicsCaptureSession::IsSupported());

    let result = match mode {
        "visual" => visual(false),
        "visual-window" => visual(true),
        "window" => window_capture(),
        "preview" => preview(),
        _ => panic!("unknown mode {mode}"),
    };
    if let Err(error) = result {
        println!("== RESULT windows {mode} ==");
        println!("FAILED: {error}");
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A popup window standing in for the game's window. Shown to the left of every screen when asked.
fn create_window(show: bool) -> Res<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = windows::core::w!("WryProbe");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        });
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN) - WIDTH as i32 - 100;
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            windows::core::w!("probe"),
            WS_POPUP,
            x, 0, WIDTH as i32, HEIGHT as i32,
            None, None, Some(instance.into()), None,
        )?;
        if show {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(hwnd)
    }
}

fn create_device(driver: D3D_DRIVER_TYPE) -> Res<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(None, driver, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None,
            D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))?;
    }
    Ok((device.unwrap(), context.unwrap()))
}

fn devices() -> Res<(ID3D11Device, ID3D11DeviceContext)> {
    let (device, context) = create_device(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| create_device(D3D_DRIVER_TYPE_WARP))?;
    let adapter = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()? };
    println!("D3D11 adapter: {}", String::from_utf16_lossy(&adapter.Description).trim_end_matches('\0'));
    Ok((device, context))
}

fn environment() -> Res<ICoreWebView2Environment> {
    let (tx, rx) = mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(|handler| unsafe { CreateCoreWebView2Environment(&handler).map_err(webview2_com::Error::WindowsError) }),
        Box::new(move |result, environment| {
            result?;
            tx.send(environment.expect("environment")).ok();
            Ok(())
        }),
    )?;
    Ok(rx.recv()?)
}

fn navigate(webview: &ICoreWebView2) -> Res<()> {
    let html = HSTRING::from(PAGE);
    unsafe { webview.NavigateToString(PCWSTR(html.as_ptr()))? };
    Ok(())
}

fn title(webview: &ICoreWebView2) -> String {
    let mut title = PWSTR::null();
    unsafe {
        if webview.DocumentTitle(&mut title).is_err() {
            return String::new();
        }
    }
    take_pwstr(title)
}

/// A texture another device or API can open, as the game's OpenGL would with GL_EXT_memory_object_win32.
struct SharedTexture {
    texture: ID3D11Texture2D,
    /// The NT handle, kept as a number so the texture can move between threads
    handle: isize,
    size: (u32, u32),
}

impl SharedTexture {
    fn new(device: &ID3D11Device, width: u32, height: u32) -> Res<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.unwrap();
        let handle = unsafe {
            texture.cast::<IDXGIResource1>()?.CreateSharedHandle(None, (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0, None)?
        };
        Ok(Self { texture, handle: handle.0 as isize, size: (width, height) })
    }
}

/// Opens the shared texture on a second device and reads it back, proving the handle carries the page.
fn verify_shared(handle: isize, width: u32, height: u32, name: &str) -> Res<String> {
    let (device, context) = create_device(D3D_DRIVER_TYPE_HARDWARE).or_else(|_| create_device(D3D_DRIVER_TYPE_WARP))?;
    let opened: ID3D11Texture2D = unsafe { device.cast::<ID3D11Device1>()?.OpenSharedResource1(HANDLE(handle as _))? };
    let (pixels, stride) = read_back(&device, &context, &opened, width, height)?;
    dump_frame(name, &pixels, width, height, stride, true);
    Ok(format!("opened on a second D3D11 device via NT handle, marker pixel ok: {}, background alpha: {}",
        marker_ok(&pixels, stride, true), background_alpha(&pixels, stride)))
}

fn read_back(device: &ID3D11Device, context: &ID3D11DeviceContext, texture: &ID3D11Texture2D, width: u32,
             height: u32) -> Res<(Vec<u8>, usize)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let stride = mapped.RowPitch as usize;
        let pixels = std::slice::from_raw_parts(mapped.pData as *const u8, stride * height as usize).to_vec();
        context.Unmap(&staging, 0);
        Ok((pixels, stride))
    }
}

struct Capture {
    stats: Stats,
    shared: Option<SharedTexture>,
    first_frame: Option<f64>,
}

/// Captures an item into a shared texture on every frame Windows.Graphics.Capture delivers.
fn start_capture(item: &GraphicsCaptureItem, device: &ID3D11Device, context: &ID3D11DeviceContext,
                 state: Arc<Mutex<Capture>>) -> Res<(Direct3D11CaptureFramePool, GraphicsCaptureSession)> {
    let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)?.cast()? };
    let size = item.Size()?;
    println!("capture item size {}x{}", size.Width, size.Height);
    let pool = Direct3D11CaptureFramePool::Create(&winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
    let session = pool.CreateCaptureSession(item)?;
    session.SetIsCursorCaptureEnabled(false).ok();
    if let Err(error) = session.SetIsBorderRequired(false) {
        println!("capture border can't be turned off: {error}");
    }

    let started = Instant::now();
    let device = device.clone();
    let context = context.clone();
    pool.FrameArrived(&TypedEventHandler::new(move |pool: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
        let pool = pool.ok()?;
        let frame = pool.TryGetNextFrame()?;
        let timer = Instant::now();
        let texture: ID3D11Texture2D = unsafe { frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };

        let mut state = state.lock().unwrap();
        if state.first_frame.is_none() {
            state.first_frame = Some(ms(started));
        }
        if state.shared.as_ref().map(|s| s.size) != Some((desc.Width, desc.Height)) {
            state.shared = SharedTexture::new(&device, desc.Width, desc.Height).ok();
        }
        if let Some(shared) = &state.shared {
            unsafe {
                // A GPU copy into the texture the game would sample
                context.CopyResource(&shared.texture, &texture);
                context.Flush();
            }
        }
        state.stats.frame(ms(timer), None);
        frame.Close()?;
        Ok(())
    }))?;
    session.StartCapture()?;
    Ok((pool, session))
}

/// Runs the message loop, calling `tick` once a second until it returns false.
fn pump(mut tick: impl FnMut() -> bool) {
    unsafe {
        SetTimer(None, 0, 1000, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_TIMER && msg.hwnd.is_invalid() && !tick() {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn wait(ms: u64) {
    let until = Instant::now() + std::time::Duration::from_millis(ms);
    unsafe {
        let mut msg = MSG::default();
        while Instant::now() < until {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

fn cpu_seconds() -> f64 {
    let (mut creation, mut exit, mut kernel, mut user) = Default::default();
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut creation, &mut exit, &mut kernel, &mut user).ok() };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    t(kernel) + t(user)
}

fn report(state: &Arc<Mutex<Capture>>, webview: &ICoreWebView2, cpu_start: &mut Option<(Instant, f64)>) -> bool {
    let mut state = state.lock().unwrap();
    if state.stats.measuring() {
        cpu_start.get_or_insert((Instant::now(), cpu_seconds()));
        if let Some(fps) = title_field(&title(webview), "fps").and_then(|f| f.parse().ok()) {
            state.stats.page_fps.push(fps);
        }
    }
    !state.stats.done()
}

fn finish(state: &Arc<Mutex<Capture>>, cpu_start: Option<(Instant, f64)>, name: &str) {
    let mut state = state.lock().unwrap();
    let first = state.first_frame;
    state.stats.notes.push(format!("first frame after {first:?} ms"));
    if let Some(shared) = &state.shared {
        let note = verify_shared(shared.handle, shared.size.0, shared.size.1, name).unwrap_or_else(|e| format!("verify failed: {e}"));
        state.stats.notes.push(note);
    }
    let cpu = cpu_start.map(|(at, start)| ((cpu_seconds() - start) / at.elapsed().as_secs_f64() * 100.0, f64::NAN));
    state.stats.report(cpu);
}

/// WebView2 hosted in a composition visual that is never shown, captured with Windows.Graphics.Capture.
fn visual(attach_to_window: bool) -> Res<()> {
    let hwnd = create_window(attach_to_window)?;
    let (device, context) = devices()?;

    // Windows.UI.Composition needs a dispatcher queue on this thread
    let _queue = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })?
    };
    let compositor = Compositor::new()?;
    let root: ContainerVisual = compositor.CreateContainerVisual()?;
    root.SetSize(windows_numerics::Vector2 { X: WIDTH as f32, Y: HEIGHT as f32 })?;
    let _target = if attach_to_window {
        let target = unsafe { compositor.cast::<ICompositorDesktopInterop>()?.CreateDesktopWindowTarget(hwnd, false)? };
        target.SetRoot(&root)?;
        Some(target)
    } else {
        None
    };

    let environment = environment()?;
    let environment3: ICoreWebView2Environment3 = environment.cast()?;
    let (tx, rx) = mpsc::channel();
    CreateCoreWebView2CompositionControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| unsafe {
            environment3.CreateCoreWebView2CompositionController(hwnd, &handler).map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |result, controller| {
            result?;
            tx.send(controller.expect("controller")).ok();
            Ok(())
        }),
    )?;
    let composition: ICoreWebView2CompositionController = rx.recv()?;
    let controller: ICoreWebView2Controller = composition.cast()?;
    let webview = unsafe {
        composition.SetRootVisualTarget(&root)?;
        controller.SetBounds(RECT { left: 0, top: 0, right: WIDTH as i32, bottom: HEIGHT as i32 })?;
        controller.SetIsVisible(true)?;
        controller.cast::<ICoreWebView2Controller2>()?.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 0, R: 0, G: 0, B: 0 })?;
        controller.CoreWebView2()?
    };
    navigate(&webview)?;
    wait(1500);

    let name = if attach_to_window { "windows visual (window target) + Graphics.Capture" } else { "windows visual + Graphics.Capture" };
    let state = Arc::new(Mutex::new(Capture { stats: Stats::new(name), shared: None, first_frame: None }));
    let visual: Visual = root.cast()?;
    let item = GraphicsCaptureItem::CreateFromVisual(&visual)?;
    let (_pool, _session) = start_capture(&item, &device, &context, state.clone())?;

    let mut cpu_start = None;
    pump(|| report(&state, &webview, &mut cpu_start));
    finish(&state, cpu_start, if attach_to_window { "windows-visual-window" } else { "windows-visual" });

    input_check(&composition, &webview);
    Ok(())
}

/// Clicks and types without the window ever having focus: mouse through the composition controller,
/// text through the DevTools protocol.
fn input_check(composition: &ICoreWebView2CompositionController, webview: &ICoreWebView2) {
    let click = |(x, y): (f64, f64)| unsafe {
        let point = POINT { x: x as i32, y: y as i32 };
        composition.SendMouseInput(COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE, COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE, 0, point).ok();
        composition.SendMouseInput(COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_LEFT_BUTTON, 0, point).ok();
        composition.SendMouseInput(COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE, 0, point).ok();
    };
    click(INPUT_FIELD);
    wait(300);
    let method = HSTRING::from("Input.insertText");
    let params = HSTRING::from(format!("{{\"text\":\"{INPUT_TEXT}\"}}"));
    unsafe {
        webview.CallDevToolsProtocolMethod(PCWSTR(method.as_ptr()), PCWSTR(params.as_ptr()),
            &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(())))).ok();
    }
    wait(300);
    click(INPUT_BUTTON);
    wait(500);
    let title = title(webview);
    println!("input without focus: {} (title '{title}')", if input_ok(&title) { "ok" } else { "FAILED" });
}

struct Win32Parent(HWND);

impl raw_window_handle::HasWindowHandle for Win32Parent {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let handle = raw_window_handle::Win32WindowHandle::new(std::num::NonZeroIsize::new(self.0 .0 as isize).unwrap());
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Win32(handle)) })
    }
}

fn wry_webview(hwnd: HWND) -> Res<wry::WebView> {
    Ok(wry::WebViewBuilder::new()
        .with_html(PAGE)
        .with_transparent(true)
        .with_bounds(wry::Rect {
            position: wry::dpi::PhysicalPosition::new(0, 0).into(),
            size: wry::dpi::PhysicalSize::new(WIDTH, HEIGHT).into(),
        })
        .build_as_child(&Win32Parent(hwnd))?)
}

/// Wry's own window hosting, with the window off-screen and captured with Windows.Graphics.Capture.
fn window_capture() -> Res<()> {
    use wry::WebViewExtWindows;
    let hwnd = create_window(true)?;
    let (device, context) = devices()?;
    let _queue = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })?
    };
    let webview = wry_webview(hwnd)?;
    wait(1500);

    let state = Arc::new(Mutex::new(Capture { stats: Stats::new("windows wry window + Graphics.Capture"), shared: None, first_frame: None }));
    let item: GraphicsCaptureItem = unsafe {
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?.CreateForWindow(hwnd)?
    };
    let (_pool, _session) = start_capture(&item, &device, &context, state.clone())?;
    let core = webview.webview();
    let mut cpu_start = None;
    pump(|| report(&state, &core, &mut cpu_start));
    finish(&state, cpu_start, "windows-window");
    Ok(())
}

/// Wry's own window hosting, copied out with ICoreWebView2::CapturePreview (PNG) as fast as it goes.
fn preview() -> Res<()> {
    use wry::WebViewExtWindows;
    let hwnd = create_window(true)?;
    let webview = wry_webview(hwnd)?;
    let core = webview.webview();
    wait(1500);

    let stats = Arc::new(Mutex::new(Stats::new("windows CapturePreview (PNG, decoded)")));
    let last = Arc::new(Mutex::new(None::<(Vec<u8>, u32, u32)>));

    fn next(core: ICoreWebView2, stats: Arc<Mutex<Stats>>, last: Arc<Mutex<Option<(Vec<u8>, u32, u32)>>>) {
        let started = Instant::now();
        let stream = unsafe { windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal(HGLOBAL::default(), true) }.unwrap();
        let handler_stream = stream.clone();
        let handler_core = core.clone();
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            result?;
            let png = unsafe { read_stream(&handler_stream) };
            let decoded = decode_png(&png);
            stats.lock().unwrap().frame(ms(started), None);
            *last.lock().unwrap() = decoded;
            if !stats.lock().unwrap().done() {
                next(handler_core, stats, last);
            }
            Ok(())
        }));
        unsafe { core.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler).ok() };
    }

    next(core.clone(), stats.clone(), last.clone());
    let mut cpu_start = None;
    pump(|| {
        let mut s = stats.lock().unwrap();
        if s.measuring() {
            cpu_start.get_or_insert((Instant::now(), cpu_seconds()));
            if let Some(fps) = title_field(&title(&core), "fps").and_then(|f| f.parse().ok()) {
                s.page_fps.push(fps);
            }
        }
        !s.done()
    });
    let mut s = stats.lock().unwrap();
    if let Some((pixels, width, height)) = last.lock().unwrap().as_ref() {
        s.notes.push(format!("frame {width}x{height}, marker pixel ok: {}", marker_ok(pixels, *width as usize * 4, false)));
        dump_frame("windows-preview", pixels, *width, *height, *width as usize * 4, false);
    }
    let cpu = cpu_start.map(|(at, start)| ((cpu_seconds() - start) / at.elapsed().as_secs_f64() * 100.0, f64::NAN));
    s.report(cpu);
    Ok(())
}

unsafe fn read_stream(stream: &IStream) -> Vec<u8> {
    unsafe {
        let hglobal = windows::Win32::System::Com::StructuredStorage::GetHGlobalFromStream(stream).unwrap();
        let size = windows::Win32::System::Memory::GlobalSize(hglobal);
        let ptr = windows::Win32::System::Memory::GlobalLock(hglobal) as *const u8;
        let data = std::slice::from_raw_parts(ptr, size).to_vec();
        let _ = windows::Win32::System::Memory::GlobalUnlock(hglobal);
        data
    }
}

fn decode_png(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    Some((buf, info.width, info.height))
}

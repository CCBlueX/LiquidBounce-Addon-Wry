//! A private Wayland compositor inside the process. GTK and WebKitGTK render into it like into any
//! desktop, and every frame arrives as a wl_buffer: shared memory without a GPU, a dmabuf with one.

use crate::common::*;
use gtk::prelude::*;
use smithay::backend::allocator::{Buffer as _, Fourcc, Modifier};
use smithay::input::keyboard::{FilterResult, Keycode, XkbConfig};
use smithay::input::pointer::{ButtonEvent, CursorImageStatus, MotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::{channel, generic::Generic, timer::{TimeoutAction, Timer}, EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{Serial, Transform, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{with_states, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, BufferAssignment};
use smithay::wayland::dmabuf::{get_dmabuf, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::selection::data_device::{ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState};
use smithay::wayland::shm::{with_buffer_contents, ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::{delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell};
use std::os::fd::OwnedFd;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// What the compositor hands to the game for each frame.
pub enum NestedFrame {
    Shm { data: Vec<u8>, width: u32, height: u32, stride: usize, copy_ms: f64 },
    Dmabuf { fds: Vec<OwnedFd>, offsets: Vec<u32>, strides: Vec<u32>, fourcc: u32, modifier: u64, width: u32, height: u32 },
}

/// Input the game forwards, delivered as real Wayland seat events.
pub enum NestedInput {
    Click(f64, f64),
    /// Linux evdev key codes
    Keys(Vec<u32>),
}

#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

struct State {
    compositor: CompositorState,
    xdg_shell: XdgShellState,
    shm: ShmState,
    dmabuf: DmabufState,
    _dmabuf_global: Option<DmabufGlobal>,
    seat_state: SeatState<State>,
    data_device: DataDeviceState,
    _output_manager: OutputManagerState,
    seat: Seat<State>,
    toplevel: Option<ToplevelSurface>,
    frames: mpsc::SyncSender<NestedFrame>,
    started: Instant,
    cursor: Option<String>,
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        let is_toplevel = self.toplevel.as_ref().is_some_and(|t| t.wl_surface() == surface);
        let buffer = with_states(surface, |states| {
            match states.cached_state.get::<SurfaceAttributes>().current().buffer.take() {
                Some(BufferAssignment::NewBuffer(buffer)) => Some(buffer),
                _ => None,
            }
        });
        let Some(buffer) = buffer else { return };
        if !is_toplevel {
            buffer.release();
            return;
        }

        let started = Instant::now();
        let frame = with_buffer_contents(&buffer, |ptr, len, data| {
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
            let start = data.offset as usize;
            let size = data.stride as usize * data.height as usize;
            NestedFrame::Shm {
                data: bytes[start..start + size].to_vec(),
                width: data.width as u32,
                height: data.height as u32,
                stride: data.stride as usize,
                copy_ms: 0.0,
            }
        }).ok().or_else(|| {
            let dmabuf = get_dmabuf(&buffer).ok()?;
            Some(NestedFrame::Dmabuf {
                fds: dmabuf.handles().map(|fd| fd.try_clone_to_owned().unwrap()).collect(),
                offsets: dmabuf.offsets().collect(),
                strides: dmabuf.strides().collect(),
                fourcc: dmabuf.format().code as u32,
                modifier: dmabuf.format().modifier.into(),
                width: dmabuf.width(),
                height: dmabuf.height(),
            })
        });
        // A dmabuf would be released only once the game drew with it; the probe imports it right away
        buffer.release();
        if let Some(mut frame) = frame {
            if let NestedFrame::Shm { copy_ms, .. } = &mut frame {
                *copy_ms = ms(started);
            }
            self.frames.try_send(frame).ok();
        }
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.size = Some((WIDTH as i32, HEIGHT as i32).into());
            // Activated keeps GTK drawing focus and the caret
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        self.toplevel = Some(surface);
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}
    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}
    fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf, notifier: ImportNotifier) {
        // The game imports it on its own GL context later
        let _ = notifier.successful::<State>();
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor = Some(match image {
            CursorImageStatus::Hidden => "hidden".into(),
            CursorImageStatus::Named(icon) => format!("{icon:?}"),
            CursorImageStatus::Surface(_) => "surface".into(),
        });
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device
    }
}

impl ClientDndGrabHandler for State {}

impl ServerDndGrabHandler for State {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

impl OutputHandler for State {}

delegate_compositor!(State);
delegate_xdg_shell!(State);
delegate_shm!(State);
delegate_dmabuf!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_output!(State);

/// The DRM render node of the GPU, which Mesa needs to hear about to hand out dmabufs.
fn render_node() -> Option<libc::dev_t> {
    let entry = std::fs::read_dir("/dev/dri").ok()?.flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("renderD"))?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let path = std::ffi::CString::new(entry.path().to_string_lossy().as_bytes()).ok()?;
    (unsafe { libc::stat(path.as_ptr(), &mut stat) } == 0).then_some(stat.st_rdev)
}

/// Starts the compositor on its own thread. Returns the socket name and where frames and input go.
pub fn spawn(frames: mpsc::SyncSender<NestedFrame>) -> (String, channel::Sender<NestedInput>, mpsc::Receiver<Option<String>>) {
    let name = format!("wry-probe-{}", std::process::id());
    let (input_tx, input_rx) = channel::channel::<NestedInput>();
    let (cursor_tx, cursor_rx) = mpsc::channel();
    let socket_name = name.clone();

    std::thread::spawn(move || {
        let mut event_loop: EventLoop<State> = EventLoop::try_new().unwrap();
        let display: Display<State> = Display::new().unwrap();
        let dh = display.handle();

        let mut dmabuf = DmabufState::new();
        let dmabuf_global = render_node().map(|device| {
            let formats = [Fourcc::Argb8888, Fourcc::Xrgb8888].into_iter()
                .flat_map(|code| [Modifier::Linear, Modifier::Invalid].map(|modifier| smithay::backend::allocator::Format { code, modifier }));
            let feedback = DmabufFeedbackBuilder::new(device, formats).build().unwrap();
            println!("nested: offering dmabufs for render node {device:#x}");
            dmabuf.create_global_with_default_feedback::<State>(&dh, &feedback)
        });
        if dmabuf_global.is_none() {
            println!("nested: no GPU render node, clients get shared memory only");
        }

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(XkbConfig::default(), 200, 25).unwrap();
        seat.add_pointer();

        let output = Output::new("probe".into(), PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "wry-probe".into(),
            model: "nested".into(),
        });
        let _output_global = output.create_global::<State>(&dh);
        let mode = smithay::output::Mode { size: (WIDTH as i32, HEIGHT as i32).into(), refresh: 60_000 };
        output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
        output.set_preferred(mode);

        let mut state = State {
            compositor: CompositorState::new::<State>(&dh),
            xdg_shell: XdgShellState::new::<State>(&dh),
            shm: ShmState::new::<State>(&dh, vec![]),
            dmabuf,
            _dmabuf_global: dmabuf_global,
            seat_state,
            data_device: DataDeviceState::new::<State>(&dh),
            _output_manager: OutputManagerState::new_with_xdg_output::<State>(&dh),
            seat,
            toplevel: None,
            frames,
            started: Instant::now(),
            cursor: None,
        };

        let source = ListeningSocketSource::with_name(&socket_name).expect("socket");
        let handle = event_loop.handle();
        let mut clients = dh.clone();
        handle.insert_source(source, move |stream, _, _| {
            clients.insert_client(stream, Arc::new(ClientState::default())).unwrap();
        }).unwrap();

        handle.insert_source(Generic::new(display, Interest::READ, Mode::Level), |_, display, state| {
            unsafe { display.get_mut().dispatch_clients(state).unwrap() };
            Ok(PostAction::Continue)
        }).unwrap();

        // Frame callbacks at 60 Hz, as a display would pace them
        handle.insert_source(Timer::from_duration(Duration::from_millis(16)), |_, _, state| {
            if let Some(toplevel) = &state.toplevel {
                let time = state.started.elapsed().as_millis() as u32;
                with_states(toplevel.wl_surface(), |states| {
                    for callback in states.cached_state.get::<SurfaceAttributes>().current().frame_callbacks.drain(..) {
                        callback.done(time);
                    }
                });
            }
            TimeoutAction::ToDuration(Duration::from_millis(16))
        }).unwrap();

        handle.insert_source(input_rx, move |event, _, state| {
            let channel::Event::Msg(event) = event else { return };
            let Some(surface) = state.toplevel.as_ref().map(|t| t.wl_surface().clone()) else { return };
            let time = state.started.elapsed().as_millis() as u32;
            match event {
                NestedInput::Click(x, y) => {
                    let pointer = state.seat.get_pointer().unwrap();
                    pointer.motion(state, Some((surface, (0.0, 0.0).into())), &MotionEvent {
                        location: (x, y).into(), serial: SERIAL_COUNTER.next_serial(), time });
                    pointer.frame(state);
                    for pressed in [smithay::backend::input::ButtonState::Pressed, smithay::backend::input::ButtonState::Released] {
                        pointer.button(state, &ButtonEvent { serial: SERIAL_COUNTER.next_serial(), time, button: 0x110, state: pressed });
                        pointer.frame(state);
                    }
                    cursor_tx.send(state.cursor.clone()).ok();
                }
                NestedInput::Keys(codes) => {
                    let keyboard = state.seat.get_keyboard().unwrap();
                    keyboard.set_focus(state, Some(surface), SERIAL_COUNTER.next_serial());
                    for code in codes {
                        for pressed in [smithay::backend::input::KeyState::Pressed, smithay::backend::input::KeyState::Released] {
                            keyboard.input::<(), _>(state, Keycode::new(code + 8), pressed, SERIAL_COUNTER.next_serial(), time,
                                |_, _, _| FilterResult::Forward);
                        }
                    }
                }
            }
        }).unwrap();

        let mut flush: DisplayHandle = dh.clone();
        loop {
            event_loop.dispatch(Some(Duration::from_millis(5)), &mut state).unwrap();
            flush.flush_clients().ok();
        }
    });

    (name, input_tx, cursor_rx)
}

/// GTK and WebKitGTK on the nested compositor, measured like the other paths.
pub fn run() {
    let (frames_tx, frames_rx) = mpsc::sync_channel::<NestedFrame>(4);
    let (name, input, cursor) = spawn(frames_tx);
    let cursor = std::rc::Rc::new(cursor);
    std::thread::sleep(Duration::from_millis(200));

    // Only this process's GTK talks to the private compositor
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", std::env::temp_dir()) };
    }
    gtk::gdk::set_allowed_backends("wayland");
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &name) };
    gtk::init().expect("gtk::init on the nested compositor");

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_decorated(false);
    window.set_default_size(WIDTH as i32, HEIGHT as i32);
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
    window.add(&container);
    use wry::{WebViewBuilderExtUnix, WebViewExtUnix};
    let webview = wry::WebViewBuilder::new().with_html(PAGE).with_transparent(true).build_gtk(&container).expect("webview");
    window.show_all();
    let wk = webview.webview();

    // The game's render thread: takes each frame and brings it into its GL context
    let (report_tx, report_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || consume(frames_rx, report_tx));

    let started = Instant::now();
    let page = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let page_samples = page.clone();
    let view = wk.clone();
    let mut cpu = None;
    glib::timeout_add_local(Duration::from_secs(1), move || {
        use webkit2gtk::WebViewExt;
        if started.elapsed() >= WARMUP {
            cpu.get_or_insert_with(crate::linux::CpuSample::now);
            if let Some(fps) = view.title().and_then(|t| title_field(&t, "fps")?.parse::<u32>().ok()) {
                page_samples.borrow_mut().push(fps);
            }
        }
        if started.elapsed() < WARMUP + MEASURE + Duration::from_millis(500) {
            return glib::ControlFlow::Continue;
        }

        println!("== RESULT linux nested wayland compositor ==");
        for line in report_rx.try_iter() {
            println!("{line}");
        }
        println!("page rAF fps samples: {:?}", page_samples.borrow());
        if let Some((own, children)) = cpu.as_ref().map(crate::linux::CpuSample::usage) {
            println!("cpu: probe process {own:.0}% of one core, web/network processes {children:.0}%");
        }

        // Clicks and types through the compositor's seat; GTK never sees the game's focus
        input.send(NestedInput::Click(INPUT_FIELD.0, INPUT_FIELD.1)).ok();
        let input = input.clone();
        let view = view.clone();
        let cursor = cursor.clone();
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            // w, r, y as evdev codes
            input.send(NestedInput::Keys(vec![17, 19, 21])).ok();
            glib::timeout_add_local_once(Duration::from_millis(300), move || {
                input.send(NestedInput::Click(INPUT_BUTTON.0, INPUT_BUTTON.1)).ok();
                glib::timeout_add_local_once(Duration::from_millis(500), move || {
                    use webkit2gtk::WebViewExt;
                    let title = view.title().map(|t| t.to_string()).unwrap_or_default();
                    println!("input without focus: {} (title '{title}')", if input_ok(&title) { "ok" } else { "FAILED" });
                    println!("cursor the page asked for over the button: {:?}", cursor.try_iter().last().flatten());
                    gtk::main_quit();
                });
            });
        });
        glib::ControlFlow::Break
    });
    gtk::main();
    drop(webview);
}

fn consume(frames: mpsc::Receiver<NestedFrame>, report: mpsc::Sender<String>) {
    let started = Instant::now();
    let (mut count, mut shm, mut dmabuf) = (0u64, 0u64, 0u64);
    let mut copy = Vec::new();
    let mut last = None;
    let mut dmabuf_note = None;
    while started.elapsed() < WARMUP + MEASURE {
        let Ok(frame) = frames.recv_timeout(Duration::from_millis(100)) else { continue };
        if started.elapsed() < WARMUP {
            continue;
        }
        count += 1;
        match frame {
            NestedFrame::Shm { data, width, height, stride, copy_ms } => {
                shm += 1;
                copy.push(copy_ms);
                last = Some((data, width, height, stride));
            }
            NestedFrame::Dmabuf { fds, offsets, strides, fourcc, modifier, width, height } => {
                dmabuf += 1;
                if dmabuf_note.is_none() {
                    dmabuf_note = Some(crate::linux::import_dmabuf(&fds, &offsets, &strides, fourcc, modifier, width, height));
                }
            }
        }
    }
    let seconds = MEASURE.as_secs_f64();
    report.send(format!("frames received: {count} in {seconds:.0}s -> {:.1} fps ({shm} shared memory, {dmabuf} dmabuf)",
        count as f64 / seconds)).ok();
    if !copy.is_empty() {
        report.send(format!("shared memory copy ms: avg {:.2}, max {:.2}", copy.iter().sum::<f64>() / copy.len() as f64,
            copy.iter().cloned().fold(0.0, f64::max))).ok();
    }
    if let Some(note) = dmabuf_note {
        report.send(format!("dmabuf import: {note}")).ok();
    }
    if let Some((data, width, height, stride)) = last {
        report.send(format!("frame {width}x{height}, marker pixel ok: {}, background alpha: {}",
            marker_ok(&data, stride, true), background_alpha(&data, stride))).ok();
        dump_frame("linux-nested", &data, width, height, stride, true);
    }
}

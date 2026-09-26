use std::io::Write;
use std::time::{Duration, Instant};

pub const WIDTH: u32 = 1600;
pub const HEIGHT: u32 = 900;
pub const WARMUP: Duration = Duration::from_secs(3);
pub const MEASURE: Duration = Duration::from_secs(10);

pub const PAGE: &str = include_str!("../page.html");

/// Collects per-frame timings of one capture path.
pub struct Stats {
    pub name: String,
    started: Instant,
    frames: u64,
    grab: Vec<f64>,
    upload: Vec<f64>,
    pub page_fps: Vec<u32>,
    pub notes: Vec<String>,
}

impl Stats {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            started: Instant::now(),
            frames: 0,
            grab: Vec::new(),
            upload: Vec::new(),
            page_fps: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn measuring(&self) -> bool {
        self.started.elapsed() >= WARMUP
    }

    pub fn done(&self) -> bool {
        self.started.elapsed() >= WARMUP + MEASURE
    }

    pub fn frame(&mut self, grab_ms: f64, upload_ms: Option<f64>) {
        if !self.measuring() {
            return;
        }
        self.frames += 1;
        self.grab.push(grab_ms);
        if let Some(upload_ms) = upload_ms {
            self.upload.push(upload_ms);
        }
    }

    pub fn report(&self, cpu: Option<(f64, f64)>) {
        let seconds = (self.started.elapsed() - WARMUP).as_secs_f64().min(MEASURE.as_secs_f64());
        println!("== RESULT {} ==", self.name);
        println!("frames captured: {} in {:.1}s -> {:.1} fps", self.frames, seconds, self.frames as f64 / seconds);
        println!("grab ms: {}", summary(&self.grab));
        if !self.upload.is_empty() {
            println!("upload ms: {}", summary(&self.upload));
        }
        if !self.page_fps.is_empty() {
            println!("page rAF fps samples: {:?}", self.page_fps);
        }
        if let Some((own, children)) = cpu {
            println!("cpu: probe process {:.0}% of one core, web/network processes {:.0}%", own, children);
        }
        for note in &self.notes {
            println!("note: {note}");
        }
        std::io::stdout().flush().ok();
    }
}

fn summary(values: &[f64]) -> String {
    if values.is_empty() {
        return "n/a".into();
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let p95 = sorted[((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1)];
    format!("avg {:.2}, p95 {:.2}, max {:.2} (n={})", avg, p95, sorted[sorted.len() - 1], sorted.len())
}

pub fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// Checks the red marker in the top left corner of the page, for BGRA or RGBA pixels.
pub fn marker_ok(pixels: &[u8], stride: usize, bgra: bool) -> bool {
    let offset = 8 * stride + 8 * 4;
    let px = &pixels[offset..offset + 4];
    let (r, g, b, a) = if bgra { (px[2], px[1], px[0], px[3]) } else { (px[0], px[1], px[2], px[3]) };
    r > 200 && g < 50 && b < 50 && a > 200
}

/// Alpha of a pixel outside every element, which stays 0 when transparency survives.
pub fn background_alpha(pixels: &[u8], stride: usize) -> u8 {
    pixels[20 * stride + 800 * 4 + 3]
}

/// Writes the frame as PNG, and a small thumbnail to the log so CI results can be looked at.
pub fn dump_frame(name: &str, pixels: &[u8], width: u32, height: u32, stride: usize, bgra: bool) {
    let rgba = |x: u32, y: u32| {
        let o = y as usize * stride + x as usize * 4;
        let p = &pixels[o..o + 4];
        if bgra { [p[2], p[1], p[0], p[3]] } else { [p[0], p[1], p[2], p[3]] }
    };

    let dir = std::path::Path::new("out");
    std::fs::create_dir_all(dir).ok();
    let full: Vec<u8> = (0..height).flat_map(|y| (0..width).flat_map(move |x| rgba(x, y))).collect();
    write_png(&dir.join(format!("{name}.png")), width, height, &full);

    let (tw, th) = (320u32, 180u32);
    let thumb: Vec<u8> = (0..th)
        .flat_map(|y| (0..tw).flat_map(move |x| rgba(x * width / tw, y * height / th)))
        .collect();
    let mut buf = Vec::new();
    encode_png(&mut buf, tw, th, &thumb);
    use base64::Engine;
    println!("THUMB {name} {}", base64::engine::general_purpose::STANDARD.encode(&buf));
}

fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) {
    let mut buf = Vec::new();
    encode_png(&mut buf, width, height, rgba);
    std::fs::write(path, buf).ok();
}

fn encode_png(out: &mut Vec<u8>, width: u32, height: u32, rgba: &[u8]) {
    let mut encoder = png::Encoder::new(out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(rgba).unwrap();
}

/// Reads a field of the state the page keeps in its title, `fps=60;clicks=1;text=wry`.
pub fn title_field<'a>(title: &'a str, key: &str) -> Option<&'a str> {
    title.split(';').find_map(|part| part.strip_prefix(key)?.strip_prefix('='))
}

/// The input check: a click on the text field, typing "wry", then a click on the button.
pub const INPUT_FIELD: (f64, f64) = (100.0, 720.0);
pub const INPUT_BUTTON: (f64, f64) = (140.0, 630.0);
pub const INPUT_TEXT: &str = "wry";

pub fn input_ok(title: &str) -> bool {
    title_field(title, "clicks") == Some("1") && title_field(title, "text") == Some(INPUT_TEXT)
}

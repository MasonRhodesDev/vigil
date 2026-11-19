#![deny(unsafe_code)]

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::BorrowedFd;
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use image::ImageEncoder;
use vigil_scene::{
    Error, Format, HELPER_ABI, Layer, Layout, Logo, MAX_EDGE, MAX_LOGO_BYTES, MAX_PIXELS, MAX_SIDE,
    MAX_SPEC_BYTES, NOTICE, Plate, Request, Scene, Sprite, confine, emit_spec, lock_down,
    read_bounded,
};

const USAGE: &str = "usage:
  vigil-scene render --stage STAGE --out FILE [--mode WxH] [--transform 0-3]
                     [--ref-short-side N] [--scene FILE] [--logo SVG]
                     [--level 0..1] [--progress 0..1] [--edge N]
  vigil-scene spec --out FILE [--scene FILE] [--ref-short-side N]
  vigil-scene mask --format svg|raster --edge N --deadline-ms N --out-len N
                   --abi V
  vigil-scene layer --mode WxH --transform 0-3 --layer frame|logo|glow|bar
                    --value 0..1 --svg-len N --spec-len N --deadline-ms N
                    --out-len N --abi V

stages:
  grub-menu     the mark sprite at its place on the mode (RGBA, straight alpha)
  grub-handoff  the whole frame at rest (RGB)
  splash        the whole frame with the glow at --level (RGB)
  glow          the glow sprite at --level, default 1 (RGBA, straight alpha)
  bar           the progress bar sprite at --progress (RGBA)
  icon          the mark alone, --edge pixels square (RGBA, straight alpha)

--mode is the output's mode as the panel scans it out; --transform turns the
scene on it as wl_output does. Sprites print their placement as JSON.

mask and layer are the helper the library runs for untrusted logos. Each
confines itself, reads the logo (and for layer, the spec after it) from
stdin, and writes --out-len bytes of raw pixels to stdout.";

const RENDER_LIMIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    GrubMenu,
    GrubHandoff,
    Splash,
    Glow,
    Bar,
    Icon,
}

impl Stage {
    fn parse(s: &str) -> Option<Stage> {
        Some(match s {
            "grub-menu" => Stage::GrubMenu,
            "grub-handoff" => Stage::GrubHandoff,
            "splash" => Stage::Splash,
            "glow" => Stage::Glow,
            "bar" => Stage::Bar,
            "icon" => Stage::Icon,
            _ => return None,
        })
    }
}

enum Picture {
    Frame { panel: (u32, u32), xrgb: Vec<u8> },
    Sprite(Sprite),
}

struct Args(HashMap<String, String>);

impl Args {
    fn parse(argv: &[String], allowed: &[&str]) -> Result<Args, String> {
        let mut map = HashMap::new();
        let mut it = argv.iter();
        while let Some(flag) = it.next() {
            let key = flag
                .strip_prefix("--")
                .filter(|k| allowed.contains(k))
                .ok_or_else(|| format!("unknown argument {flag}"))?;
            let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
            if map.insert(key.to_owned(), value.clone()).is_some() {
                return Err(format!("{flag} given twice"));
            }
        }
        Ok(Args(map))
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    fn need(&self, key: &str) -> Result<&str, String> {
        self.get(key).ok_or_else(|| format!("--{key} is required"))
    }

    fn num<T: std::str::FromStr>(&self, key: &str) -> Result<Option<T>, String> {
        self.get(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| format!("--{key} {v} is not a number"))
            })
            .transpose()
    }

    fn unit(&self, key: &str, default: f64) -> Result<f64, String> {
        let v = self.num::<f64>(key)?.unwrap_or(default);
        if !(0.0..=1.0).contains(&v) {
            return Err(format!("--{key} {v} is outside 0..1"));
        }
        Ok(v)
    }
}

fn mode(text: &str) -> Result<(u32, u32), String> {
    let (w, h) = text
        .split_once('x')
        .ok_or_else(|| format!("--mode {text} is not WxH"))?;
    let parse = |v: &str| {
        v.parse::<u32>()
            .map_err(|_| format!("--mode {text} is not WxH"))
    };
    Ok((parse(w)?, parse(h)?))
}

fn scene_spec(args: &Args) -> Result<(Scene, Option<String>), String> {
    let source = args
        .get("scene")
        .map(|p| {
            let bytes = read_bounded(Path::new(p), MAX_SPEC_BYTES)
                .map_err(|e| format!("--scene {p}: {e}"))?;
            String::from_utf8(bytes).map_err(|_| format!("--scene {p}: not UTF-8"))
        })
        .transpose()?;
    let mut spec = match &source {
        Some(text) => Scene::parse(text).map_err(|e| format!("--scene: {e}"))?,
        None => Scene::default(),
    };
    if let Some(r) = args.num::<u32>("ref-short-side")? {
        if r > MAX_SIDE {
            return Err(format!("--ref-short-side {r} is over {MAX_SIDE}"));
        }
        spec.ref_short_side = (r > 0).then_some(r);
    }
    Ok((spec, source))
}

fn logo(args: &Args) -> Result<Option<Logo>, String> {
    args.get("logo")
        .map(|p| {
            read_bounded(Path::new(p), MAX_LOGO_BYTES)
                .and_then(|data| Logo::svg(&data))
                .map_err(|e| format!("--logo {p}: {e}"))
        })
        .transpose()
}

fn draw(args: &Args, stage: Stage) -> Result<Picture, String> {
    let (spec, _) = scene_spec(args)?;
    let logo = logo(args)?;
    let needs_logo = || {
        logo.as_ref()
            .ok_or_else(|| "this stage draws the mark and needs --logo".to_owned())
    };
    let plate = || -> Result<(Plate, (u32, u32)), String> {
        let panel = mode(args.need("mode")?)?;
        let transform = args.num::<u8>("transform")?.unwrap_or(0);
        if transform > 3 {
            return Err(format!("--transform {transform} is outside 0..3"));
        }
        let scene = Layout::new(&spec, panel, transform)
            .map_err(|e| e.to_string())?
            .scene();
        let plate = match (&logo, stage) {
            (None, Stage::GrubHandoff | Stage::Splash | Stage::Bar) => {
                Plate::ground_only(&spec, scene, transform)
            }
            _ => Plate::new(&spec, needs_logo()?, scene, transform).map_err(|e| e.to_string())?,
        };
        Ok((plate, panel))
    };
    let frame = |level: f64| -> Result<Picture, String> {
        let (plate, panel) = plate()?;
        Ok(Picture::Frame {
            panel,
            xrgb: plate.frame(level),
        })
    };
    match stage {
        Stage::GrubMenu => Ok(Picture::Sprite(plate()?.0.logo_sprite().ok_or("no mark")?)),
        Stage::GrubHandoff => frame(0.0),
        Stage::Splash => frame(args.unit("level", 0.0)?),
        Stage::Glow => {
            let level = args.unit("level", 1.0)?;
            Ok(Picture::Sprite(
                plate()?.0.glow_sprite(level).ok_or("no glow")?,
            ))
        }
        Stage::Bar => {
            let progress = args.unit("progress", 0.0)?;
            Ok(Picture::Sprite(
                plate()?.0.bar_sprite(progress).ok_or("no bar")?,
            ))
        }
        Stage::Icon => {
            let edge = args
                .num::<u32>("edge")?
                .ok_or("--edge is required for icon")?;
            if edge == 0 || edge > MAX_EDGE {
                return Err(format!("--edge {edge} is outside 1..={MAX_EDGE}"));
            }
            let mask = needs_logo()?.mask(edge).map_err(|e| e.to_string())?;
            Ok(Picture::Sprite(Sprite {
                x: 0,
                y: 0,
                width: edge,
                height: edge,
                rgba: mask.sprite(spec.mark),
            }))
        }
    }
}

fn write_png(out: &Path, width: u32, height: u32, pixels: &[u8], rgba: bool) -> Result<(), Error> {
    let file = std::fs::File::create(out).map_err(|e| Error::Io(e.to_string()))?;
    let color = if rgba {
        image::ExtendedColorType::Rgba8
    } else {
        image::ExtendedColorType::Rgb8
    };
    image::codecs::png::PngEncoder::new_with_quality(
        std::io::BufWriter::new(file),
        image::codecs::png::CompressionType::Best,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(pixels, width, height, color)
    .map_err(|e| Error::Io(e.to_string()))
}

fn render(argv: &[String]) -> Result<(), String> {
    let args = Args::parse(
        argv,
        &[
            "stage",
            "out",
            "mode",
            "transform",
            "ref-short-side",
            "scene",
            "logo",
            "level",
            "progress",
            "edge",
        ],
    )?;
    let stage_name = args.need("stage")?.to_owned();
    let stage = Stage::parse(&stage_name).ok_or_else(|| format!("unknown stage {stage_name}"))?;
    let out = Path::new(args.need("out")?).to_owned();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(draw(&args, stage));
    });
    let picture = rx.recv_timeout(RENDER_LIMIT).map_err(|e| match e {
        mpsc::RecvTimeoutError::Timeout => {
            format!("gave up after {}s of drawing", RENDER_LIMIT.as_secs())
        }
        mpsc::RecvTimeoutError::Disconnected => "the drawing failed".to_owned(),
    })??;
    let fail = |e: Error| format!("{}: {e}", out.display());
    match picture {
        Picture::Frame { panel, xrgb } => {
            let rgb: Vec<u8> = xrgb.chunks(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            write_png(&out, panel.0, panel.1, &rgb, false).map_err(fail)?;
        }
        Picture::Sprite(s) => {
            write_png(&out, s.width, s.height, &s.rgba, true).map_err(fail)?;
            vigil_log::out!(
                "{}",
                serde_json::json!({
                    "stage": stage_name,
                    "x": s.x,
                    "y": s.y,
                    "width": s.width,
                    "height": s.height,
                })
            );
        }
    }
    Ok(())
}

fn spec(argv: &[String]) -> Result<(), String> {
    let args = Args::parse(argv, &["out", "scene", "ref-short-side"])?;
    let out = args.need("out")?;
    let (_, source) = scene_spec(&args)?;
    let text = emit_spec(source.as_deref(), args.num::<u32>("ref-short-side")?)
        .map_err(|e| e.to_string())?;
    std::fs::write(out, text).map_err(|e| format!("{out}: {e}"))
}

// Descriptors the caller leaked into this process, a compositor's among
// them, are closed before the input is read.
#[allow(unsafe_code)]
fn close_inherited() -> Result<(), Error> {
    // SAFETY: close_range takes no pointers. It runs first in main, before
    // this process has made any descriptor above 2, so nothing here owns one
    // it closes.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3 as libc::c_uint,
            libc::c_uint::MAX,
            0 as libc::c_uint,
        )
    };
    if rc != 0 {
        return Err(Error::Helper(format!(
            "close_range: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

// The helper's stdout and stderr are the library's memfds, and once it is
// locked down write(2) is its only way out: say!'s fstat and its reopen
// through /proc are calls the sandbox kills.
fn record(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> Result<(), Error> {
    while !bytes.is_empty() {
        match rustix::io::write(fd, bytes) {
            Ok(0) => return Err(Error::Io("the output takes no more bytes".into())),
            Ok(n) => bytes = &bytes[n..],
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(Error::Io(e.to_string())),
        }
    }
    Ok(())
}

// Reads all of stdin, then locks the process down before any of it is
// parsed.
fn input_up_to(cap: u64) -> Result<Vec<u8>, Error> {
    let mut data = Vec::new();
    std::io::stdin()
        .lock()
        .take(cap + 1)
        .read_to_end(&mut data)
        .map_err(|e| Error::Io(e.to_string()))?;
    lock_down()?;
    if data.len() as u64 > cap {
        return Err(Error::TooLarge(format!("the input is over {cap} bytes")));
    }
    Ok(data)
}

fn mask_job(args: &Args) -> Result<Vec<u8>, Error> {
    let format = Format::parse(args.need("format").map_err(Error::Helper)?)
        .ok_or_else(|| Error::Helper("--format is svg or raster".into()))?;
    let edge = args
        .num::<u32>("edge")
        .map_err(Error::Helper)?
        .ok_or_else(|| Error::Helper("--edge is required".into()))?;
    let data = input_up_to(format.cap())?;
    Ok(Logo::parse(&data, format)?.mask(edge)?.into_alpha())
}

fn layer_job(args: &Args) -> Result<Vec<u8>, Error> {
    let len = |key: &str, cap: u64| -> Result<usize, Error> {
        let n = args
            .num::<u64>(key)
            .map_err(Error::Helper)?
            .ok_or_else(|| Error::Helper(format!("--{key} is required")))?;
        if n > cap {
            return Err(Error::TooLarge(format!("--{key} {n} is over {cap}")));
        }
        Ok(n as usize)
    };
    let (svg_len, spec_len) = (
        len("svg-len", MAX_LOGO_BYTES)?,
        len("spec-len", MAX_SPEC_BYTES)?,
    );
    let data = input_up_to((svg_len + spec_len) as u64)?;
    if data.len() != svg_len + spec_len {
        return Err(Error::Invalid(format!(
            "read {} bytes of input, not {}",
            data.len(),
            svg_len + spec_len
        )));
    }
    let (svg, spec) = data.split_at(svg_len);
    let layer = args.need("layer").map_err(Error::Helper)?;
    Request {
        svg: (svg_len > 0).then_some(svg),
        spec: (spec_len > 0).then_some(spec),
        mode: mode(args.need("mode").map_err(Error::Helper)?).map_err(Error::Helper)?,
        transform: args
            .num::<u8>("transform")
            .map_err(Error::Helper)?
            .unwrap_or(0),
        layer: Layer::parse(layer).ok_or_else(|| Error::Helper(format!("no layer {layer}")))?,
        value: args.unit("value", 0.0).map_err(Error::Helper)?,
    }
    .draw()
}

fn helper(job: &str, argv: &[String]) -> ExitCode {
    let run = || -> Result<(), Error> {
        close_inherited()?;
        let allowed: &[&str] = match job {
            "mask" => &["format", "edge", "deadline-ms", "out-len", "abi"],
            _ => &[
                "mode",
                "transform",
                "layer",
                "value",
                "svg-len",
                "spec-len",
                "deadline-ms",
                "out-len",
                "abi",
            ],
        };
        let args = Args::parse(argv, allowed).map_err(Error::Helper)?;
        let abi = args.need("abi").map_err(Error::Helper)?;
        if abi != HELPER_ABI {
            return Err(Error::Helper(format!(
                "this helper is version {HELPER_ABI}, the library {abi}"
            )));
        }
        let deadline = args
            .num::<u64>("deadline-ms")
            .map_err(Error::Helper)?
            .ok_or_else(|| Error::Helper("--deadline-ms is required".into()))?;
        let out_len = args
            .num::<u64>("out-len")
            .map_err(Error::Helper)?
            .filter(|n| *n <= MAX_PIXELS * 4)
            .ok_or_else(|| {
                Error::Helper(format!("--out-len up to {} is required", MAX_PIXELS * 4))
            })?;
        if let Some(notice) = confine(Duration::from_millis(deadline), out_len)? {
            let _ = record(
                rustix::stdio::stderr(),
                format!("{NOTICE}{notice}\n").as_bytes(),
            );
        }
        let pixels = match job {
            "mask" => mask_job(&args)?,
            _ => layer_job(&args)?,
        };
        if pixels.len() as u64 != out_len {
            return Err(Error::Helper(format!(
                "drew {} bytes, but the caller expects {out_len}",
                pixels.len()
            )));
        }
        record(rustix::stdio::stdout(), &pixels)
    };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = record(
                rustix::stdio::stderr(),
                format!("vigil-scene: {e}\n").as_bytes(),
            );
            ExitCode::from(e.exit_code())
        }
    }
}

fn main() -> ExitCode {
    vigil_secret::exit_on_panic();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let result = match argv.first().map(String::as_str) {
        Some(job @ ("mask" | "layer")) => return helper(job, &argv[1..]),
        Some("render") => render(&argv[1..]),
        Some("spec") => spec(&argv[1..]),
        Some("--help" | "-h" | "help") => {
            vigil_log::out!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            vigil_log::say!(
                "usage: vigil-scene render|spec|mask|layer ...; see vigil-scene --help"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            vigil_log::say!("vigil-scene: {e}");
            ExitCode::FAILURE
        }
    }
}

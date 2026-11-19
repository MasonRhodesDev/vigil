#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use image::ImageEncoder;
use vigil_scene::{
    Error, Logo, MAX_EDGE, MAX_SIDE, Plate, Scene, Sprite, check_output, emit_spec, read_bounded,
};

const USAGE: &str = "usage:
  vigil-scene render --stage STAGE --out FILE [--mode WxH] [--transform 0-3]
                     [--ref-short-side N] [--scene FILE] [--logo SVG]
                     [--level 0..1] [--progress 0..1] [--edge N]
  vigil-scene spec --out FILE [--scene FILE] [--ref-short-side N]

stages:
  grub-menu     the mark sprite at its place on the mode (RGBA, straight alpha)
  grub-handoff  the whole frame at rest (RGB)
  splash        the whole frame with the glow at --level (RGB)
  glow          the glow sprite at --level, default 1 (RGBA, straight alpha)
  bar           the progress bar sprite at --progress (RGBA)
  icon          the mark alone, --edge pixels square (RGBA, straight alpha)

--mode is the output's mode as the panel scans it out; --transform turns the
scene on it as wl_output does. Sprites print their placement as JSON.";

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
    let panel = (parse(w)?, parse(h)?);
    check_output(panel).map_err(|e| e.to_string())?;
    Ok(panel)
}

fn scene_spec(args: &Args) -> Result<(Scene, Option<String>), String> {
    let source = args
        .get("scene")
        .map(|p| {
            let bytes = read_bounded(Path::new(p)).map_err(|e| format!("--scene {p}: {e}"))?;
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
            read_bounded(Path::new(p))
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
        let scene = if transform % 2 == 1 {
            (panel.1, panel.0)
        } else {
            panel
        };
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
            Ok(Picture::Sprite(plate()?.0.bar_sprite(progress)))
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
    let picture = rx
        .recv_timeout(RENDER_LIMIT)
        .map_err(|_| format!("gave up after {}s of drawing", RENDER_LIMIT.as_secs()))??;
    let fail = |e: Error| format!("{}: {e}", out.display());
    match picture {
        Picture::Frame { panel, xrgb } => {
            let rgb: Vec<u8> = xrgb.chunks(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            write_png(&out, panel.0, panel.1, &rgb, false).map_err(fail)?;
        }
        Picture::Sprite(s) => {
            write_png(&out, s.width, s.height, &s.rgba, true).map_err(fail)?;
            println!(
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

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let result = match argv.first().map(String::as_str) {
        Some("render") => render(&argv[1..]),
        Some("spec") => spec(&argv[1..]),
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vigil-scene: {e}");
            ExitCode::FAILURE
        }
    }
}

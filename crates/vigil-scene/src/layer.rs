use crate::{Error, Layout, Logo, Placement, Plate, Scene};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Frame,
    Logo,
    Glow,
    Bar,
}

impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Layer::Frame => "frame",
            Layer::Logo => "logo",
            Layer::Glow => "glow",
            Layer::Bar => "bar",
        }
    }

    pub fn parse(name: &str) -> Option<Layer> {
        [Layer::Frame, Layer::Logo, Layer::Glow, Layer::Bar]
            .into_iter()
            .find(|l| l.name() == name)
    }
}

/// One layer of the scene on one output. The frame is XRGB8888 (B, G, R,
/// 0xff); the logo and glow are RGBA with straight alpha; the bar is opaque
/// RGBA. `value` is the glow's level or the bar's progress.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub svg: Option<&'a [u8]>,
    pub spec: Option<&'a [u8]>,
    pub mode: (u32, u32),
    pub transform: u8,
    pub layer: Layer,
    pub value: f64,
}

impl Request<'_> {
    fn scene(&self) -> Result<Scene, Error> {
        match self.spec {
            Some(bytes) => std::str::from_utf8(bytes)
                .map_err(|_| Error::Invalid("the scene spec is not UTF-8".into()))
                .and_then(Scene::parse),
            None => Ok(Scene::default()),
        }
    }

    /// Where the layer goes on the panel, from the spec and the mode alone.
    /// The logo is never read.
    pub fn place(&self) -> Result<Placement, Error> {
        let layout = Layout::new(&self.scene()?, self.mode, self.transform)?;
        let (width, height) = layout.panel();
        match self.layer {
            Layer::Frame => Some(Placement {
                x: 0,
                y: 0,
                width,
                height,
            }),
            Layer::Logo => layout.logo(),
            Layer::Glow => layout.glow(),
            Layer::Bar => layout.bar(),
        }
        .ok_or_else(|| Error::Refused(format!("this layout has no {}", self.layer.name())))
    }

    pub fn bytes(&self) -> Result<usize, Error> {
        let at = self.place()?;
        Ok(at.width as usize * at.height as usize * 4)
    }

    /// Draws in this process, so only for a logo the caller trusts.
    /// `Helper::draw` runs this in the helper.
    pub fn draw(&self) -> Result<Vec<u8>, Error> {
        let mut out = vec![0u8; self.bytes()?];
        self.draw_into(&mut out)?;
        Ok(out)
    }

    /// `draw` into `out`, which must be exactly `bytes()` long. Nothing is
    /// written unless the drawing succeeds.
    pub fn draw_into(&self, out: &mut [u8]) -> Result<(), Error> {
        let spec = self.scene()?;
        let layout = Layout::new(&spec, self.mode, self.transform)?;
        let (scene, transform) = (layout.scene(), layout.transform());
        let plate = match self.svg {
            Some(svg) => Plate::new(&spec, &Logo::svg(svg)?, scene, transform)?,
            None => Plate::ground_only(&spec, scene, transform),
        };
        let need = self.bytes()?;
        if out.len() != need {
            return Err(Error::Invalid(format!(
                "the {} needs {need} bytes, not {}",
                self.layer.name(),
                out.len()
            )));
        }
        let drawn = match self.layer {
            Layer::Frame => {
                plate.draw(out, layout.panel().0 as usize * 4, self.value);
                Some(())
            }
            Layer::Logo => plate.logo_into(out),
            Layer::Glow => plate.glow_into(self.value, out),
            Layer::Bar => plate.bar_into(self.value, out),
        };
        drawn.ok_or_else(|| Error::Refused(format!("this scene has no {}", self.layer.name())))
    }
}

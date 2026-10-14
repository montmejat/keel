//! What makes a simulated sensor read like a real one: one chain of effects
//! for numbers ([`VectorModel`]: encoders, IMUs, force/torque sensors), one
//! for pictures ([`ImageModel`]), and the ways a device breaks ([`Fault`]).
//! Each device gets its own, configured from the robot's file.
//!
//! The randomness is seeded: a lockstep run gives the same readings every
//! time.

use std::collections::VecDeque;
use std::io;

use crate::{Format, Image};

/// A small, fast, seeded generator (xorshift64*): noise, not cryptography.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e3779b97f4a7c15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545f4914f6cdd1d)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Normal, mean 0, standard deviation 1 (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        let (u, v) = (self.unit().max(1e-300), self.unit());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }

    /// Uniform with mean 0 and standard deviation 1: cheaper than
    /// [`Rng::normal`], for a picture's million pixels.
    pub fn flat(&mut self) -> f64 {
        (self.unit() * 2.0 - 1.0) * 3f64.sqrt()
    }
}

/// How a device breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Breakdown {
    /// A drive gives no torque at all.
    Dead,
    /// A drive gives a fifth of what it should.
    Weak,
    /// A sensor repeats its last reading; a camera, its last picture.
    Freeze,
    /// A sensor's noise grows twentyfold.
    Noisy,
    /// A camera's lens is covered.
    Dark,
}

/// A device breaking, some time after it starts.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fault {
    pub what: Breakdown,
    /// Seconds after the device's first reading or command.
    #[serde(default)]
    pub after_s: f64,
}

impl Fault {
    /// `what` after `after_s` seconds: `dead`, `dead@20`, as on a command line.
    pub fn parse(s: &str) -> io::Result<Fault> {
        let (what, after) = s.split_once('@').unwrap_or((s, "0"));
        let what = match what {
            "dead" => Breakdown::Dead,
            "weak" => Breakdown::Weak,
            "freeze" => Breakdown::Freeze,
            "noisy" => Breakdown::Noisy,
            "dark" => Breakdown::Dark,
            other => return Err(io::Error::other(format!("no fault {other:?}: dead, weak, freeze, noisy, dark"))),
        };
        let after_s = after.parse().map_err(|_| io::Error::other(format!("{s:?}: seconds after `@`")))?;
        Ok(Fault { what, after_s })
    }

    /// Whether it has happened by `now`, for a device started at `start`.
    pub fn active(&self, start: u64, now: u64) -> bool {
        now.saturating_sub(start) as f64 >= self.after_s * 1e9
    }

    /// An error unless the fault is one of `kinds`: a camera can't be weak.
    pub fn check(fault: Option<Fault>, device: &str, kinds: &[Breakdown]) -> io::Result<()> {
        match fault {
            Some(f) if !kinds.contains(&f.what) => {
                Err(io::Error::other(format!("{device} can't break with {:?}: only {kinds:?}", f.what)))
            }
            _ => Ok(()),
        }
    }
}

/// A numeric sensor's imperfections, in the order a reading meets them.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VectorSettings {
    /// How late a reading is, in milliseconds: it shows the world as it was.
    pub delay_ms: f64,
    /// Each number's offset, drawn once: its standard deviation.
    pub bias: f64,
    /// How fast the offset wanders: a random walk, per √second.
    pub drift: f64,
    /// White noise: its standard deviation.
    pub noise: f64,
    /// The smallest step the sensor can tell apart (an encoder's count);
    /// 0 for none.
    pub quantum: f64,
    /// Readings stop at ± this; 0 for no limit.
    pub range: f64,
    pub fault: Option<Fault>,
    /// For the noise; each device gets its own by default.
    pub seed: Option<u64>,
}

/// [`VectorSettings`] at work on one sensor's readings.
pub struct VectorModel {
    settings: VectorSettings,
    rng: Rng,
    /// The true values, kept as long as the delay needs.
    history: VecDeque<(u64, Vec<f64>)>,
    bias: Vec<f64>,
    start: Option<u64>,
    last_t: u64,
    last: Vec<f64>,
    frozen: Option<Vec<f64>>,
}

impl VectorModel {
    pub fn new(settings: VectorSettings, seed: u64) -> io::Result<VectorModel> {
        Fault::check(settings.fault, "a sensor", &[Breakdown::Freeze, Breakdown::Noisy, Breakdown::Dead])?;
        let rng = Rng::new(settings.seed.unwrap_or(seed));
        Ok(VectorModel {
            settings,
            rng,
            history: VecDeque::new(),
            bias: Vec::new(),
            start: None,
            last_t: 0,
            last: Vec::new(),
            frozen: None,
        })
    }

    /// What the sensor reads at `now`, with the world's true values `truth`.
    pub fn apply(&mut self, now: u64, truth: &[f64], out: &mut [f64]) {
        let s = &self.settings;
        let start = *self.start.get_or_insert(now);
        if self.bias.len() != truth.len() {
            self.bias = (0..truth.len()).map(|_| s.bias * self.rng.normal()).collect();
        }
        // The newest truth at least `delay` old; the oldest there is, until then.
        self.history.push_back((now, truth.to_vec()));
        let seen = now.saturating_sub((s.delay_ms * 1e6) as u64);
        while self.history.len() > 1 && self.history[1].0 <= seen {
            self.history.pop_front();
        }
        let dt = now.saturating_sub(self.last_t) as f64 / 1e9;
        self.last_t = now;
        let fault = s.fault.filter(|f| f.active(start, now)).map(|f| f.what);
        let noise = if fault == Some(Breakdown::Noisy) { s.noise.max(1e-3) * 20.0 } else { s.noise };
        for (i, (out, &value)) in out.iter_mut().zip(&self.history[0].1).enumerate() {
            self.bias[i] += s.drift * dt.sqrt() * self.rng.normal();
            let mut v = value + self.bias[i] + noise * self.rng.normal();
            if s.quantum > 0.0 {
                v = (v / s.quantum).round() * s.quantum;
            }
            if s.range > 0.0 {
                v = v.clamp(-s.range, s.range);
            }
            *out = v;
        }
        match fault {
            Some(Breakdown::Freeze) => out.copy_from_slice(self.frozen.get_or_insert_with(|| self.last.clone())),
            Some(Breakdown::Dead) => out.fill(0.0),
            _ => {}
        }
        self.last.clear();
        self.last.extend_from_slice(out);
    }
}

/// A camera's imperfections.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImageSettings {
    /// Brightness, against the rendering's: 1 as rendered.
    pub exposure: f64,
    /// Noise on each pixel: its standard deviation, in levels of 255 (in
    /// metres, for depth).
    pub noise: f64,
    /// A box blur this many pixels wide either side: focus, motion.
    pub blur: u32,
    /// For depth: the fraction of pixels that get nothing back.
    pub dropouts: f64,
    pub fault: Option<Fault>,
    pub seed: Option<u64>,
}

impl Default for ImageSettings {
    fn default() -> Self {
        ImageSettings { exposure: 1.0, noise: 0.0, blur: 0, dropouts: 0.0, fault: None, seed: None }
    }
}

/// [`ImageSettings`] at work on one camera's pictures.
pub struct ImageModel {
    settings: ImageSettings,
    rng: Rng,
    start: Option<u64>,
    frozen: Option<Vec<u8>>,
    scratch: Vec<u8>,
}

impl ImageModel {
    pub fn new(settings: ImageSettings, seed: u64) -> io::Result<ImageModel> {
        Fault::check(settings.fault, "a camera", &[Breakdown::Freeze, Breakdown::Dark])?;
        let rng = Rng::new(settings.seed.unwrap_or(seed));
        Ok(ImageModel { settings, rng, start: None, frozen: None, scratch: Vec::new() })
    }

    /// What the camera gives at `now`, from the rendered `image`.
    pub fn apply(&mut self, now: u64, image: &mut Image) {
        let s = &self.settings;
        let start = *self.start.get_or_insert(now);
        let fault = s.fault.filter(|f| f.active(start, now)).map(|f| f.what);
        if fault == Some(Breakdown::Freeze) {
            match &self.frozen {
                Some(frozen) if frozen.len() == image.pixels.len() => image.pixels.copy_from_slice(frozen),
                _ => self.frozen = Some(image.pixels.clone()),
            }
            return;
        }
        match image.format {
            Format::Rgb8 => {
                let gain = if fault == Some(Breakdown::Dark) { s.exposure * 0.05 } else { s.exposure };
                if s.blur > 0 {
                    box_blur(image, s.blur as usize, &mut self.scratch);
                }
                if gain != 1.0 || s.noise > 0.0 {
                    for p in image.pixels.iter_mut() {
                        let v = *p as f64 * gain + s.noise * self.rng.flat();
                        *p = v.round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
            Format::DepthF32 => {
                for px in image.pixels.as_chunks_mut::<4>().0 {
                    let mut d = f32::from_le_bytes(*px) as f64;
                    if fault == Some(Breakdown::Dark) || self.rng.unit() < s.dropouts {
                        d = 0.0;
                    } else if d > 0.0 {
                        d = (d + s.noise * self.rng.flat()).max(0.0);
                    }
                    px.copy_from_slice(&(d as f32).to_le_bytes());
                }
            }
        }
    }
}

/// Blurs an RGB image in place, a row pass then a column pass of a box
/// `2 * r + 1` wide: blur that costs the same whatever its width.
fn box_blur(image: &mut Image, r: usize, scratch: &mut Vec<u8>) {
    let (w, h) = (image.width as usize, image.height as usize);
    scratch.resize(image.pixels.len(), 0);
    let pass = |src: &[u8], dst: &mut [u8], n: usize, lines: usize, at: &dyn Fn(usize, usize) -> usize| {
        for line in 0..lines {
            for c in 0..3 {
                let mut sum = 0usize;
                let get = |i: isize| src[at(line, i.clamp(0, n as isize - 1) as usize) + c] as usize;
                for i in -(r as isize)..=(r as isize) {
                    sum += get(i);
                }
                for i in 0..n {
                    dst[at(line, i) + c] = (sum / (2 * r + 1)) as u8;
                    sum += get(i as isize + r as isize + 1);
                    sum -= get(i as isize - r as isize);
                }
            }
        }
    };
    pass(&image.pixels, scratch, w, h, &|y, x| (y * w + x) * 3);
    pass(scratch, &mut image.pixels, h, w, &|x, y| (y * w + x) * 3);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_perfect_sensor_reads_the_truth() {
        let mut model = VectorModel::new(VectorSettings::default(), 1).unwrap();
        let mut out = [0.0; 2];
        model.apply(0, &[1.0, -2.0], &mut out);
        assert_eq!(out, [1.0, -2.0]);
    }

    #[test]
    fn delay_quantum_and_range() {
        let settings = VectorSettings { delay_ms: 10.0, quantum: 0.5, range: 2.0, ..Default::default() };
        let mut model = VectorModel::new(settings, 1).unwrap();
        let mut out = [0.0];
        for (t, truth, read) in [(0, 0.2, 0.0), (5, 1.2, 0.0), (10, 3.1, 0.0), (15, 3.1, 1.0), (20, 3.1, 2.0)] {
            model.apply(t * 1_000_000, &[truth], &mut out);
            assert_eq!(out[0], read, "at {t} ms");
        }
    }

    #[test]
    fn noise_has_its_standard_deviation() {
        let mut model = VectorModel::new(VectorSettings { noise: 0.1, ..Default::default() }, 7).unwrap();
        let mut out = [0.0];
        let n = 20_000;
        let var = (0..n).map(|t| {
            model.apply(t, &[0.0], &mut out);
            out[0] * out[0]
        });
        let sd = (var.sum::<f64>() / n as f64).sqrt();
        assert!((sd - 0.1).abs() < 0.005, "{sd}");
    }

    #[test]
    fn a_frozen_sensor_repeats_itself() {
        let fault = Some(Fault { what: Breakdown::Freeze, after_s: 1.0 });
        let mut model = VectorModel::new(VectorSettings { fault, ..Default::default() }, 1).unwrap();
        let mut out = [0.0];
        model.apply(0, &[1.0], &mut out);
        model.apply(900_000_000, &[2.0], &mut out);
        assert_eq!(out[0], 2.0);
        model.apply(1_000_000_000, &[3.0], &mut out);
        model.apply(2_000_000_000, &[4.0], &mut out);
        assert_eq!(out[0], 2.0);
    }

    #[test]
    fn faults_parse_and_fit_their_device() {
        assert_eq!(Fault::parse("dead@20").unwrap(), Fault { what: Breakdown::Dead, after_s: 20.0 });
        assert_eq!(Fault::parse("freeze").unwrap().after_s, 0.0);
        assert!(Fault::parse("melted").is_err());
        let weak = Some(Fault { what: Breakdown::Weak, after_s: 0.0 });
        assert!(ImageModel::new(ImageSettings { fault: weak, ..Default::default() }, 1).is_err());
    }

    #[test]
    fn blur_keeps_a_flat_picture_flat_and_softens_an_edge() {
        let mut image = Image::new(8, 4, Format::Rgb8);
        image.pixels.fill(100);
        let mut scratch = Vec::new();
        box_blur(&mut image, 2, &mut scratch);
        assert!(image.pixels.iter().all(|&p| p == 100));
        for y in 0..4 {
            for x in 4..8 {
                image.pixels[(y * 8 + x) * 3..][..3].fill(200);
            }
        }
        box_blur(&mut image, 1, &mut scratch);
        let at = |x: usize| image.pixels[(8 + x) * 3];
        assert!(at(0) == 100 && at(3) > 100 && at(4) < 200 && at(7) == 200, "{:?}", &image.pixels[24..48]);
    }

    #[test]
    fn a_dark_or_frozen_camera() {
        let dark = Some(Fault { what: Breakdown::Dark, after_s: 0.0 });
        let mut model = ImageModel::new(ImageSettings { fault: dark, ..Default::default() }, 1).unwrap();
        let mut image = Image::new(2, 2, Format::Rgb8);
        image.pixels.fill(200);
        model.apply(0, &mut image);
        assert!(image.pixels.iter().all(|&p| p == 10));

        let freeze = Some(Fault { what: Breakdown::Freeze, after_s: 0.0 });
        let mut model = ImageModel::new(ImageSettings { fault: freeze, ..Default::default() }, 1).unwrap();
        image.pixels.fill(50);
        model.apply(0, &mut image);
        image.pixels.fill(60);
        model.apply(1, &mut image);
        assert!(image.pixels.iter().all(|&p| p == 50));
    }
}

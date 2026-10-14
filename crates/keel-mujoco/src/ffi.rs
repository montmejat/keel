//! The shim's functions (`shim/shim.c`), and safe owners for what they
//! return. MuJoCo's own types stay opaque here.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr, CString};
use std::io;
use std::path::Path;

use keel_control::{Format, Image};

#[repr(C)]
struct RawSpec {
    _opaque: [u8; 0],
}

#[repr(C)]
struct RawSim {
    _opaque: [u8; 0],
}

extern "C" {
    fn kmj_parse(path: *const c_char, err: *mut c_char, len: c_int) -> *mut RawSpec;
    fn kmj_free_spec(spec: *mut RawSpec);
    fn kmj_mount_site(
        spec: *mut RawSpec,
        body: *const c_char,
        name: *const c_char,
        pos: *const f64,
        quat: *const f64,
        err: *mut c_char,
        len: c_int,
    ) -> c_int;
    fn kmj_mount_sensor(
        spec: *mut RawSpec,
        kind: *const c_char,
        site: *const c_char,
        name: *const c_char,
        err: *mut c_char,
        len: c_int,
    ) -> c_int;
    fn kmj_mount_camera(
        spec: *mut RawSpec,
        body: *const c_char,
        name: *const c_char,
        pos: *const f64,
        quat: *const f64,
        fovy: f64,
        err: *mut c_char,
        len: c_int,
    ) -> c_int;
    fn kmj_compile(spec: *mut RawSpec, keyframe: *const c_char, err: *mut c_char, len: c_int) -> *mut RawSim;
    fn kmj_free(sim: *mut RawSim);
    fn kmj_timestep(sim: *const RawSim) -> f64;
    fn kmj_time(sim: *const RawSim) -> f64;
    fn kmj_step(sim: *mut RawSim);
    fn kmj_joints(sim: *const RawSim) -> c_int;
    fn kmj_joint_name(sim: *const RawSim, i: c_int) -> *const c_char;
    fn kmj_joint(sim: *const RawSim, i: c_int, pos: *mut f64, vel: *mut f64) -> c_int;
    fn kmj_take_over(sim: *mut RawSim, i: c_int, kp: *mut f64, kv: *mut f64, limit: *mut f64);
    fn kmj_set_torque(sim: *mut RawSim, i: c_int, torque: f64);
    fn kmj_sensor(sim: *mut RawSim, name: *const c_char, out: *mut f64, max: c_int) -> c_int;
    fn kmj_render_init(sim: *mut RawSim, width: c_int, height: c_int, err: *mut c_char, len: c_int) -> c_int;
    fn kmj_camera(sim: *const RawSim, name: *const c_char) -> c_int;
    fn kmj_render(sim: *mut RawSim, cam: c_int, width: c_int, height: c_int, rgb: *mut u8, depth: *mut f32);
}

fn cstring(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::other(format!("{s:?} has a NUL in it")))
}

/// Calls `f` with a buffer for an error message, and makes one of it when
/// `failed` says so.
fn checked<T>(f: impl FnOnce(*mut c_char, c_int) -> T, failed: impl FnOnce(&T) -> bool) -> io::Result<T> {
    let mut err = [0 as c_char; 1024];
    let out = f(err.as_mut_ptr(), err.len() as c_int);
    if failed(&out) {
        // SAFETY: the shim writes a NUL-terminated message into `err`.
        let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
        return Err(io::Error::other(msg));
    }
    Ok(out)
}

/// A scene being put together: parsed, with mounts to add.
pub struct Spec(*mut RawSpec);

impl Spec {
    pub fn parse(path: &Path) -> io::Result<Spec> {
        let p = cstring(&path.to_string_lossy())?;
        // SAFETY: valid strings and an error buffer of the given length.
        let raw = checked(|e, n| unsafe { kmj_parse(p.as_ptr(), e, n) }, |r| r.is_null())?;
        Ok(Spec(raw))
    }

    pub fn mount_site(&mut self, body: &str, name: &str, pos: [f64; 3], quat: [f64; 4]) -> io::Result<()> {
        let (body, name) = (cstring(body)?, cstring(name)?);
        // SAFETY: a live spec, valid strings, arrays of 3 and 4.
        checked(
            |e, n| unsafe { kmj_mount_site(self.0, body.as_ptr(), name.as_ptr(), pos.as_ptr(), quat.as_ptr(), e, n) },
            |r| *r != 0,
        )
        .map(drop)
    }

    pub fn mount_sensor(&mut self, kind: &str, site: &str, name: &str) -> io::Result<()> {
        let (kind, site, name) = (cstring(kind)?, cstring(site)?, cstring(name)?);
        // SAFETY: a live spec and valid strings.
        checked(
            |e, n| unsafe { kmj_mount_sensor(self.0, kind.as_ptr(), site.as_ptr(), name.as_ptr(), e, n) },
            |r| *r != 0,
        )
        .map(drop)
    }

    pub fn mount_camera(&mut self, body: &str, name: &str, pos: [f64; 3], quat: [f64; 4], fovy: f64) -> io::Result<()> {
        let (body, name) = (cstring(body)?, cstring(name)?);
        // SAFETY: a live spec, valid strings, arrays of 3 and 4.
        checked(
            |e, n| unsafe {
                kmj_mount_camera(self.0, body.as_ptr(), name.as_ptr(), pos.as_ptr(), quat.as_ptr(), fovy, e, n)
            },
            |r| *r != 0,
        )
        .map(drop)
    }

    /// The model, with its data, at `keyframe` if given.
    pub fn compile(self, keyframe: Option<&str>) -> io::Result<Sim> {
        let key = cstring(keyframe.unwrap_or(""))?;
        // SAFETY: a live spec; the model doesn't depend on it afterwards.
        let raw = checked(|e, n| unsafe { kmj_compile(self.0, key.as_ptr(), e, n) }, |r| r.is_null())?;
        Ok(Sim { raw, cameras: HashMap::new(), depth: Vec::new() })
    }
}

impl Drop for Spec {
    fn drop(&mut self) {
        // SAFETY: ours, freed once.
        unsafe { kmj_free_spec(self.0) }
    }
}

/// A compiled model and its data.
pub struct Sim {
    raw: *mut RawSim,
    cameras: HashMap<String, c_int>,
    /// Depth, as the shim writes it.
    depth: Vec<f32>,
}

// SAFETY notes for every call below: `raw` is a live simulation owned by
// this struct, indices are below `joints()`, and buffers have the lengths
// passed with them.
impl Sim {
    pub fn timestep(&self) -> f64 {
        unsafe { kmj_timestep(self.raw) }
    }

    pub fn time(&self) -> f64 {
        unsafe { kmj_time(self.raw) }
    }

    pub fn step(&mut self) {
        unsafe { kmj_step(self.raw) }
    }

    pub fn joints(&self) -> usize {
        unsafe { kmj_joints(self.raw) as usize }
    }

    pub fn joint_name(&self, i: usize) -> String {
        unsafe { CStr::from_ptr(kmj_joint_name(self.raw, i as c_int)) }.to_string_lossy().into_owned()
    }

    /// Joint `i`'s position and velocity.
    pub fn joint(&self, i: usize) -> (f64, f64) {
        let (mut pos, mut vel) = (0.0, 0.0);
        unsafe { kmj_joint(self.raw, i as c_int, &mut pos, &mut vel) };
        (pos, vel)
    }

    /// Makes actuator `i` a motor, and returns its servo's gains and force
    /// limit (0: none).
    pub fn take_over(&mut self, i: usize) -> (f64, f64, f64) {
        let (mut kp, mut kv, mut limit) = (0.0, 0.0, 0.0);
        unsafe { kmj_take_over(self.raw, i as c_int, &mut kp, &mut kv, &mut limit) };
        (kp, kv, limit)
    }

    pub fn set_torque(&mut self, i: usize, torque: f64) {
        unsafe { kmj_set_torque(self.raw, i as c_int, torque) }
    }

    pub fn sensor(&self, name: &str, out: &mut Vec<f64>) -> io::Result<()> {
        let c = cstring(name)?;
        let mut values = [0.0; 64];
        let dim = unsafe { kmj_sensor(self.raw, c.as_ptr(), values.as_mut_ptr(), values.len() as c_int) };
        if dim < 0 {
            return Err(io::Error::other(format!("no sensor {name:?} in the scene")));
        }
        out.extend_from_slice(&values[..(dim as usize).min(values.len())]);
        Ok(())
    }

    pub fn render_init(&mut self, width: u32, height: u32) -> io::Result<()> {
        checked(|e, n| unsafe { kmj_render_init(self.raw, width as c_int, height as c_int, e, n) }, |r| *r != 0)
            .map(drop)
    }

    pub fn render(&mut self, camera: &str, image: &mut Image) -> io::Result<()> {
        let cam = match self.cameras.get(camera) {
            Some(&id) => id,
            None => {
                let c = cstring(camera)?;
                let id = unsafe { kmj_camera(self.raw, c.as_ptr()) };
                if id < 0 {
                    return Err(io::Error::other(format!("no camera {camera:?} in the scene")));
                }
                self.cameras.insert(camera.into(), id);
                id
            }
        };
        let (w, h) = (image.width as c_int, image.height as c_int);
        match image.format {
            Format::Rgb8 => unsafe {
                kmj_render(self.raw, cam, w, h, image.pixels.as_mut_ptr(), std::ptr::null_mut());
            },
            Format::DepthF32 => {
                self.depth.resize(image.pixels.len() / 4, 0.0);
                unsafe { kmj_render(self.raw, cam, w, h, std::ptr::null_mut(), self.depth.as_mut_ptr()) };
                for (out, d) in image.pixels.as_chunks_mut::<4>().0.iter_mut().zip(&self.depth) {
                    *out = d.to_le_bytes();
                }
            }
        }
        Ok(())
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        unsafe { kmj_free(self.raw) }
    }
}

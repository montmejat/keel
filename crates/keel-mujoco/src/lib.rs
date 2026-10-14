//! A simulated robot in MuJoCo, as keel-control's simulated devices: its
//! joints, the sensors and cameras mounted on it, each with its hardware
//! model, all over one MuJoCo world.
//!
//! What the robot is comes from its robot file ([`Robot`]): the scene (an
//! MJCF file, such as one from MuJoCo Menagerie), where its sensors and
//! cameras are mounted, and what its hardware is like. MuJoCo plays the
//! physics only: the scene's position servos become plain motors, and the
//! drives in front of them are keel-control's drive model, which starts with
//! the servos' own gains.
//!
//! MuJoCo is reached through a small C shim (`shim/shim.c`), so nothing here
//! mirrors MuJoCo's structs. Rendering is MuJoCo's classic OpenGL renderer,
//! headless through EGL.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use keel::Node;
use keel_control::drive::DriveSettings;
use keel_control::law::Pid;
use keel_control::model::{Fault, ImageSettings, VectorSettings};
use keel_control::schedule::{CameraLoop, JointLoop, JointSettings, LoopAt, SensorLoop, Tick};
use keel_control::sim::{Physics, Shared, SimCamera, SimJoints, SimSensor, World};
#[cfg(mujoco)]
use keel_control::Image;
use keel_control::{Format, State};

#[cfg(mujoco)]
mod ffi;

/// The robot file.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Robot {
    /// The MJCF scene, relative to the robot file.
    pub scene: PathBuf,
    /// A keyframe to start from.
    #[serde(default)]
    pub keyframe: Option<String>,
    pub joints: JointsFile,
    #[serde(default)]
    pub sensors: HashMap<String, SensorFile>,
    #[serde(default)]
    pub cameras: HashMap<String, CameraFile>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JointsFile {
    #[serde(default = "default_joint_hz")]
    pub hz: f64,
    /// Where position loops close: `drive` (the simulated drives) or `keel`.
    #[serde(default)]
    pub r#loop: LoopAt,
    /// Every drive's settings; without a `pid`, each drive has its joint's
    /// servo gains from the scene.
    #[serde(default)]
    pub drive: serde_yaml::Value,
    /// Every encoder's (see [`VectorSettings`]).
    #[serde(default)]
    pub encoder: serde_yaml::Value,
    /// Per joint (by actuator name), on top of the above: `drive`, `encoder`.
    #[serde(default)]
    pub each: HashMap<String, JointFile>,
}

fn default_joint_hz() -> f64 {
    500.0
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JointFile {
    #[serde(default)]
    pub drive: serde_yaml::Value,
    #[serde(default)]
    pub encoder: serde_yaml::Value,
}

/// Where a sensor or a camera sits: on a body of the scene, at a position,
/// turned by `euler` (degrees, about the body's x, then y, then z axes).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub on: String,
    #[serde(default)]
    pub at: [f64; 3],
    #[serde(default)]
    pub euler: [f64; 3],
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorFile {
    /// Mounted here, with MuJoCo sensors of these kinds (accelerometer,
    /// gyro, force, torque, rangefinder)...
    #[serde(default)]
    pub mount: Option<Mount>,
    #[serde(default)]
    pub kinds: Vec<String>,
    /// ...or the scene's own sensors, by name.
    #[serde(default)]
    pub mjcf: Vec<String>,
    pub hz: f64,
    #[serde(default)]
    pub model: VectorSettings,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraFile {
    /// Mounted here, with this vertical field of view...
    #[serde(default)]
    pub mount: Option<Mount>,
    #[serde(default = "default_fovy")]
    pub fovy: f64,
    /// ...or the scene's own camera, by name.
    #[serde(default)]
    pub mjcf: Option<String>,
    /// Width and height, in pixels.
    pub size: [u32; 2],
    pub hz: f64,
    /// Distances in metres, rather than colours.
    #[serde(default)]
    pub depth: bool,
    #[serde(default)]
    pub model: ImageSettings,
}

fn default_fovy() -> f64 {
    60.0
}

impl Robot {
    pub fn read(path: &Path) -> io::Result<Robot> {
        let text = std::fs::read_to_string(path).map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
        let mut robot: Robot =
            serde_yaml::from_str(&text).map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
        robot.scene = path.parent().unwrap_or(Path::new(".")).join(&robot.scene);
        Ok(robot)
    }

    /// Breaks a device: `joint2:dead@10`, `imu:freeze@5`, `wrist:dark`
    /// (`<device>:<fault>@<seconds>`), as a command line gives it.
    pub fn break_device(&mut self, spec: &str) -> io::Result<()> {
        let (device, fault) =
            spec.split_once(':').ok_or_else(|| io::Error::other(format!("{spec:?}: <device>:<fault>[@<seconds>]")))?;
        let fault = Fault::parse(fault)?;
        if let Some(sensor) = self.sensors.get_mut(device) {
            sensor.model.fault = Some(fault);
        } else if let Some(camera) = self.cameras.get_mut(device) {
            camera.model.fault = Some(fault);
        } else {
            // A joint: its drive breaks down, or its encoder.
            let joint = self.joints.each.entry(device.to_string()).or_default();
            let part = match fault.what {
                keel_control::model::Breakdown::Dead | keel_control::model::Breakdown::Weak => &mut joint.drive,
                _ => &mut joint.encoder,
            };
            let mut map = part.as_mapping().cloned().unwrap_or_default();
            map.insert("fault".into(), serde_yaml::to_value(FaultFile::from(fault)).unwrap());
            *part = serde_yaml::Value::Mapping(map);
        }
        Ok(())
    }
}

/// A fault, back into the robot file's terms.
#[derive(serde::Serialize)]
struct FaultFile {
    what: String,
    after_s: f64,
}

impl From<Fault> for FaultFile {
    fn from(f: Fault) -> Self {
        FaultFile { what: format!("{:?}", f.what).to_lowercase(), after_s: f.after_s }
    }
}

/// `over` on top of `base`, key by key, maps within maps.
fn merge(base: &serde_yaml::Value, over: &serde_yaml::Value) -> serde_yaml::Value {
    match (base, over) {
        (serde_yaml::Value::Mapping(b), serde_yaml::Value::Mapping(o)) => {
            let mut out = b.clone();
            for (k, v) in o {
                let merged = out.get(k).map_or_else(|| v.clone(), |bv| merge(bv, v));
                out.insert(k.clone(), merged);
            }
            serde_yaml::Value::Mapping(out)
        }
        (_, serde_yaml::Value::Null) => base.clone(),
        _ => over.clone(),
    }
}

fn settings<T: serde::de::DeserializeOwned>(what: &str, value: serde_yaml::Value) -> io::Result<T> {
    let value = if value.is_null() { serde_yaml::Value::Mapping(Default::default()) } else { value };
    serde_yaml::from_value(value).map_err(|e| io::Error::other(format!("{what}: {e}")))
}

/// A rotation of `euler` degrees about fixed x, y, z axes, as a quaternion
/// (w, x, y, z).
pub fn quat(euler: [f64; 3]) -> [f64; 4] {
    let half = |deg: f64| deg.to_radians() / 2.0;
    let mul = |a: [f64; 4], b: [f64; 4]| {
        [
            a[0] * b[0] - a[1] * b[1] - a[2] * b[2] - a[3] * b[3],
            a[0] * b[1] + a[1] * b[0] + a[2] * b[3] - a[3] * b[2],
            a[0] * b[2] - a[1] * b[3] + a[2] * b[0] + a[3] * b[1],
            a[0] * b[3] + a[1] * b[2] - a[2] * b[1] + a[3] * b[0],
        ]
    };
    let (x, y, z) = (half(euler[0]), half(euler[1]), half(euler[2]));
    let qx = [x.cos(), x.sin(), 0.0, 0.0];
    let qy = [y.cos(), 0.0, y.sin(), 0.0];
    let qz = [z.cos(), 0.0, 0.0, z.sin()];
    mul(qz, mul(qy, qx))
}

/// MuJoCo, as keel-control's [`Physics`].
pub struct MuJoCo {
    #[cfg(mujoco)]
    sim: ffi::Sim,
    /// The joints' names, and their servos' gains in the scene.
    pub joints: Vec<(String, Pid)>,
}

/// What a simulated robot is, ready to run: its world, and a loop per
/// device.
pub struct Simulation {
    pub world: Shared,
    pub loops: Vec<Box<dyn Tick>>,
    /// What each device publishes on, for the log.
    pub outputs: Vec<String>,
}

impl Simulation {
    /// The robot of `robot`, its joints' loop running as `joints` says
    /// (rate and loop placement come from the robot file).
    pub fn build(robot: &Robot, mut joints: JointSettings, node: &Node) -> io::Result<Simulation> {
        let mut mujoco = MuJoCo::load(robot)?;
        let names: Vec<String> = mujoco.joints.iter().map(|(n, _)| n.clone()).collect();
        for name in robot.joints.each.keys() {
            if !names.contains(name) {
                return Err(io::Error::other(format!("no joint {name:?} in the scene: its joints are {names:?}")));
            }
        }
        let each = |j: usize| robot.joints.each.get(&names[j]).cloned().unwrap_or_default();
        // A drive starts as its joint's servo in the scene.
        let mut drives = Vec::new();
        for (j, (name, servo)) in mujoco.joints.iter().enumerate() {
            let servo = serde_yaml::to_value(ServoFile::from(*servo)).unwrap();
            let base =
                merge(&serde_yaml::Value::Mapping([("pid".into(), servo)].into_iter().collect()), &robot.joints.drive);
            drives.push(settings::<DriveSettings>(&format!("{name}'s drive"), merge(&base, &each(j).drive))?);
        }
        let encoders = (0..names.len())
            .map(|j| {
                let encoder = merge(&robot.joints.encoder, &each(j).encoder);
                settings::<VectorSettings>(&format!("{}'s encoder", names[j]), encoder)
            })
            .collect::<io::Result<Vec<_>>>()?;

        // Closing loops in keel: each joint with its servo's gains too.
        let pids: Vec<Pid> = drives.iter().map(|d| d.pid).collect();
        let sizes: Vec<[u32; 2]> = robot.cameras.values().map(|c| c.size).collect();
        mujoco.prepare_rendering(&sizes)?;
        let world = World::new(Box::new(mujoco), |j| drives[j].clone())?;

        joints.hz = robot.joints.hz;
        joints.loop_at = robot.joints.r#loop;
        let device = SimJoints::new(&world, |j| encoders[j].clone())?;
        let mut loops: Vec<Box<dyn Tick>> = vec![Box::new(JointLoop::new(device, joints, node)?.with_laws(pids))];
        let mut outputs = vec![format!("state ({} joints: {}, {} Hz)", names.len(), names.join(" "), joints.hz)];

        let mut sensors: Vec<_> = robot.sensors.iter().collect();
        sensors.sort_by_key(|(name, _)| name.as_str());
        for (name, sensor) in sensors {
            let mjcf = if sensor.mount.is_some() {
                sensor.kinds.iter().map(|kind| format!("{name}_{kind}")).collect()
            } else {
                sensor.mjcf.clone()
            };
            let device = SimSensor::new(&world, name, &mjcf, sensor.model.clone())?;
            let n = keel_control::device::VectorSensor::count(&device);
            outputs.push(format!("{name} ({n} values: {}, {} Hz)", mjcf.join(" "), sensor.hz));
            loops.push(Box::new(SensorLoop::new(device, sensor.hz, name)));
        }
        let mut cameras: Vec<_> = robot.cameras.iter().collect();
        cameras.sort_by_key(|(name, _)| name.as_str());
        for (name, camera) in cameras {
            let mjcf = camera.mjcf.clone().unwrap_or_else(|| name.clone());
            let format = if camera.depth { Format::DepthF32 } else { Format::Rgb8 };
            let size = (camera.size[0], camera.size[1]);
            let device = SimCamera::new(&world, name, &mjcf, size, format, camera.model.clone())?;
            let what = if camera.depth { "depth" } else { "colour" };
            outputs.push(format!("{name} ({}x{} {what}, {} Hz)", size.0, size.1, camera.hz));
            loops.push(Box::new(CameraLoop::new(device, camera.hz, name)));
        }
        Ok(Simulation { world, loops, outputs })
    }
}

/// A servo's gains, in the drive settings' terms.
#[derive(serde::Serialize)]
struct ServoFile {
    kp: f64,
    ki: f64,
    kd: f64,
    limit: f64,
}

impl From<Pid> for ServoFile {
    fn from(p: Pid) -> Self {
        ServoFile { kp: p.kp, ki: p.ki, kd: p.kd, limit: p.limit }
    }
}

#[cfg(not(mujoco))]
impl MuJoCo {
    pub fn load(_robot: &Robot) -> io::Result<MuJoCo> {
        Err(io::Error::other(
            "keel-mujoco was built without MuJoCo: run examples/mujoco/setup.sh, then build again (or set MUJOCO_DIR)",
        ))
    }

    fn prepare_rendering(&mut self, _sizes: &[[u32; 2]]) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(not(mujoco))]
impl Physics for MuJoCo {
    fn time_ns(&self) -> u64 {
        0
    }
    fn timestep_ns(&self) -> u64 {
        1
    }
    fn joints(&self) -> usize {
        0
    }
    fn joint(&self, _i: usize) -> State {
        State::default()
    }
    fn set_torque(&mut self, _i: usize, _torque: f64) {}
    fn step(&mut self) {}
}

#[cfg(mujoco)]
impl MuJoCo {
    pub fn load(robot: &Robot) -> io::Result<MuJoCo> {
        let mut spec = ffi::Spec::parse(&robot.scene)?;
        // In order: the model's numbering is then the same every run.
        let mut sensors: Vec<_> = robot.sensors.iter().collect();
        sensors.sort_by_key(|(name, _)| name.as_str());
        let mut cameras: Vec<_> = robot.cameras.iter().collect();
        cameras.sort_by_key(|(name, _)| name.as_str());
        for (name, sensor) in sensors {
            match (&sensor.mount, sensor.mjcf.is_empty()) {
                (Some(mount), true) => {
                    let site = format!("{name}_site");
                    spec.mount_site(&mount.on, &site, mount.at, quat(mount.euler))?;
                    for kind in &sensor.kinds {
                        spec.mount_sensor(kind, &site, &format!("{name}_{kind}"))?;
                    }
                }
                (None, false) => {}
                _ => return Err(io::Error::other(format!("sensor {name:?}: a `mount` with `kinds`, or `mjcf` names"))),
            }
        }
        for (name, camera) in cameras {
            match (&camera.mount, &camera.mjcf) {
                (Some(mount), None) => spec.mount_camera(&mount.on, name, mount.at, quat(mount.euler), camera.fovy)?,
                (None, Some(_)) => {}
                _ => return Err(io::Error::other(format!("camera {name:?}: a `mount`, or an `mjcf` camera"))),
            }
        }
        let mut sim = spec.compile(robot.keyframe.as_deref())?;
        let joints = (0..sim.joints())
            .map(|j| {
                let (kp, kd, limit) = sim.take_over(j);
                // A servo without a force limit gets a generous one.
                (sim.joint_name(j), Pid::new(kp, 0.0, kd, if limit > 0.0 { limit } else { 1000.0 }))
            })
            .collect();
        Ok(MuJoCo { sim, joints })
    }

    fn prepare_rendering(&mut self, sizes: &[[u32; 2]]) -> io::Result<()> {
        match (sizes.iter().map(|s| s[0]).max(), sizes.iter().map(|s| s[1]).max()) {
            (Some(w), Some(h)) => self.sim.render_init(w, h),
            _ => Ok(()),
        }
    }
}

#[cfg(mujoco)]
impl Physics for MuJoCo {
    fn time_ns(&self) -> u64 {
        (self.sim.time() * 1e9).round() as u64
    }

    fn timestep_ns(&self) -> u64 {
        (self.sim.timestep() * 1e9).round() as u64
    }

    fn joints(&self) -> usize {
        self.joints.len()
    }

    fn joint(&self, i: usize) -> State {
        let (position, velocity) = self.sim.joint(i);
        State { position, velocity }
    }

    fn set_torque(&mut self, i: usize, torque: f64) {
        self.sim.set_torque(i, torque);
    }

    fn step(&mut self) {
        self.sim.step();
    }

    fn sensor(&self, name: &str, out: &mut Vec<f64>) -> io::Result<()> {
        self.sim.sensor(name, out)
    }

    fn render(&mut self, camera: &str, image: &mut Image) -> io::Result<()> {
        self.sim.render(camera, image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn euler_angles_turn_the_right_way() {
        let close = |a: [f64; 4], b: [f64; 4]| a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-9);
        let h = std::f64::consts::FRAC_1_SQRT_2;
        assert!(close(quat([0.0, 0.0, 0.0]), [1.0, 0.0, 0.0, 0.0]));
        assert!(close(quat([90.0, 0.0, 0.0]), [h, h, 0.0, 0.0]));
        assert!(close(quat([0.0, 0.0, 90.0]), [h, 0.0, 0.0, h]));
        assert!(close(quat([180.0, 0.0, 0.0]), [0.0, 1.0, 0.0, 0.0]));
    }

    #[test]
    fn overrides_merge_into_settings() {
        let base: serde_yaml::Value = serde_yaml::from_str("{pid: {kp: 80, kd: 5}, delay_ms: 1}").unwrap();
        let over: serde_yaml::Value = serde_yaml::from_str("{pid: {kp: 10}, fault: {what: dead, after_s: 2}}").unwrap();
        let drive: DriveSettings = settings("drive", merge(&base, &over)).unwrap();
        assert_eq!((drive.pid.kp, drive.pid.kd, drive.delay_ms), (10.0, 5.0, 1.0));
        assert_eq!(drive.fault, Some(Fault::parse("dead@2").unwrap()));
    }

    #[test]
    fn faults_from_the_command_line() {
        let mut robot: Robot = serde_yaml::from_str(
            "{scene: s.xml, joints: {}, sensors: {imu: {mjcf: [a], hz: 100}}, cameras: {cam: {mjcf: c, size: [4, 3], hz: 5}}}",
        )
        .unwrap();
        robot.break_device("joint2:dead@10").unwrap();
        robot.break_device("joint3:freeze").unwrap();
        robot.break_device("imu:noisy@1").unwrap();
        robot.break_device("cam:dark@2").unwrap();
        let drive: DriveSettings = settings("", robot.joints.each["joint2"].drive.clone()).unwrap();
        assert_eq!(drive.fault, Some(Fault::parse("dead@10").unwrap()));
        let encoder: VectorSettings = settings("", robot.joints.each["joint3"].encoder.clone()).unwrap();
        assert_eq!(encoder.fault, Some(Fault::parse("freeze").unwrap()));
        assert_eq!(robot.sensors["imu"].model.fault, Some(Fault::parse("noisy@1").unwrap()));
        assert_eq!(robot.cameras["cam"].model.fault, Some(Fault::parse("dark@2").unwrap()));
        assert!(robot.break_device("joint2").is_err());
    }
}

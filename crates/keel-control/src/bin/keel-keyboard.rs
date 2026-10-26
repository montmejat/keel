//! Steering a walking robot from the keyboard: a `walk` command
//! (`keel_control::Walk`), for `keel-policy`'s `walk` input.
//!
//! One program, two roles. **Started by keel**, it's the node: it listens
//! on a local socket for keys, and publishes the command they make each
//! cycle (50 Hz). **Started in a terminal**, it sends that terminal's keys
//! to the node's socket, and shows the command:
//!
//! ```text
//! keel-keyboard        # in another terminal, while the dataflow runs
//! ```
//!
//! w/s or ↑/↓: faster forward or back; a/d or ←/→: turn left or right; q/e:
//! step left or right; space: stop; Ctrl-C: quit. Nodes can't read a
//! terminal (keel starts them in their own process group), hence the socket.
//! If the terminal goes, the node asks the robot to stop.
//!
//! `--socket <path>` (both sides; `$XDG_RUNTIME_DIR/keel-keyboard.sock` by
//! default), `--max 0.4,0.3,1.0` (the node: the fastest forward, sideways
//! and turning it asks for: the Microduck's policies' training ranges).

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use keel::{Event, Node, Periodic};
use keel_control::{arg, joints, Walk};

fn main() -> std::process::ExitCode {
    let started_by_keel = std::env::var_os(keel::protocol::ENV_NODE_ID).is_some();
    joints::exit_on(if started_by_keel { node() } else { terminal() })
}

fn socket() -> io::Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    Ok(arg("socket", dir.join("keel-keyboard.sock").display().to_string())?.into())
}

/// How keys change a command, within `max` (forward, left, turn).
fn press(walk: &mut Walk, key: Key, max: [f64; 3]) {
    let (step, turn) = (0.05, 0.2);
    match key {
        Key::Forward => walk.forward += step,
        Key::Back => walk.forward -= step,
        Key::TurnLeft => walk.turn += turn,
        Key::TurnRight => walk.turn -= turn,
        Key::StepLeft => walk.left += step,
        Key::StepRight => walk.left -= step,
        Key::Stop => *walk = Walk::default(),
    }
    // Rounded to the steps, so a few presses there and back come to zero.
    walk.forward = (walk.forward / step).round() * step;
    walk.left = (walk.left / step).round() * step;
    walk.turn = (walk.turn / turn).round() * turn;
    walk.forward = walk.forward.clamp(-max[0], max[0]);
    walk.left = walk.left.clamp(-max[1], max[1]);
    walk.turn = walk.turn.clamp(-max[2], max[2]);
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Key {
    Forward,
    Back,
    TurnLeft,
    TurnRight,
    StepLeft,
    StepRight,
    Stop,
}

/// The keys in `bytes`, from the start, and how many bytes they took;
/// arrows are three (ESC [ A..D). An arrow cut in two waits for its rest.
fn keys(bytes: &[u8]) -> (Vec<Key>, usize) {
    let (mut out, mut i) = (Vec::new(), 0);
    while i < bytes.len() {
        let key = match bytes[i] {
            b'w' | b'W' => Some(Key::Forward),
            b's' | b'S' => Some(Key::Back),
            b'a' | b'A' => Some(Key::TurnLeft),
            b'd' | b'D' => Some(Key::TurnRight),
            b'q' | b'Q' => Some(Key::StepLeft),
            b'e' | b'E' => Some(Key::StepRight),
            b' ' => Some(Key::Stop),
            0x1B => {
                if bytes.len() < i + 3 {
                    break;
                }
                i += 2;
                match bytes[i] {
                    b'A' => Some(Key::Forward),
                    b'B' => Some(Key::Back),
                    b'C' => Some(Key::TurnRight),
                    b'D' => Some(Key::TurnLeft),
                    _ => None,
                }
            }
            _ => None,
        };
        out.extend(key);
        i += 1;
    }
    (out, i)
}

fn describe(w: &Walk) -> String {
    format!("forward {:+.2} m/s, left {:+.2} m/s, turn {:+.1} rad/s", w.forward, w.left, w.turn)
}

/// The node: keys from the socket, a `walk` each cycle.
fn node() -> io::Result<()> {
    let path = socket()?;
    let max: String = arg("max", "0.4,0.3,1.0".to_string())?;
    let max: Vec<f64> = max
        .split(',')
        .map(|s| s.trim().parse())
        .collect::<Result<_, _>>()
        .map_err(|_| io::Error::other("--max: three numbers"))?;
    let max: [f64; 3] = max.try_into().map_err(|_| io::Error::other("--max: forward, left, turn"))?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    println!("keys at {}: run `keel-keyboard` in a terminal to steer", path.display());
    let mut node = Node::from_env()?;
    let (mut walk, mut client, mut pending) = (Walk::default(), None::<UnixStream>, Vec::new());
    let mut periodic = Periodic::new(Duration::from_millis(20));
    let result = loop {
        periodic.wait();
        match node.try_next_event() {
            Ok(Some(Event::Stop)) => break Ok(()),
            Err(e) => break Err(e),
            _ => {}
        }
        if let Ok((stream, _)) = listener.accept() {
            stream.set_nonblocking(true)?;
            println!("a keyboard is steering");
            let mut stream = stream;
            let _ = writeln!(stream, "{}", describe(&walk));
            client = Some(stream);
            pending.clear();
        }
        let mut gone = false;
        if let Some(stream) = client.as_mut() {
            let mut buf = [0u8; 64];
            match stream.read(&mut buf) {
                Ok(0) => gone = true,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    let (pressed, used) = keys(&pending);
                    pending.drain(..used);
                    let before = walk;
                    for key in pressed {
                        press(&mut walk, key, max);
                    }
                    if walk != before {
                        println!("{}", describe(&walk));
                        let _ = writeln!(stream, "{}", describe(&walk));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => gone = true,
            }
        }
        if gone {
            client = None;
            if walk != Walk::default() {
                walk = Walk::default();
                println!("the keyboard is gone: stopping");
            }
        }
        if let Err(e) = node.send_with("walk", Walk::LEN, |p| walk.write(p)) {
            break Err(e);
        }
    };
    let _ = std::fs::remove_file(&path);
    result
}

/// The terminal: its keys to the node, raw, until Ctrl-C.
fn terminal() -> io::Result<()> {
    let path = socket()?;
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        io::Error::other(format!(
            "no keyboard node at {} ({e}): is the dataflow running, with keel-keyboard in it?",
            path.display()
        ))
    })?;
    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    // SAFETY: termios calls on stdin, with a struct that lives through them.
    let saved = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return Err(io::Error::other("keel-keyboard reads keys from a terminal: run it in one"));
        }
        let saved = t;
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(fd, libc::TCSANOW, &t);
        saved
    };
    println!("w/s ↑/↓ forward and back, a/d ←/→ turn, q/e step aside, space stop, Ctrl-C quit\r");
    let mut status = stream.try_clone()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        while let Ok(n) = status.read(&mut buf) {
            if n == 0 {
                break;
            }
            for line in String::from_utf8_lossy(&buf[..n]).lines() {
                print!("\r\x1b[K{line}");
                let _ = io::stdout().flush();
            }
        }
    });
    let mut key = [0u8; 16];
    let result = loop {
        match stdin.lock().read(&mut key) {
            Ok(0) => break Ok(()),
            Ok(n) if key[..n].contains(&0x03) => break Ok(()),
            Ok(n) => {
                if let Err(e) = stream.write_all(&key[..n]) {
                    break Err(io::Error::other(format!("the keyboard node has gone ({e})")));
                }
            }
            Err(e) => break Err(e),
        }
    };
    // SAFETY: as above.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    println!("\r");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_arrows_read_and_an_arrow_cut_in_two_waits() {
        let (pressed, used) = keys(b"w d\x1b[A");
        assert_eq!(pressed, [Key::Forward, Key::Stop, Key::TurnRight, Key::Forward]);
        assert_eq!(used, 6);
        let (pressed, used) = keys(b"s\x1b[");
        assert_eq!((pressed, used), (vec![Key::Back], 1), "the arrow's rest hasn't come");
    }

    #[test]
    fn presses_step_and_stop_within_the_limits() {
        let (mut w, max) = (Walk::default(), [0.4, 0.3, 1.0]);
        for _ in 0..20 {
            press(&mut w, Key::Forward, max);
        }
        press(&mut w, Key::TurnLeft, max);
        assert_eq!((w.forward, w.turn), (0.4, 0.2));
        press(&mut w, Key::TurnRight, max);
        assert_eq!(w.turn, 0.0, "back to zero, not a rounding error away");
        press(&mut w, Key::Stop, max);
        assert_eq!(w, Walk::default());
    }
}

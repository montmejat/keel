//! A node that records every input it has to a file, for `keel replay` and
//! `keel export`.
//!
//! The file is its first argument, or
//! `~/.local/share/keel/recordings/<dataflow>-<unix time>.keel`. Recording
//! big messages to a slow disk can fall behind: give such inputs
//! `keep: latest` so the sender never waits for the recorder, at the cost of
//! the messages it had to skip.
//!
//! With `--last <seconds>` it's a flight recorder instead: it keeps only the
//! last seconds, in memory, and the daemon saves them to that directory when
//! a node fails (see `keel_record::flight`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use keel::protocol::ENV_SHM_DIR;
use keel::{Event, Node};
use keel_record::flight::{self, Ring};
use keel_record::{Channel, Header, Writer};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    let (dataflow, deployment) = node.dataflow();
    let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let last = match args.first() {
        Some(flag) if flag == "--last" => {
            let seconds = args.get(1).and_then(|s| s.to_str()?.parse::<f64>().ok()).filter(|s| *s > 0.0);
            Some(Duration::from_secs_f64(seconds.ok_or_else(|| std::io::Error::other("--last <seconds>"))?))
        }
        _ => None,
    };

    let inputs = node.inputs();
    let channel: HashMap<&str, u16> = inputs.iter().enumerate().map(|(i, (input, _))| (*input, i as u16)).collect();
    let channels =
        inputs.iter().map(|(input, source)| Channel { input: input.to_string(), source: source.to_string() });
    let header = Header {
        dataflow: dataflow.clone(),
        deployment,
        recorder: node.id().to_owned(),
        started,
        channels: channels.collect(),
        failure: None,
    };
    let sources: Vec<&str> = inputs.iter().map(|(_, source)| *source).collect();

    if let Some(last) = last {
        let shm_dir = PathBuf::from(std::env::var_os(ENV_SHM_DIR).unwrap_or_default());
        let mut ring = Ring::create(flight::root(&shm_dir).join(node.id()), header, last)?;
        println!("keeping the last {last:?} of {}, in memory", sources.join(", "));
        while let Event::Input { id, data } = node.next_event()? {
            let context = data.context();
            ring.write(channel[id], context.span, context.published_ns, &data)?;
        }
        return Ok(());
    }

    let path = match args.first() {
        Some(path) => PathBuf::from(path),
        None => {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            let data = std::env::var_os("XDG_DATA_HOME").map_or(home.join(".local/share"), PathBuf::from);
            data.join("keel/recordings").join(format!("{dataflow}-{started}.keel"))
        }
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut writer = Writer::create(&path, &header)?;
    println!("recording {} to {}", sources.join(", "), path.display());

    let (mut count, mut bytes) = (0u64, 0u64);
    while let Event::Input { id, data } = node.next_event()? {
        let context = data.context();
        writer.write(channel[id], context.span, context.published_ns, &data)?;
        count += 1;
        bytes += data.len() as u64;
    }
    writer.flush()?;
    println!("recorded {count} messages, {:.1} MiB, to {}", bytes as f64 / (1 << 20) as f64, path.display());
    Ok(())
}

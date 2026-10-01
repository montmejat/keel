//! Recordings (see `keel_record`): look inside one, replay it in place of the
//! nodes that produced it, or export it as a dataset.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use keel::Node;
use keel_daemon::dataflow::Dataflow;
use keel_record::Reader;

use crate::fmt;

/// `keel recording <file>`: what's in it.
pub fn info(path: &Path) -> io::Result<()> {
    let mut reader = Reader::open(path)?;
    let h = reader.header.clone();
    println!("dataflow   {}", h.dataflow);
    println!("deployment {}", h.deployment.as_deref().unwrap_or("(none: not deployed)"));
    println!("recorder   {}, started {}", h.recorder, fmt::age(h.started));
    if let Some(f) = &h.failure {
        println!("kept       because `{}` failed ({}), {}", f.node, f.status, fmt::age(f.at));
    }
    let mut stats = vec![(0u64, 0u64); h.channels.len()];
    let (mut first, mut last) = (u64::MAX, 0);
    while let Some(record) = reader.next_record()? {
        let s = &mut stats[record.channel as usize];
        (s.0, s.1) = (s.0 + 1, s.1 + record.payload.len() as u64);
        (first, last) = (first.min(record.t_ns), last.max(record.t_ns));
    }
    let span = Duration::from_nanos(last.saturating_sub(first));
    println!("duration   {:.1}s{}", span.as_secs_f64(), if reader.truncated { " (last record cut short)" } else { "" });
    println!("\n{:<24} {:<24} {:>9} {:>11}", "CHANNEL", "FROM", "MESSAGES", "BYTES");
    for (channel, (count, bytes)) in h.channels.iter().zip(stats) {
        println!("{:<24} {:<24} {:>9} {:>11}", channel.input, channel.source, count, fmt::bytes(bytes as f64));
    }
    Ok(())
}

/// `keel replay <file> <dataflow>`: runs the dataflow on this machine with
/// every recorded source node (one without inputs) replaced by a node that
/// publishes what it recorded, at the recorded pace (times `speed`; 0 means
/// as fast as possible). The other nodes run as usual and can't tell.
pub fn replay(recording: &Path, dataflow_path: &Path, speed: f64) -> io::Result<bool> {
    let recording = std::fs::canonicalize(recording)?;
    let header = Reader::open(&recording)?.header;
    let recorded: BTreeSet<&str> = header.channels.iter().filter_map(|c| c.source.split('/').next()).collect();

    let name = std::fs::canonicalize(dataflow_path)?;
    let mut dataflow = Dataflow::load(&name)?;
    let keel = std::env::current_exe()?;
    let mut replaced = Vec::new();
    for node in &mut dataflow.nodes {
        if node.inputs.is_empty() && recorded.contains(node.id.as_str()) {
            node.path = Some(keel.clone());
            node.build = None;
            node.rt = None;
            let args = ["replay-node".into(), recording.display().to_string(), "--speed".into(), speed.to_string()];
            node.args = args.into();
            replaced.push(node.id.clone());
        }
    }
    if replaced.is_empty() {
        return Err(io::Error::other(format!(
            "none of the dataflow's source nodes appear in the recording (it has {})",
            recorded.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    eprintln!("[replay] {} replayed from {}", replaced.join(", "), recording.display());
    let base_dir = name.parent().unwrap().to_owned();
    keel_daemon::run_here(name, dataflow, base_dir)
}

/// The node standing in for a recorded one: `keel replay-node <file>`.
pub fn replay_node(recording: &Path, speed: f64) -> io::Result<()> {
    let mut node = Node::from_env()?;
    let mut reader = Reader::open(recording)?;
    // This node's recorded outputs, by channel.
    let outputs: BTreeMap<u16, String> = (reader.header.channels.iter().enumerate())
        .filter_map(|(i, c)| {
            let (source, output) = c.source.split_once('/')?;
            (source == node.id()).then(|| (i as u16, output.to_owned()))
        })
        .collect();
    let (mut start, mut count) = (None, 0u64);
    while let Some(record) = reader.next_record()? {
        let Some(output) = outputs.get(&record.channel) else { continue };
        let (t0, now0) = *start.get_or_insert((record.t_ns, Instant::now()));
        if speed > 0.0 {
            let due = now0 + Duration::from_nanos(record.t_ns.saturating_sub(t0)).div_f64(speed);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
        node.send_output(output, &record.payload)?;
        count += 1;
    }
    println!("replayed {count} messages");
    Ok(())
}

/// `keel export <file> <dir>`: one file per message, `<channel>/<n>.bin`,
/// and `index.csv` listing them with their time and trace span. `from` and
/// `to` are seconds since the first record.
pub fn export(recording: &Path, dir: &Path, channels: &[String], from: f64, to: f64) -> io::Result<()> {
    let mut reader = Reader::open(recording)?;
    let names: Vec<String> = reader.header.channels.iter().map(|c| c.input.clone()).collect();
    for wanted in channels {
        if !names.contains(wanted) && !reader.header.channels.iter().any(|c| &c.source == wanted) {
            return Err(io::Error::other(format!("no channel `{wanted}` (has: {})", names.join(", "))));
        }
    }
    let keep = |i: usize| {
        let c = &reader.header.channels[i];
        channels.is_empty() || channels.contains(&c.input) || channels.contains(&c.source)
    };
    let keep: Vec<bool> = (0..names.len()).map(keep).collect();
    std::fs::create_dir_all(dir)?;
    let mut index = io::BufWriter::new(std::fs::File::create(dir.join("index.csv"))?);
    writeln!(index, "channel,source,n,t_s,span,bytes,file")?;
    let (mut first, mut counts, mut written) = (None, vec![0u64; names.len()], 0u64);
    while let Some(record) = reader.next_record()? {
        let ch = record.channel as usize;
        let t = (record.t_ns - *first.get_or_insert(record.t_ns)) as f64 / 1e9;
        if !keep[ch] || t < from || t > to {
            continue;
        }
        let n = counts[ch];
        counts[ch] += 1;
        let file = PathBuf::from(&names[ch]).join(format!("{n:06}.bin"));
        std::fs::create_dir_all(dir.join(&names[ch]))?;
        std::fs::write(dir.join(&file), &record.payload)?;
        let source = &reader.header.channels[ch].source;
        writeln!(
            index,
            "{},{source},{n},{t:.6},{:x},{},{}",
            names[ch],
            record.span,
            record.payload.len(),
            file.display()
        )?;
        written += 1;
    }
    index.flush()?;
    println!("exported {written} messages to {} (index.csv lists them)", dir.display());
    Ok(())
}

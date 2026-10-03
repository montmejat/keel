//! What one hop costs with keel out of the way: two threads pinned to two
//! CPUs, one publishing a timestamp into a shared cache line at a steady
//! pace, the other watching it (`--mode spin`) or sleeping on a futex there
//! (`--mode futex`, the same protocol as keel's bells). The gap between this
//! and `hop-sink` is keel's own cost.
//!
//! `hop-floor --cpus 2,4 --mode spin --count 20000 --period-us 1000`

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hop::{arg, report};
use keel::trace::now_ns;
use keel::Periodic;

/// One cache line, as a bell and a message in one.
#[repr(C, align(64))]
#[derive(Default)]
struct Line {
    seq: AtomicU32,
    waiting: AtomicU32,
    stamp: AtomicU64,
}

fn main() {
    let cpus: String = arg("cpus", String::from("2,4"));
    let cpus: Vec<usize> = cpus.split(',').map(|c| c.parse().expect("--cpus a,b")).collect();
    let mode: String = arg("mode", String::from("spin"));
    let spin = match mode.as_str() {
        "spin" => true,
        "futex" => false,
        _ => panic!("--mode spin|futex"),
    };
    let count: usize = arg("count", 20_000);
    let period = Duration::from_micros(arg("period-us", 1000));
    let line = Arc::new(Line::default());

    let receiver = {
        let line = line.clone();
        let cpu = cpus[1];
        std::thread::spawn(move || {
            pin(cpu);
            let mut hops = Vec::with_capacity(count);
            let mut late = 0;
            for seen in 0..count as u32 {
                // `seq` counts messages: wait for the one after `seen`. If
                // it's already further along, the receiver fell a period
                // behind and that message's stamp was overwritten.
                if line.seq.load(Ordering::Acquire) > seen + 1 {
                    late += 1;
                }
                while line.seq.load(Ordering::Acquire) == seen {
                    if spin {
                        std::hint::spin_loop();
                    } else {
                        line.waiting.store(1, Ordering::SeqCst);
                        if line.seq.load(Ordering::SeqCst) == seen {
                            futex(&line.seq, libc::FUTEX_WAIT, seen);
                        }
                        line.waiting.store(0, Ordering::SeqCst);
                    }
                }
                hops.push(now_ns().saturating_sub(line.stamp.load(Ordering::Relaxed)));
            }
            if late > 0 {
                println!("receiver fell behind {late} times: those hops are missing");
            }
            hops
        })
    };

    pin(cpus[0]);
    // Give the receiver time to start watching.
    std::thread::sleep(Duration::from_millis(50));
    let mut periodic = Periodic::new(period);
    for _ in 0..count {
        periodic.wait();
        line.stamp.store(now_ns(), Ordering::Relaxed);
        line.seq.fetch_add(1, Ordering::SeqCst);
        if line.waiting.load(Ordering::SeqCst) != 0 {
            futex(&line.seq, libc::FUTEX_WAKE, i32::MAX as u32);
        }
    }
    let mut hops = receiver.join().unwrap();
    report(&format!("floor, {mode}, cpus {}→{}", cpus[0], cpus[1]), &mut hops);
}

fn pin(cpu: usize) {
    // SAFETY: plain syscall on the calling thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        assert_eq!(libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set), 0, "can't pin to {cpu}");
    }
}

/// Not `FUTEX_PRIVATE_FLAG`, like keel's bells, to compare like with like.
fn futex(word: &AtomicU32, op: i32, value: u32) {
    // SAFETY: `word` outlives the call; spurious returns just mean "check again".
    unsafe {
        libc::syscall(libc::SYS_futex, word.as_ptr(), op, value, std::ptr::null::<libc::timespec>(), 0usize, 0u32)
    };
}

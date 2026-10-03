// What the site shows, measured: the one place to update after a benchmark.
// Times in seconds unless named otherwise. Sources: docs/architecture.md
// (benchmarks), docs/decisions/0027-0029, and `keel trace` on the examples.
window.KEEL_MEASUREMENTS = {
  machine:
    "a laptop (Intel Core Ultra 7 155H, powersave governor, no isolated cores, no real-time priority), " +
    "one way unless marked; the Wi-Fi figures are a Raspberry Pi sending to that laptop",

  // The latency scale: one row per kind of thing, one point per measurement.
  scale: [
    { row: "machine", v: 84e-9, what: "A cache line between two hyperthreads of one core", why: "The floor, with keel out of the way: one thread stamps a shared cache line, the other spins on it (hop-floor)." },
    { row: "machine", v: 170e-9, what: "A cache line between two separate cores", why: "The same floor across physical cores 2 and 4: the least any hop between them can cost on this laptop." },
    { row: "machine", v: 5.0e-6, what: "Waking a sleeping thread, without keel", why: "A futex wake: a system call, the scheduler, and an idle core leaving its sleep state. Most of a sleeping hop is this." },
    { row: "hop", v: 229e-9, what: "One keel hop between hyperthreads, spinning", why: "keel's hop where the floor is 84 ns: the extra is the four cache lines a hop touches." },
    { row: "hop", v: 519e-9, what: "One keel hop between two cores, the receiver spinning", why: "From publish to the receiver holding the message. Four cache lines change hands in turn: the bell, the ring, the ring entry, the region header.", selected: true },
    { row: "hop", v: 6.01e-6, what: "One keel hop, the receiver asleep", why: "The default: the receiver sleeps on a futex and the kernel wakes it. keel adds about 1 µs to the bare wake-up." },
    { row: "control", v: 1.3e-6, what: "State → command applied, both sides spinning", why: "A 1 kHz loop closed within its cycle, each node pinned to its own core. The controller keeps a whole core busy." },
    { row: "control", v: 2.97e-6, what: "State → command applied, the controller spinning around its states", why: "The controller sleeps, wakes 30 µs before each state is due and spins until 30 µs after: 6% of a core." },
    { row: "control", v: 21.3e-6, what: "State → command applied, same cycle, both asleep", why: "The joints publish, wait up to 300 µs for the answer, and apply it before the tick ends." },
    { row: "control", v: 1.0e-3, what: "State → command applied, next cycle (before)", why: "The command waited for the joints' next tick: a full period, 987 µs of it waiting." },
    { row: "control", v: 2.49e-3, what: "A command's wait for the bus master on a virtual CAN bus, p99, next cycle", why: "Before the loop closed within its cycle. With same-cycle closing, the same p99 is 9.7 µs." },
    { row: "data", v: 574e-9, what: "A 6.2 MB camera frame, one hop, spinning", why: "As fast as 64 bytes: the frame stays where the camera wrote it, only its descriptor moves." },
    { row: "data", v: 78.7e-6, what: "Reading that frame once", why: "One byte per cache line through 6.2 MB. Touching the data costs 140 times the hop that delivered it." },
    { row: "data", v: 302e-3, what: "A 6 MB frame crossing Wi-Fi", why: "The network part of a sampled trace, from a Raspberry Pi to a laptop." },
    { row: "data", v: 6.51, what: "The same frame, waiting to be sent", why: "It sat 6.5 s in the camera's queue before its daemon could send it. The trace found it; nobody had guessed it." },
  ],

  // One hop, one way, publish → in the receiver's hands (examples/hop.yml).
  hop: {
    sleep: 6.01e-6,
    sleepFloor: 5.0e-6,
    spin: 519e-9,
    spinFloor: 170e-9,
    spinFrame: 574e-9,
  },

  // One control cycle at 1 kHz (control-sim.yml), from sampled traces, in µs
  // after the state is published: [taken by the controller, command sent,
  // command taken by the joints, applied].
  cycle: {
    next: { e2e: 1000, cpu: "~0%", who: "The command waits for the next tick", ev: [9.7, 10.9, null, null] },
    same: { e2e: 21.3, cpu: "~0%", who: "The joints wait, asleep, up to 300 µs", ev: [11.1, 11.8, 20.8, 21.3] },
    simspin: { e2e: 14.6, cpu: "~0%", who: "The joints spin in their 300 µs window", ev: [11.8, 12.7, 13.9, 14.6] },
    around: { e2e: 2.97, cpu: "6%", who: "Both spin, the controller only around each state", ev: [1.1, 2.0, 2.74, 2.97] },
    both: { e2e: 1.3, cpu: "100%", who: "Both spin, each on its own core", ev: [0.43, 0.71, 1.03, 1.3] },
  },
};

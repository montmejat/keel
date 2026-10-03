// The timing page's figures: the latency scale, one hop, one control cycle,
// and phases. Every number comes from measurements.js.
(() => {
  const M = window.KEEL_MEASUREMENTS;
  const NS = "http://www.w3.org/2000/svg";
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const el = (tag, attrs = {}, parent) => {
    const n = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, v);
    if (parent) parent.appendChild(n);
    return n;
  };
  const txt = (parent, x, y, s, attrs = {}) => {
    const t = el("text", { x, y, ...attrs }, parent);
    t.textContent = s;
    return t;
  };
  const fmt = (s) => {
    if (s >= 1) return (s >= 10 ? s.toFixed(0) : s.toFixed(1)) + " s";
    if (s >= 1e-3) return (s * 1e3 >= 10 ? (s * 1e3).toFixed(0) : String(+(s * 1e3).toFixed(2))) + " ms";
    if (s >= 1e-6) return (s * 1e6 >= 10 ? (s * 1e6).toFixed(0) : (s * 1e6).toFixed(1)) + " µs";
    return (s * 1e9).toFixed(0) + " ns";
  };
  const pressGroup = (selector, attr, value) => {
    document.querySelectorAll(selector).forEach((b) => b.setAttribute("aria-pressed", String(b.dataset[attr] === value)));
  };
  const byId = (id) => document.getElementById(id);
  const machine = byId("machine");
  if (machine) machine.textContent = M.machine;

  /* ---------- the latency scale ---------- */
  const scale = byId("scaleSvg");
  const LEFT = 150, RIGHT = 970, LO = -8, HI = 1;
  const sx = (v) => LEFT + ((Math.log10(v) - LO) / (HI - LO)) * (RIGHT - LEFT);
  const rows = [
    { key: "machine", label: "the machine", y: 92 },
    { key: "hop", label: "a keel hop", y: 146 },
    { key: "control", label: "a control loop", y: 200 },
    { key: "data", label: "data in motion", y: 254 },
  ];
  const points = M.scale.map((p) => ({ ...p }));
  // Labels alternate above and below along each row, so neighbours don't collide.
  for (const r of rows) {
    points.filter((p) => p.row === r.key).sort((a, b) => a.v - b.v).forEach((p, i) => (p.side = i % 2 ? 1 : -1));
  }
  const decades = [[1e-8, "10 ns"], [1e-7, "100 ns"], [1e-6, "1 µs"], [1e-5, "10 µs"], [1e-4, "100 µs"], [1e-3, "1 ms"], [1e-2, "10 ms"], [1e-1, "100 ms"], [1, "1 s"], [10, "10 s"]];
  for (const [v, label] of decades) {
    const x = sx(v);
    el("line", { x1: x, x2: x, y1: 40, y2: 276, class: "grid" }, scale);
    txt(scale, x, 30, label, { "text-anchor": "middle", "font-size": 11, class: "muted" });
  }
  el("line", { x1: LEFT, x2: RIGHT, y1: 40, y2: 40, class: "rule" }, scale);
  for (const r of rows) {
    el("line", { x1: LEFT, x2: RIGHT, y1: r.y, y2: r.y, class: "grid", "stroke-dasharray": "2 4" }, scale);
    txt(scale, 0, r.y + 4, r.label, { "font-size": 12, class: "muted" });
  }
  const cursor = el("line", { x1: LEFT, x2: LEFT, y1: 40, y2: 276, stroke: "var(--accent)", "stroke-width": 1.5, opacity: 0 }, scale);
  const dots = points.map((p, i) => {
    const row = rows.find((r) => r.key === p.row);
    const g = el("g", { class: "dot", tabindex: 0, role: "button", "aria-label": `${fmt(p.v)}: ${p.what}` }, scale);
    const x = sx(p.v);
    el("circle", { class: "ping", cx: x, cy: row.y, r: 5 }, g);
    el("circle", { class: "core", cx: x, cy: row.y, r: 5 }, g);
    txt(g, x, row.y + (p.side < 0 ? -12 : 21), fmt(p.v), { "text-anchor": "middle" });
    g.addEventListener("click", () => select(i));
    g.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") { e.preventDefault(); select(i); }
    });
    return { g, x };
  });
  function select(i) {
    dots.forEach((d, j) => d.g.classList.toggle("sel", i === j));
    byId("dVal").textContent = fmt(points[i].v);
    byId("dWhat").textContent = points[i].what;
    byId("dWhy").textContent = points[i].why;
  }
  select(Math.max(0, points.findIndex((p) => p.selected)));

  let sweepId = 0;
  function sweep() {
    if (reduced) return;
    const id = ++sweepId, t0 = performance.now(), dur = 2600, fired = new Set();
    cursor.setAttribute("opacity", 0.8);
    const frame = (t) => {
      if (id !== sweepId) return;
      const k = Math.min(1, (t - t0) / dur);
      const x = LEFT + k * (RIGHT - LEFT);
      cursor.setAttribute("x1", x);
      cursor.setAttribute("x2", x);
      dots.forEach((d, j) => {
        if (!fired.has(j) && d.x <= x) {
          fired.add(j);
          d.g.classList.remove("pinging");
          void d.g.getBBox();
          d.g.classList.add("pinging");
        }
      });
      if (k < 1) requestAnimationFrame(frame);
      else cursor.setAttribute("opacity", 0);
    };
    requestAnimationFrame(frame);
  }
  byId("sweep").addEventListener("click", sweep);

  /* ---------- one hop ---------- */
  const hop = byId("hopSvg");
  const box = (x, y, w, h, title, sub, dashed = false) => {
    const g = el("g", {}, hop);
    el("rect", { x, y, width: w, height: h, rx: 5, class: "box", ...(dashed ? { "stroke-dasharray": "5 4" } : {}) }, g);
    if (title) txt(g, x + 12, y + 22, title, { "font-size": 13, "font-weight": 600 });
    if (sub) txt(g, x + 12, y + 40, sub, { "font-size": 11, class: "muted" });
    return g;
  };
  box(20, 70, 200, 110, "camera", "sender · CPU 2");
  box(780, 70, 200, 110, "detector", "receiver · CPU 4");
  box(260, 30, 480, 190, "", "", true);
  txt(hop, 272, 50, "/dev/shm", { "font-size": 11, class: "muted" });
  el("rect", { x: 280, y: 64, width: 230, height: 22, class: "box" }, hop);
  txt(hop, 290, 79, "header: refcount · span · stamp", { "font-size": 10, class: "muted" });
  el("rect", { x: 280, y: 86, width: 230, height: 116, class: "box" }, hop);
  const payloadFill = el("rect", { x: 281, y: 87, width: 0, height: 114, fill: "var(--accent-soft)" }, hop);
  const payloadLabel = txt(hop, 395, 150, "payload · 64 B", { "text-anchor": "middle", "font-size": 12 });
  txt(hop, 545, 58, "ring", { "font-size": 11, class: "muted" });
  const slots = [];
  for (let i = 0; i < 6; i++) slots.push(el("rect", { x: 545 + i * 28, y: 64, width: 24, height: 24, rx: 2, class: "box" }, hop));
  el("circle", { cx: 630, cy: 160, r: 22, class: "box" }, hop);
  txt(hop, 630, 164, "bell", { "text-anchor": "middle", "font-size": 11 });
  const kernel = box(260, 252, 480, 52, "kernel", "futex · scheduler");
  const kernelRect = kernel.querySelector("rect");
  const watch = el("line", { x1: 780, y1: 150, x2: 652, y2: 160, stroke: "var(--accent)", "stroke-width": 1.5, "stroke-dasharray": "3 4", opacity: 0 }, hop);
  const watchLabel = txt(hop, 716, 196, "watching", { "text-anchor": "middle", "font-size": 10, fill: "var(--accent)", opacity: 0 });
  const token = el("circle", { cx: 220, cy: 125, r: 7, fill: "var(--accent)" }, hop);
  const receiverGlow = el("rect", { x: 780, y: 70, width: 200, height: 110, rx: 5, fill: "none", stroke: "var(--accent)", "stroke-width": 2.5, opacity: 0 }, hop);

  const hopState = { wait: "sleep", size: "small" };
  const P = { sender: [220, 125], payload: [395, 120], slot: [557, 76], bell: [630, 160], kernel: [500, 278], recvBottom: [880, 180], recv: [780, 125] };
  function hopSteps() {
    const steps = [
      { from: P.sender, to: P.payload, ms: 900, label: "Write the payload in place", counts: false, fill: true },
      { from: P.payload, to: P.slot, ms: 700, label: "Stamp it, put its descriptor in the ring", counts: true, entry: true },
      { from: P.slot, to: P.bell, ms: 600, label: "Bump the bell", counts: true },
    ];
    if (hopState.wait === "sleep") {
      steps.push({ from: P.bell, to: P.kernel, ms: 900, label: "FUTEX_WAKE: a system call", counts: true, kernel: true });
      steps.push({ from: P.kernel, to: P.recvBottom, ms: 1300, label: "The scheduler wakes the receiver", counts: true, kernel: true });
      steps.push({ from: P.recvBottom, to: P.recv, ms: 600, label: "It reads the bell, ring and header", counts: true, lines: ["bell", "ring", "entry", "header"] });
    } else {
      steps.push({ from: P.bell, to: P.recv, ms: 900, label: "The spinning receiver sees the bell move", counts: true, lines: ["bell"] });
      steps.push({ from: P.recv, to: P.recv, ms: 700, label: "It reads the ring, entry and header", counts: true, lines: ["ring", "entry", "header"] });
    }
    return steps;
  }
  const clockEl = byId("hopClock"), stepEl = byId("hopStep"), slowEl = byId("hopSlow");
  const chips = [...document.querySelectorAll(".chip[data-line]")];
  const hotLines = (names) => chips.forEach((c) => c.classList.toggle("hot", names.includes(c.dataset.line)));
  const hopTotal = () => (hopState.wait === "sleep" ? M.hop.sleep : hopState.size === "frame" ? M.hop.spinFrame : M.hop.spin);

  function hopReset() {
    hotLines([]);
    payloadFill.setAttribute("width", 0);
    kernel.style.opacity = hopState.wait === "sleep" ? 1 : 0.35;
    kernelRect.setAttribute("stroke", "var(--rule)");
    watch.setAttribute("opacity", hopState.wait === "spin" ? 0.9 : 0);
    watchLabel.setAttribute("opacity", hopState.wait === "spin" ? 1 : 0);
    receiverGlow.setAttribute("opacity", 0);
    slots.forEach((r) => r.setAttribute("fill", "var(--paper)"));
    payloadLabel.textContent = hopState.size === "frame" ? "payload · 6.2 MB frame" : "payload · 64 B";
  }
  function hopDelivered() {
    token.setAttribute("cx", P.recv[0]);
    token.setAttribute("cy", P.recv[1]);
    payloadFill.setAttribute("width", 228);
    slots[0].setAttribute("fill", "var(--accent-soft)");
    hotLines(["bell", "ring", "entry", "header"]);
    receiverGlow.setAttribute("opacity", 1);
    kernelRect.setAttribute("stroke", "var(--rule)");
    clockEl.textContent = fmt(hopTotal());
    stepEl.textContent = "In the receiver's hands";
  }
  let hopRun = 0;
  function playHop() {
    hopReset();
    const id = ++hopRun;
    const steps = hopSteps();
    const total = hopTotal();
    const countMs = steps.filter((s) => s.counts).reduce((a, s) => a + s.ms, 0);
    slowEl.textContent = Math.round(countMs / 1000 / total).toLocaleString("en-US") + "×";
    renderBars();
    if (reduced) return hopDelivered();
    let i = 0, t0 = performance.now(), counted = 0;
    const lit = new Set();
    clockEl.textContent = "0 ns";
    const frame = (t) => {
      if (id !== hopRun) return;
      const s = steps[i];
      const k = Math.min(1, (t - t0) / s.ms);
      const e = k < 0.5 ? 2 * k * k : 1 - Math.pow(-2 * k + 2, 2) / 2;
      token.setAttribute("cx", s.from[0] + (s.to[0] - s.from[0]) * e);
      token.setAttribute("cy", s.from[1] + (s.to[1] - s.from[1]) * e);
      stepEl.textContent = s.label;
      if (s.fill) payloadFill.setAttribute("width", 228 * k);
      if (s.entry && k > 0.6) slots[0].setAttribute("fill", "var(--accent-soft)");
      if (s.kernel) kernelRect.setAttribute("stroke", "var(--wait)");
      if (s.lines && k > 0.2) {
        s.lines.forEach((n) => lit.add(n));
        hotLines([...lit]);
      }
      if (s.counts) clockEl.textContent = fmt(((counted + k * s.ms) / countMs) * total);
      if (k >= 1) {
        if (s.counts) counted += s.ms;
        i++;
        t0 = t;
        if (i >= steps.length) return hopDelivered();
      }
      requestAnimationFrame(frame);
    };
    requestAnimationFrame(frame);
  }
  function renderBars() {
    const bars = [
      { k: "sleep", lbl: "keel, receiver asleep", v: M.hop.sleep },
      { k: "sleep-floor", lbl: "no keel, asleep (futex)", v: M.hop.sleepFloor },
      { k: "spin", lbl: "keel, receiver spinning", v: M.hop.spin },
      { k: "frame", lbl: "keel, spinning, 6.2 MB frame", v: M.hop.spinFrame },
      { k: "spin-floor", lbl: "no keel, spinning", v: M.hop.spinFloor },
    ];
    const on = hopState.wait === "sleep" ? ["sleep", "sleep-floor"] : hopState.size === "frame" ? ["frame", "spin-floor"] : ["spin", "spin-floor"];
    const max = Math.max(...bars.map((b) => b.v)) * 1.08;
    byId("hopBars").innerHTML = bars
      .map((b) => `<div class="bar${on.includes(b.k) ? " on" : ""}"><span class="lbl">${b.lbl}</span><span class="track"><span class="fill" style="width:${((b.v / max) * 100).toFixed(2)}%"></span></span><span class="num">${fmt(b.v)}</span></div>`)
      .join("");
  }
  document.querySelectorAll("[data-wait]").forEach((b) =>
    b.addEventListener("click", () => {
      hopState.wait = b.dataset.wait;
      pressGroup("[data-wait]", "wait", hopState.wait);
      playHop();
    })
  );
  document.querySelectorAll("[data-size]").forEach((b) =>
    b.addEventListener("click", () => {
      hopState.size = b.dataset.size;
      pressGroup("[data-size]", "size", hopState.size);
      playHop();
    })
  );
  byId("hopPlay").addEventListener("click", playHop);

  /* ---------- one control cycle ---------- */
  const cy = byId("cycleSvg");
  const OV = { x0: 130, x1: 980, t0: 0, t1: 2000 };
  const ZM = { x0: 130, x1: 980, t0: -5, t1: 25 };
  const ox = (t) => OV.x0 + ((t - OV.t0) / (OV.t1 - OV.t0)) * (OV.x1 - OV.x0);
  const zx = (t) => ZM.x0 + ((t - ZM.t0) / (ZM.t1 - ZM.t0)) * (ZM.x1 - ZM.x0);
  const LANE = { ovJ: 52, ovC: 92, zJ: 196, zC: 268 };
  let mode = "same";
  const staticLayer = el("g", {}, cy), modeLayer = el("g", {}, cy);
  const clip = el("clipPath", { id: "zoomReveal" }, el("defs", {}, cy));
  const clipRect = el("rect", { x: 0, y: 150, width: 1000, height: 170 }, clip);
  txt(staticLayer, 0, 20, "two cycles · 0 → 2 ms", { "font-size": 11, class: "muted" });
  for (const [t, l] of [[0, "tick k"], [1000, "tick k+1"], [2000, "k+2"]]) {
    el("line", { x1: ox(t), x2: ox(t), y1: 30, y2: 108, class: "rule" }, staticLayer);
    txt(staticLayer, ox(t) + (t === 2000 ? -4 : 4), 122, l, { "font-size": 10, class: "muted", "text-anchor": t === 2000 ? "end" : "start" });
  }
  for (const [y, l] of [[LANE.ovJ, "joints"], [LANE.ovC, "controller"]]) {
    txt(staticLayer, 0, y + 4, l, { "font-size": 12 });
    el("line", { x1: OV.x0, x2: OV.x1, y1: y, y2: y, class: "grid" }, staticLayer);
  }
  txt(staticLayer, 0, 150, "the start of tick k, zoomed · −5 → 25 µs", { "font-size": 11, class: "muted" });
  for (let t = -5; t <= 25; t += 5) {
    el("line", { x1: zx(t), x2: zx(t), y1: 166, y2: 296, class: "grid" }, staticLayer);
    txt(staticLayer, zx(t), 314, (t > 0 ? "+" : "") + t + " µs", { "font-size": 10, class: "muted", "text-anchor": "middle" });
  }
  el("line", { x1: zx(0), x2: zx(0), y1: 166, y2: 296, stroke: "var(--ink)", "stroke-width": 1.2 }, staticLayer);
  for (const [y, l] of [[LANE.zJ, "joints"], [LANE.zC, "controller"]]) {
    txt(staticLayer, 0, y + 4, l, { "font-size": 12 });
    el("line", { x1: ZM.x0, x2: ZM.x1, y1: y, y2: y, class: "grid" }, staticLayer);
  }
  const playhead = el("line", { x1: zx(-5), x2: zx(-5), y1: 166, y2: 296, stroke: "var(--accent)", "stroke-width": 1.5, opacity: 0 }, cy);

  function arrow(parent, x1, y1, x2, y2, color) {
    el("line", { x1, y1, x2, y2, stroke: color, "stroke-width": 2 }, parent);
    const a = Math.atan2(y2 - y1, x2 - x1), L = 7;
    el("path", { d: `M${x2},${y2} L${x2 - L * Math.cos(a - 0.45)},${y2 - L * Math.sin(a - 0.45)} L${x2 - L * Math.cos(a + 0.45)},${y2 - L * Math.sin(a + 0.45)} Z`, fill: color }, parent);
  }
  function drawMode() {
    modeLayer.innerHTML = "";
    const m = M.cycle[mode];
    const ov = el("g", {}, modeLayer), zm = el("g", { "clip-path": "url(#zoomReveal)" }, modeLayer);
    for (const t of [0, 1000]) el("circle", { cx: ox(t), cy: LANE.ovJ, r: 4, fill: "var(--accent)" }, ov);
    if (mode === "next") {
      for (const t of [0, 1000]) {
        el("rect", { x: ox(t + 11), y: LANE.ovJ - 7, width: ox(t + 1000) - ox(t + 11), height: 14, fill: "var(--wait-soft)", stroke: "var(--wait)" }, ov);
      }
      txt(ov, ox(500), LANE.ovJ + 4, "command waits 987 µs for the next tick", { "text-anchor": "middle", "font-size": 11, fill: "var(--wait)" });
    } else {
      for (const t of [0, 1000]) {
        el("rect", { x: ox(t), y: LANE.ovJ - 7, width: ox(t + 300) - ox(t), height: 14, fill: "var(--accent-soft)", stroke: "var(--accent)", "stroke-dasharray": "3 3" }, ov);
        el("line", { x1: ox(t + m.e2e), x2: ox(t + m.e2e), y1: LANE.ovJ - 9, y2: LANE.ovJ + 9, stroke: "var(--accent)", "stroke-width": 2 }, ov);
      }
      txt(ov, ox(300) + 8, LANE.ovJ + 4, "300 µs deadline: applied after " + fmt(m.e2e * 1e-6), { "font-size": 11, fill: "var(--accent)" });
    }
    if (mode === "around") {
      for (const t of [0, 1000, 2000]) {
        const a = Math.max(OV.t0, t - 30), b = Math.min(OV.t1, t + 30);
        el("rect", { x: ox(a), y: LANE.ovC - 7, width: Math.max(2, ox(b) - ox(a)), height: 14, fill: "var(--accent)" }, ov);
      }
      txt(ov, ox(500), LANE.ovC + 4, "spins ±30 µs around each state, asleep otherwise", { "text-anchor": "middle", "font-size": 11, class: "muted" });
    } else if (mode === "both") {
      el("rect", { x: OV.x0, y: LANE.ovC - 7, width: OV.x1 - OV.x0, height: 14, fill: "var(--accent-soft)", stroke: "var(--accent)" }, ov);
      txt(ov, ox(500), LANE.ovC + 4, "spins the whole time", { "text-anchor": "middle", "font-size": 11, fill: "var(--accent)" });
    } else {
      txt(ov, ox(500), LANE.ovC + 4, "asleep between states", { "text-anchor": "middle", "font-size": 11, class: "muted" });
    }
    const [recv, sent, arrive, applied] = m.ev;
    if (mode === "around" || mode === "both") el("rect", { x: zx(-5), y: LANE.zC - 9, width: zx(25) - zx(-5), height: 18, fill: "var(--accent-soft)" }, zm);
    if (mode === "same" || mode === "simspin") {
      el("rect", { x: zx(0), y: LANE.zJ - 9, width: zx(25) - zx(0), height: 18, fill: mode === "simspin" ? "var(--accent-soft)" : "none", stroke: "var(--accent)", "stroke-dasharray": "3 3" }, zm);
    }
    el("circle", { cx: zx(0), cy: LANE.zJ, r: 5, fill: "var(--accent)" }, zm);
    txt(zm, zx(0) + 8, LANE.zJ - 14, "state(k) published", { "font-size": 10 });
    arrow(zm, zx(0), LANE.zJ + 6, zx(recv), LANE.zC - 8, "var(--accent)");
    el("rect", { x: zx(recv), y: LANE.zC - 5, width: Math.max(3, zx(sent) - zx(recv)), height: 10, fill: "var(--ink)" }, zm);
    txt(zm, zx(recv), LANE.zC + 24, "taken " + fmt(recv * 1e-6), { "font-size": 10, class: "muted", "text-anchor": recv < 3 ? "start" : "middle" });
    if (mode === "next") {
      arrow(zm, zx(sent), LANE.zC - 8, zx(25) - 4, LANE.zJ + 30, "var(--wait)");
      txt(zm, zx(24.6), LANE.zJ + 22, "cmd(k) → waits for tick k+1", { "font-size": 10, fill: "var(--wait)", "text-anchor": "end" });
    } else {
      arrow(zm, zx(sent), LANE.zC - 8, zx(arrive), LANE.zJ + 8, "var(--accent)");
      el("rect", { x: zx(arrive), y: LANE.zJ - 5, width: Math.max(3, zx(applied) - zx(arrive)), height: 10, fill: "var(--ink)" }, zm);
      txt(zm, zx(applied) + 6, LANE.zJ + 18, "cmd(k) applied · " + fmt(applied * 1e-6), { "font-size": 10, "font-weight": 600 });
    }
    const e2e = byId("cyE2e");
    e2e.textContent = fmt(m.e2e * 1e-6);
    e2e.className = "v " + (mode === "next" ? "wait" : "accent");
    byId("cyCpu").textContent = m.cpu;
    byId("cyWho").textContent = m.who;
  }
  let cyRun = 0;
  function playCycle() {
    drawMode();
    const id = ++cyRun;
    if (reduced) {
      clipRect.setAttribute("width", 1000);
      return;
    }
    const t0 = performance.now(), dur = 2400;
    playhead.setAttribute("opacity", 0.9);
    const frame = (t) => {
      if (id !== cyRun) return;
      const k = Math.min(1, (t - t0) / dur);
      const x = ZM.x0 + k * (ZM.x1 - ZM.x0);
      clipRect.setAttribute("width", x);
      playhead.setAttribute("x1", x);
      playhead.setAttribute("x2", x);
      if (k < 1) requestAnimationFrame(frame);
      else {
        clipRect.setAttribute("width", 1000);
        playhead.setAttribute("opacity", 0);
      }
    };
    requestAnimationFrame(frame);
  }
  document.querySelectorAll("[data-mode]").forEach((b) =>
    b.addEventListener("click", () => {
      mode = b.dataset.mode;
      pressGroup("[data-mode]", "mode", mode);
      playCycle();
    })
  );
  byId("cyclePlay").addEventListener("click", playCycle);

  /* ---------- phases ---------- */
  const ph = byId("phaseSvg");
  const PX = { x0: 190, x1: 980, t0: 0, t1: 5000 };
  const px = (t) => PX.x0 + ((t - PX.t0) / (PX.t1 - PX.t0)) * (PX.x1 - PX.x0);
  const pxPerUs = (PX.x1 - PX.x0) / (PX.t1 - PX.t0);
  for (let t = 0; t <= 5000; t += 1000) {
    el("line", { x1: px(t), x2: px(t), y1: 20, y2: 160, class: "grid" }, ph);
    txt(ph, px(t), 178, t / 1000 + " ms", { "font-size": 10, class: "muted", "text-anchor": "middle" });
  }
  const loops = [
    { name: "left arm · bus master", y: 46, grid: 0 },
    { name: "right arm · bus master", y: 90, grid: 0 },
    { name: "drive · phase_us 900", y: 134, grid: 900 },
  ];
  const phaseClip = el("clipPath", { id: "phaseClip" }, el("defs", {}, ph));
  el("rect", { x: PX.x0 - 4, y: 0, width: PX.x1 - PX.x0 + 8, height: 190 }, phaseClip);
  loops.forEach((l) => {
    txt(ph, 0, l.y + 4, l.name, { "font-size": 11 });
    el("line", { x1: PX.x0, x2: PX.x1, y1: l.y, y2: l.y, class: "grid", "stroke-dasharray": "2 4" }, ph);
    const outer = el("g", { "clip-path": "url(#phaseClip)" }, ph);
    l.g = el("g", {}, outer);
    for (let k = -1; k < 6; k++) el("rect", { x: px(k * 1000) - 1.5, y: l.y - 11, width: 3, height: 22, fill: l.grid ? "var(--wait)" : "var(--accent)" }, l.g);
    l.at = l.grid;
  });
  const gapLabel = txt(ph, px(950), 18, "100 µs", { "font-size": 10, "text-anchor": "middle", fill: "var(--wait)" });
  const place = () => loops.forEach((l) => l.g.setAttribute("transform", `translate(${(l.at * pxPerUs).toFixed(2)},0)`));
  let starts = [370, 810, 120];
  const notes = {
    grid: byId("phaseNote").innerHTML,
    start: "Before milestone 20, each loop ticked one period after it started, so the phases were whatever start-up order gave: the arms read their joints at different moments, and a state could be read just before or just after the drive reported it.",
  };
  function setPhase(next) {
    pressGroup("[data-phase]", "phase", next);
    const targets = loops.map((l, i) => (next === "grid" ? l.grid : starts[i]));
    gapLabel.setAttribute("opacity", next === "grid" ? 1 : 0);
    byId("phaseNote").innerHTML = notes[next];
    if (reduced) {
      loops.forEach((l, i) => (l.at = targets[i]));
      return place();
    }
    const from = loops.map((l) => l.at), t0 = performance.now(), dur = 800;
    const frame = (t) => {
      const k = Math.min(1, (t - t0) / dur), e = 1 - Math.pow(1 - k, 3);
      loops.forEach((l, i) => (l.at = from[i] + (targets[i] - from[i]) * e));
      place();
      if (k < 1) requestAnimationFrame(frame);
    };
    requestAnimationFrame(frame);
  }
  place();
  document.querySelectorAll("[data-phase]").forEach((b) => b.addEventListener("click", () => setPhase(b.dataset.phase)));
  byId("restartLoops").addEventListener("click", () => {
    starts = loops.map(() => Math.round(Math.random() * 999));
    setPhase("start");
  });

  /* ---------- first frame: everything at rest, readable ---------- */
  renderBars();
  drawMode();
  hopReset();
  hopDelivered();
  slowEl.textContent = "press Replay";
  if (!reduced) setTimeout(sweep, 400);
})();

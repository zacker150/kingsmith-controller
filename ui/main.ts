// The UI renders the session's snapshot and sends intents. It holds no
// protocol knowledge: every number arrives in display units, every enabled
// state comes from `controls`, and every action is an intent.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// --- snapshot types (mirror walkingpad::session, camelCase) ---------------

type LinkPhase = "idle" | "searching" | "reconnecting" | "connecting" | "handshake" | "ready";
type BeltView =
  | "idle" | "running" | "paused" | "stopped" | "fault"
  | "standby" | "countdown" | "lockOff" | "other";
type ModeView = "auto" | "manual" | "standby" | "newer" | "unknown";
type Mode = "auto" | "manual" | "standby";
type DisplayUnits = "miles" | "km";

interface KnownPad { id: string; mac: string | null; name: string | null }
interface Advertisement { id: string; name: string | null; mac: string | null; rssi: number | null }
interface Stats {
  belt: BeltView;
  mode: ModeView;
  speed: number;
  targetSpeed: number | null;
  timeS: number;
  distance: number;
  steps: number;
  countdownS: number | null;
  fault: string | null;
}
interface Snapshot {
  link: LinkPhase;
  hasLink: boolean;
  stale: boolean;
  linkLost: boolean;
  scanning: boolean;
  pad: KnownPad | null;
  remembered: KnownPad | null;
  discovered: Advertisement[];
  stats: Stats | null;
  units: { display: DisplayUnits; speedUnit: string; distanceUnit: string; minSpeed: number; maxSpeed: number };
  controls: { stop: boolean; start: boolean; showStart: boolean; speed: boolean; manual: boolean; auto: boolean; standby: boolean; disconnect: boolean };
  stopping: boolean;
  slowing: boolean;
  starting: boolean;
  notice: { level: "info" | "warning" | "error"; text: string } | null;
  problem: { kind: "permissionDenied" | "adapterOff" | "other"; text: string } | null;
  presets: number[];
  badFrames: number;
}

type Intent =
  | { type: "connect"; id: string }
  | { type: "disconnect" }
  | { type: "search" }
  | { type: "forget" }
  | { type: "start" }
  | { type: "stop" }
  | { type: "setMode"; mode: Mode }
  | { type: "adjustSpeed"; steps: number }
  | { type: "setSpeed"; tenths: number }
  | { type: "applyPreset"; index: number }
  | { type: "setPreset"; index: number; tenths: number }
  | { type: "setDisplayUnits"; units: DisplayUnits }
  | { type: "setVisible"; visible: boolean };

// --- plumbing --------------------------------------------------------------

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
let snap: Snapshot | null = null;
let selected: string | null = null;

function send(intent: Intent): void {
  invoke("intent", { intent }).catch((e) => console.error("intent failed", intent, e));
}

const stop = () => send({ type: "stop" });

// The Start/Stop button flips to Start once the belt has stopped. Taps on Start
// in the first second after that are ignored, so a double tap on Stop can't
// start the belt again.
const START_GUARD_MS = 1000;
let showingStart = false;
let startShownAt = 0;

// --- rendering -------------------------------------------------------------

function show(el: HTMLElement, visible: boolean): void {
  el.hidden = !visible;
}

function fmtTime(s: number): string {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

function padLabel(p: KnownPad | Advertisement | null): string {
  if (!p) return "the pad";
  return p.name ?? p.mac ?? p.id;
}

function linkText(s: Snapshot): [string, string] {
  switch (s.link) {
    case "ready":
      return [s.stale ? "lost" : "ready", `${padLabel(s.pad)} · ${s.stale ? "link stale" : "connected"}`];
    case "handshake":
    case "connecting":
      return ["busy", `Connecting to ${padLabel(s.pad)}…`];
    case "reconnecting":
      return ["lost", "Connection lost · reconnecting…"];
    case "searching":
      return [s.linkLost ? "lost" : "busy", s.linkLost ? "Connection lost · searching" : "Searching…"];
    case "idle":
      return ["", "Disconnected"];
  }
}

const BELT_LABEL: Record<BeltView, string> = {
  idle: "Idle",
  running: "Running",
  paused: "Paused",
  stopped: "Stopped",
  fault: "Fault",
  standby: "Standby",
  countdown: "Starting",
  lockOff: "Lock off",
  other: "Unknown state",
};
const MODE_LABEL: Record<ModeView, string> = {
  auto: "Auto",
  manual: "Manual",
  standby: "Standby mode",
  newer: "Mode 3",
  unknown: "Unknown mode",
};

function renderBanner(s: Snapshot): void {
  const el = $("banner");
  let text = "";
  let level = "warning";
  if (s.notice) {
    text = s.notice.text;
    level = s.notice.level;
  } else if (s.link === "reconnecting" || (s.linkLost && !s.hasLink)) {
    text = "Connection lost. Touch M on the pad to reconnect.";
  } else if (s.stale) {
    text = "No status from the pad (link stale). Stop still works.";
  }
  el.textContent = text;
  el.className = `banner ${level}`;
  show(el, text !== "");
}

function renderConnect(s: Snapshot): void {
  const firstRun = !s.remembered;
  $("connect-title").textContent =
    s.link === "connecting" || s.link === "handshake" ? `Connecting to ${padLabel(s.pad)}…` : "Connect to your WalkingPad";
  show($("safety"), firstRun);

  const looking = $("looking");
  if (s.remembered && s.link !== "idle") {
    looking.textContent = `Looking for ${padLabel(s.remembered)}${s.remembered.mac ? ` (${s.remembered.mac})` : ""}. It connects as soon as it appears.`;
  }
  show(looking, !!s.remembered && s.link !== "idle");

  // Problems (permission, adapter off).
  const problem = s.problem;
  show($("problem"), !!problem);
  if (problem) {
    $("problem-text").textContent = problem.text;
    show($("grant"), problem.kind !== "adapterOff");
    show($("open-settings"), problem.kind === "permissionDenied" && !isTouch);
  }

  const searching = s.scanning && (s.link === "searching" || s.link === "reconnecting");
  show($("searching"), searching);
  $("searching-text").textContent = s.link === "reconnecting" ? "Reconnecting…" : "Searching…";

  // Device list: only needed when no pad is remembered (or to pick another).
  const list = $("devices");
  const pickable = s.link === "searching" || s.link === "reconnecting";
  const devices = pickable && !s.remembered ? s.discovered : [];
  if (!devices.some((d) => d.id === selected)) {
    selected = (devices.find((d) => d.name === "R1 Pro") ?? devices[0])?.id ?? null;
  }
  list.replaceChildren(
    ...devices.map((d) => {
      const li = document.createElement("li");
      const label = document.createElement("label");
      const radio = document.createElement("input");
      radio.type = "radio";
      radio.name = "device";
      radio.checked = d.id === selected;
      radio.addEventListener("change", () => (selected = d.id));
      const text = document.createElement("div");
      text.textContent = d.name ?? "Unnamed device";
      const sub = document.createElement("small");
      sub.textContent = [d.mac, d.rssi != null ? `${d.rssi} dBm` : null].filter(Boolean).join(" · ");
      text.append(sub);
      label.append(radio, text);
      li.append(label);
      return li;
    }),
  );
  show($("connect-btn"), devices.length > 0);
  show($("search-btn"), s.link === "idle");
  show($("forget-btn"), !!s.remembered && (s.link === "searching" || s.link === "idle"));
}

function renderDash(s: Snapshot): void {
  const st = s.stats;
  const c = s.controls;
  $("readout").classList.toggle("stale", s.stale);
  $("speed").textContent = st ? st.speed.toFixed(1) : "–";
  $("speed-unit").textContent = s.units.speedUnit;
  const target = $("target");
  show(target, !!st?.targetSpeed);
  if (st?.targetSpeed) target.textContent = `→ ${st.targetSpeed.toFixed(1)} ${s.units.speedUnit}`;

  const belt = $("belt");
  let label = st ? `${BELT_LABEL[st.belt]} · ${MODE_LABEL[st.mode]}` : "Waiting for the pad…";
  belt.className = "belt";
  if (st?.belt === "countdown") {
    label = st.countdownS ? `Starting in ${st.countdownS}` : "Starting…";
    belt.className = "belt countdown";
  } else if (s.slowing) {
    label = "Slowing down…";
  } else if (st?.belt === "running") {
    belt.className = "belt moving";
  }
  belt.textContent = label;
  const fault = $("fault");
  show(fault, !!st?.fault);
  if (st?.fault) fault.textContent = `Fault ${st.fault}`;

  $("time").textContent = st ? fmtTime(st.timeS) : "–";
  $("distance").textContent = st ? st.distance.toFixed(2) : "–";
  $("distance-unit").textContent = s.units.distanceUnit;
  $("steps").textContent = st ? st.steps.toLocaleString() : "–";

  document.querySelectorAll<HTMLButtonElement>(".speed-buttons button").forEach((b) => (b.disabled = !c.speed));
  document.querySelectorAll<HTMLButtonElement>(".preset").forEach((b) => {
    const i = Number(b.dataset.preset);
    b.textContent = s.presets[i].toFixed(1);
    b.disabled = !c.speed;
  });
  document.querySelectorAll<HTMLButtonElement>(".modes button").forEach((b) => {
    const mode = b.dataset.mode as Mode;
    b.disabled = !c[mode];
    b.classList.toggle("active", st?.mode === mode);
  });
}

function render(s: Snapshot): void {
  snap = s;
  const [dot, text] = linkText(s);
  $("dot").className = `dot ${dot}`;
  $("link-text").textContent = text;
  $("units").textContent = s.units.speedUnit;
  const disconnect = $("disconnect");
  show(disconnect, s.controls.disconnect);
  // While only searching, this stops auto-connecting (to hand the pad over).
  disconnect.textContent = s.link === "searching" || s.link === "reconnecting" ? "Stop searching" : "Disconnect";
  renderBanner(s);

  const ready = s.link === "ready";
  show($("connect"), !ready);
  show($("dash"), ready);
  if (ready) renderDash(s);
  else renderConnect(s);

  // Start/Stop: one button in the footer. It's Stop whenever the belt might
  // be moving, and Stop is enabled whenever a link exists.
  const asStart = s.controls.showStart;
  if (asStart && !showingStart) startShownAt = performance.now();
  showingStart = asStart;
  const toggle = $<HTMLButtonElement>("toggle");
  toggle.className = `toggle ${asStart ? "start" : "stop"}`;
  toggle.disabled = asStart ? !s.controls.start : !s.controls.stop;
  $("toggle-label").textContent = asStart ? "START" : s.stopping ? "STOPPING…" : "STOP";
  show($("toggle-keys"), !asStart);
}

// --- wiring ----------------------------------------------------------------

const isTouch = matchMedia("(pointer: coarse)").matches;
document.documentElement.classList.toggle("touch", isTouch);

function onClick(id: string, f: () => void): void {
  $(id).addEventListener("click", (e) => {
    (e.currentTarget as HTMLElement).blur(); // focus never rests on a button
    f();
  });
}

onClick("toggle", () => {
  if (!showingStart) stop();
  else if (performance.now() - startShownAt >= START_GUARD_MS) send({ type: "start" });
});
onClick("units", () =>
  send({ type: "setDisplayUnits", units: snap?.units.display === "miles" ? "km" : "miles" }),
);
onClick("disconnect", () => send({ type: "disconnect" }));
onClick("connect-btn", () => selected && send({ type: "connect", id: selected }));
onClick("search-btn", () => send({ type: "search" }));
onClick("grant", () => send({ type: "search" }));
onClick("open-settings", () => invoke("open_bluetooth_settings"));
onClick("forget-btn", () => send({ type: "forget" }));

document.querySelectorAll<HTMLButtonElement>(".speed-buttons button").forEach((b) =>
  b.addEventListener("click", () => {
    b.blur();
    send({ type: "adjustSpeed", steps: Number(b.dataset.steps) });
  }),
);
document.querySelectorAll<HTMLButtonElement>(".preset").forEach((b) =>
  b.addEventListener("click", () => {
    b.blur();
    send({ type: "applyPreset", index: Number(b.dataset.preset) });
  }),
);
document.querySelectorAll<HTMLButtonElement>(".modes button").forEach((b) =>
  b.addEventListener("click", () => {
    b.blur();
    send({ type: "setMode", mode: b.dataset.mode as Mode });
  }),
);

// Presets are edited in the display unit, in 0.1 steps.
const editor = $<HTMLFormElement>("preset-editor");
onClick("edit-presets", () => {
  if (!snap) return;
  editor.querySelectorAll<HTMLInputElement>("input").forEach((input) => {
    input.value = snap!.presets[Number(input.dataset.edit)].toFixed(1);
  });
  show(editor, editor.hidden !== false);
});
editor.addEventListener("submit", (e) => {
  e.preventDefault();
  editor.querySelectorAll<HTMLInputElement>("input").forEach((input) => {
    const v = Number(input.value);
    if (Number.isFinite(v) && v > 0) {
      send({ type: "setPreset", index: Number(input.dataset.edit), tenths: Math.round(v * 10) });
    }
  });
  show(editor, false);
});

// Keyboard: Esc and Space are Stop, never Start, wherever focus is. Arrows
// change the speed by 0.1, with Shift by 0.5.
function isStopKey(e: KeyboardEvent): boolean {
  return e.key === "Escape" || e.key === " " || e.code === "Space";
}
window.addEventListener(
  "keydown",
  (e) => {
    if (isStopKey(e)) {
      e.preventDefault();
      e.stopPropagation();
      if (!e.repeat) stop();
      return;
    }
    const inInput = e.target instanceof HTMLInputElement;
    if ((e.key === "ArrowUp" || e.key === "ArrowDown") && !inInput) {
      e.preventDefault();
      if (snap?.controls.speed) {
        const step = e.shiftKey ? 5 : 1;
        send({ type: "adjustSpeed", steps: e.key === "ArrowUp" ? step : -step });
      }
    }
  },
  { capture: true },
);
// A focused button activates on Space keyup; swallow it so Space can only stop.
window.addEventListener(
  "keyup",
  (e) => {
    if (isStopKey(e)) {
      e.preventDefault();
      e.stopPropagation();
    }
  },
  { capture: true },
);

// Scanning stops while the app is in the background (design §5.2).
document.addEventListener("visibilitychange", () =>
  send({ type: "setVisible", visible: document.visibilityState === "visible" }),
);

listen<Snapshot>("pad-state", (e) => render(e.payload));
invoke<Snapshot>("snapshot").then(render);

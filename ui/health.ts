import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface Monitor { id: string; name: string | null; rssi: number | null }
interface HeartState {
  phase: "idle" | "searching" | "connecting" | "connected" | "reconnecting" | "disconnecting";
  monitor: Monitor | null;
  devices: Monitor[];
  bpm: number | null;
  message: string;
}
interface Walk {
  id: string;
  startedMs: number;
  endedMs: number | null;
  interrupted: boolean;
  timeS: number;
  distanceM: number;
  steps: number;
  samples: number;
  averageBpm: number | null;
  maxBpm: number | null;
}
interface WalkState { current: Walk | null; recent: Walk[]; error: string | null }

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
let walks: WalkState = { current: null, recent: [], error: null };
let miles = true;

function send(intent: object): void {
  invoke("heart_rate_intent", { intent }).catch((error) => {
    $("heart-status").textContent = `Heart-rate monitor: ${String(error)}`;
  });
}

function renderHeart(s: HeartState): void {
  $("heart-bpm").textContent = s.bpm == null ? "–" : String(s.bpm);
  $("heart-live").textContent = s.bpm == null ? "–" : String(s.bpm);
  $("heart-status").textContent = s.monitor && s.phase === "connected"
    ? `${s.monitor.name ?? "Heart-rate monitor"} · ${s.message}` : s.message;
  $<HTMLButtonElement>("heart-search").disabled = s.phase !== "idle";
  const disconnect = $<HTMLButtonElement>("heart-disconnect");
  disconnect.hidden = s.phase === "idle";
  disconnect.disabled = s.phase === "disconnecting";
  disconnect.textContent = s.phase === "searching" ? "Stop searching" : "Disconnect monitor";
  $("heart-devices").replaceChildren(...(s.phase === "idle" || s.phase === "searching" ? s.devices : []).map((monitor) => {
    const li = document.createElement("li");
    const label = document.createElement("div");
    label.textContent = monitor.name ?? "Heart-rate monitor";
    const detail = document.createElement("small");
    detail.textContent = `${monitor.id}${monitor.rssi == null ? "" : ` · ${monitor.rssi} dBm`}`;
    label.append(detail);
    const connect = document.createElement("button");
    connect.type = "button";
    connect.textContent = "Connect";
    connect.setAttribute("aria-label", `Connect ${monitor.name ?? monitor.id}`);
    connect.addEventListener("click", () => send({ type: "connect", id: monitor.id }));
    li.append(label, connect);
    return li;
  }));
}

function duration(seconds: number): string {
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

function metrics(walk: Walk): string {
  const distance = (walk.distanceM / (miles ? 1609.344 : 1000)).toFixed(2);
  return `${duration(walk.timeS)} · ${distance} ${miles ? "mi" : "km"} · ${walk.steps.toLocaleString()} steps`;
}

function heartSummary(walk: Walk): string {
  return walk.averageBpm == null ? "No heart-rate readings recorded" : `Average ${walk.averageBpm} BPM · Max ${walk.maxBpm} BPM · ${walk.samples.toLocaleString()} readings`;
}

function renderWalks(): void {
  $("walk-current").textContent = walks.current
    ? `Recording · ${metrics(walks.current)}. ${heartSummary(walks.current)}.`
    : "Walks save automatically when the belt stops. Recording continues with the screen off.";
  $("walk-error").hidden = walks.error == null;
  $("walk-error").textContent = walks.error ?? "";
  $("walk-history").replaceChildren(...walks.recent.map((walk) => {
    const li = document.createElement("li");
    const date = document.createElement("div");
    date.textContent = new Date(walk.startedMs).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
    const stats = document.createElement("p");
    stats.textContent = metrics(walk);
    const heart = document.createElement("p");
    heart.textContent = heartSummary(walk);
    li.append(date, stats, heart);
    if (walk.interrupted) {
      const note = document.createElement("small");
      note.textContent = "Partial walk · pad status was lost or the app closed.";
      li.append(note);
    }
    return li;
  }));
}

export function setHealthUnits(display: "miles" | "km"): void {
  miles = display === "miles";
  renderWalks();
}

$("heart-search").addEventListener("click", () => send({ type: "search" }));
$("heart-disconnect").addEventListener("click", () => send({ type: "disconnect" }));
document.addEventListener("visibilitychange", () => send({ type: "setVisible", visible: document.visibilityState === "visible" }));

async function init(): Promise<void> {
  await listen<HeartState>("heart-rate-state", (event) => renderHeart(event.payload));
  await listen<WalkState>("walks-state", (event) => { walks = event.payload; renderWalks(); });
  const renderBackgroundError = (error: string | null): void => {
    $("background-error").hidden = !error;
    $("background-error").textContent = error ? `${error}. Keep the app visible until background operation is available.` : "";
  };
  await listen<string | null>("background-error", (event) => renderBackgroundError(event.payload));
  renderBackgroundError(await invoke<string | null>("background_status"));
  renderHeart(await invoke<HeartState>("heart_rate_snapshot"));
  walks = await invoke<WalkState>("walks_snapshot");
  renderWalks();
}
init().catch((error) => { $("heart-status").textContent = `Couldn't load heart-rate tracking: ${String(error)}`; });

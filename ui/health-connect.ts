import { invoke } from "@tauri-apps/api/core";

interface Status {
  availability: "available" | "unavailable" | "installRequired" | "unsupported";
  enabled: boolean;
  permitted: boolean;
  syncedWalks: number;
  lastSyncedMs: number | null;
  message: string | null;
}

const panel = document.getElementById("health-connect-panel")!;
const status = document.getElementById("health-connect-status")!;
const connect = document.getElementById("health-connect-enable") as HTMLButtonElement;
const disconnect = document.getElementById("health-connect-disable") as HTMLButtonElement;
const settings = document.getElementById("health-connect-settings") as HTMLButtonElement;
let busy = false;
let refreshing = false;

function render(s: Status): void {
  panel.hidden = s.availability === "unsupported";
  connect.hidden = s.enabled && s.permitted && s.availability === "available";
  connect.disabled = busy || s.availability === "unavailable";
  connect.textContent = s.availability === "installRequired" ? "Install or update Health Connect"
    : s.enabled ? "Restore permissions" : "Connect Health Connect";
  disconnect.hidden = !s.enabled;
  disconnect.disabled = busy;
  settings.hidden = s.availability !== "available";
  settings.disabled = busy;
  let text = s.availability === "unavailable" ? "Health Connect is unavailable on this device."
    : s.availability === "installRequired" ? "Install or update Health Connect to sync your walks."
    : !s.enabled ? "Connect once to automatically save your walks, distance, steps, and heart rate to Health Connect. Existing saved walks are included."
    : !s.permitted ? "Sync paused. Restore Health Connect write permissions to continue."
    : `Automatic sync on. ${s.syncedWalks.toLocaleString()} ${s.syncedWalks === 1 ? "walk" : "walks"} synced.`;
  if (s.enabled && s.lastSyncedMs) text += ` Last synced ${new Date(s.lastSyncedMs).toLocaleString()}.`;
  if (s.message) text += ` ${s.message}`;
  status.textContent = text;
}

async function refresh(): Promise<void> {
  if (refreshing || busy) return;
  refreshing = true;
  try { render(await invoke<Status>("health_connect_status")); }
  catch (error) {
    panel.hidden = false;
    status.textContent = `Couldn't check Health Connect: ${String(error)}`;
  } finally { refreshing = false; }
}

async function act(action: "connect" | "disconnect" | "settings"): Promise<void> {
  if (busy) return;
  busy = true;
  for (const button of [connect, disconnect, settings]) button.disabled = true;
  let error: unknown;
  try { await invoke("health_connect_action", { action }); }
  catch (failure) { error = failure; }
  finally { busy = false; }
  await refresh();
  if (error) status.textContent = String(error);
}

connect.addEventListener("click", () => void act("connect"));
disconnect.addEventListener("click", () => void act("disconnect"));
settings.addEventListener("click", () => void act("settings"));
document.addEventListener("visibilitychange", () => { if (!document.hidden) void refresh(); });
setInterval(() => { if (!document.hidden && !panel.hidden) void refresh(); }, 10_000);
void refresh();

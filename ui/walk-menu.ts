// One context menu shared by stable saved-walk rows.
const menu = document.getElementById("walk-menu")!;
const items = Array.from(menu.querySelectorAll<HTMLButtonElement>("button"));
let selected: HTMLElement | null = null;
let pending: ReturnType<typeof setTimeout> | undefined;
let press: { row: HTMLElement; x: number; y: number; pointer: number } | null = null;
let suppressClick: HTMLElement | null = null;
let busy = false;

function cancelPress(): void {
  clearTimeout(pending);
  pending = undefined;
  press = null;
}

export function closeWalkMenu(restoreFocus = true): void {
  cancelPress();
  menu.hidden = true;
  if (selected) {
    selected.querySelector("button")?.setAttribute("aria-expanded", "false");
    if (restoreFocus && selected.isConnected) selected.querySelector("button")?.focus({ preventScroll: true });
  }
  selected = null;
}

function open(row: HTMLElement, x: number, y: number): void {
  if (busy) return;
  closeWalkMenu(false);
  selected = row;
  row.querySelector("button")?.setAttribute("aria-expanded", "true");
  menu.hidden = false;
  const rect = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(x, window.innerWidth - rect.width - 8))}px`;
  menu.style.top = `${Math.max(8, Math.min(y, window.innerHeight - rect.height - 8))}px`;
  items[0].focus({ preventScroll: true });
}

export function bindWalkMenu(row: HTMLElement): void {
  const button = row.querySelector("button")!;
  button.addEventListener("click", () => {
    if (selected === row) { closeWalkMenu(); return; }
    const rect = button.getBoundingClientRect();
    open(row, rect.right, rect.bottom);
  });
  row.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    cancelPress();
    suppressClick = row;
    if (selected !== row) open(row, e.clientX, e.clientY);
  });
  row.addEventListener("keydown", (e) => {
    if (e.key === "ContextMenu" || (e.shiftKey && e.key === "F10")) {
      e.preventDefault();
      const rect = button.getBoundingClientRect();
      open(row, rect.left, rect.bottom);
    }
  });
  row.addEventListener("pointerdown", (e) => {
    if (!e.isPrimary || e.button !== 0) return;
    cancelPress();
    press = { row, x: e.clientX, y: e.clientY, pointer: e.pointerId };
    pending = setTimeout(() => {
      const held = press;
      cancelPress();
      if (!held || !row.isConnected) return;
      suppressClick = row;
      open(row, held.x, held.y);
    }, 550);
  });
  row.addEventListener("click", (e) => {
    if (suppressClick === row) {
      e.preventDefault();
      e.stopImmediatePropagation();
      suppressClick = null;
    }
  }, { capture: true });
}

export function walkMenuRemoved(row: HTMLElement): void {
  if (selected === row) {
    const focused = menu.contains(document.activeElement);
    closeWalkMenu(false);
    if (focused) document.getElementById("walk-title")?.focus();
  }
  if (press?.row === row) cancelPress();
}

export function onWalkAction(action: (id: string, type: "download" | "delete") => Promise<void>): void {
  for (const item of items) item.addEventListener("click", () => {
    const id = selected?.dataset.walkId;
    const type = item.dataset.action;
    if (!id || busy || (type !== "download" && type !== "delete")) return;
    closeWalkMenu();
    busy = true;
    for (const button of items) button.disabled = true;
    void action(id, type).finally(() => {
      busy = false;
      for (const button of items) button.disabled = false;
    });
  });
}

document.addEventListener("pointerdown", (e) => {
  if (!e.isPrimary) cancelPress();
  suppressClick = null;
  if (!menu.hidden && e.target instanceof Node && !menu.contains(e.target) && !selected?.contains(e.target)) closeWalkMenu(false);
}, { capture: true });
document.addEventListener("pointermove", (e) => {
  if (press && e.pointerId === press.pointer && Math.hypot(e.clientX - press.x, e.clientY - press.y) > 10) cancelPress();
});
document.addEventListener("pointerup", cancelPress);
document.addEventListener("pointercancel", cancelPress);
document.addEventListener("scroll", () => closeWalkMenu(false), { capture: true });
window.addEventListener("resize", () => closeWalkMenu(false));
document.addEventListener("visibilitychange", () => { if (document.hidden) closeWalkMenu(false); });
menu.addEventListener("keydown", (e) => {
  if (["ArrowDown", "ArrowUp", "Home", "End"].includes(e.key)) {
    e.preventDefault();
    const current = items.indexOf(document.activeElement as HTMLButtonElement);
    const next = e.key === "Home" ? 0 : e.key === "End" ? items.length - 1
      : (current + (e.key === "ArrowDown" ? 1 : -1) + items.length) % items.length;
    items[next].focus();
  } else if (e.key === "Escape") closeWalkMenu();
  else if (e.key === "Tab") closeWalkMenu();
});

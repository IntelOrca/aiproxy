/**
 * Persistence for session pins so routing survives restarts.
 *
 * Pins are stored as JSON in the platform state/cache directory:
 *   Windows:    %LOCALAPPDATA%\aiproxy\pins.json
 *   Linux/macOS: $XDG_CACHE_HOME/aiproxy/pins.json or ~/.cache/aiproxy/pins.json
 *
 * The app needs --allow-write (and read) for this to work; otherwise load and
 * save silently no-op.
 */

export interface StoredPin {
  backendId: string;
  lastSeen: number;
}

const PIN_FILE_VERSION = 1;

function join(...parts: string[]): string {
  const sep = Deno.build.os === "windows" ? "\\" : "/";
  return parts.join(sep);
}

/** Platform state/cache directory for aiproxy (fallback: ./.aiproxy-cache). */
export function defaultStateDir(): string {
  if (Deno.build.os === "windows") {
    const base = Deno.env.get("LOCALAPPDATA");
    if (base) return join(base, "aiproxy");
  } else {
    const xdg = Deno.env.get("XDG_CACHE_HOME");
    if (xdg) return join(xdg, "aiproxy");
    const home = Deno.env.get("HOME");
    if (home) return join(home, ".cache", "aiproxy");
  }
  return join(Deno.cwd(), ".aiproxy-cache");
}

function pinsFile(stateDir: string): string {
  return join(stateDir, "pins.json");
}

/** Load persisted pins. Returns an empty map when nothing is stored yet. */
export function loadPins(stateDir: string): Map<string, StoredPin> {
  try {
    const data = JSON.parse(Deno.readTextFileSync(pinsFile(stateDir)));
    if (data?.version !== PIN_FILE_VERSION || !data.pins) return new Map();
    const pins = new Map<string, StoredPin>();
    for (const [key, value] of Object.entries(data.pins)) {
      const v = value as Partial<StoredPin>;
      if (v && typeof v.backendId === "string" && typeof v.lastSeen === "number") {
        pins.set(key, { backendId: v.backendId, lastSeen: v.lastSeen });
      }
    }
    return pins;
  } catch {
    return new Map();
  }
}

/** Persist pins atomically (temp file + rename). Best-effort. */
export function savePins(stateDir: string, pins: Map<string, StoredPin>): void {
  const file = pinsFile(stateDir);
  const payload = JSON.stringify(
    { version: PIN_FILE_VERSION, pins: Object.fromEntries(pins) },
    null,
    2,
  );
  try {
    Deno.mkdirSync(stateDir, { recursive: true });
    const tmp = file + ".tmp";
    Deno.writeTextFileSync(tmp, payload);
    try {
      Deno.renameSync(tmp, file);
    } catch {
      // rename over an existing file can fail on some platforms; fall back.
      Deno.writeTextFileSync(file, payload);
    }
  } catch (err) {
    if (err instanceof Deno.errors.PermissionDenied) return; // persistence not enabled
    console.warn(
      `[aiproxy] could not persist session pins: ${err instanceof Error ? err.message : err}`,
    );
  }
}

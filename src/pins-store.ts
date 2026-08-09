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

/** Join paths with the platform separator. */
export function joinPath(...parts: string[]): string {
  return join(...parts);
}

/**
 * Atomically write a JSON value to `<stateDir>/<filename>` (temp file +
 * rename). Silently no-ops when write permission is missing.
 */
export function writeJsonAtomic(
  stateDir: string,
  filename: string,
  data: unknown,
): void {
  const file = join(stateDir, filename);
  const payload = JSON.stringify(data, null, 2);
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
      `[aiproxy] could not persist ${filename}: ${
        err instanceof Error ? err.message : err
      }`,
    );
  }
}

/** Read and parse a JSON file from the state dir, or null when missing/invalid. */
export function readJson(stateDir: string, filename: string): unknown {
  try {
    return JSON.parse(Deno.readTextFileSync(join(stateDir, filename)));
  } catch {
    return null;
  }
}

/** Load persisted pins. Returns an empty map when nothing is stored yet. */
export function loadPins(stateDir: string): Map<string, StoredPin> {
  const data = readJson(stateDir, "pins.json") as {
    version?: number;
    pins?: Record<string, Partial<StoredPin>>;
  } | null;
  if (data?.version !== PIN_FILE_VERSION || !data.pins) return new Map();
  const pins = new Map<string, StoredPin>();
  for (const [key, value] of Object.entries(data.pins)) {
    if (
      value && typeof value.backendId === "string" &&
      typeof value.lastSeen === "number"
    ) {
      pins.set(key, { backendId: value.backendId, lastSeen: value.lastSeen });
    }
  }
  return pins;
}

/** Persist pins atomically (temp file + rename). Best-effort. */
export function savePins(stateDir: string, pins: Map<string, StoredPin>): void {
  writeJsonAtomic(stateDir, "pins.json", {
    version: PIN_FILE_VERSION,
    pins: Object.fromEntries(pins),
  });
}

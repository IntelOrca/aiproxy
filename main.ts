import { loadConfig } from "./src/config.ts";
import { startAiproxy } from "./src/app.ts";

function parseArgs(args: string[]): { configPath: string; port?: number } {
  let configPath = "config.json";
  let port: number | undefined;
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "--config") {
      configPath = args[++i] ?? configPath;
    } else if (a === "-p" || a === "--port") {
      const v = Number(args[++i]);
      if (Number.isFinite(v)) port = v;
    }
  }
  return { configPath, port };
}

const { configPath, port: portArg } = parseArgs(Deno.args);
const config = loadConfig(configPath);
if (portArg !== undefined) config.port = portArg;

console.log(
  `[aiproxy] config: ${configPath} — listening on http://127.0.0.1:${config.port} — backends: ` +
    config.backends.map((b) => `${b.id}(${b.baseUrl})`).join(", "),
);

startAiproxy(config);

import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"

function healthPath(): string {
  if (process.env.GECO_HEALTH) return process.env.GECO_HEALTH
  const home = os.homedir()
  if (process.platform === "darwin")
    return path.join(home, "Library", "Application Support", "dev.geco.geco", "health.json")
  if (process.platform === "win32")
    return path.join(process.env.APPDATA || path.join(home, "AppData", "Roaming"),
                     "geco", "geco", "data", "health.json")
  return path.join(process.env.XDG_DATA_HOME || path.join(home, ".local", "share"),
                   "dev.geco.geco", "health.json")
}

type Issue = { kind: string; subject: string; detail: string; severity: string }
type Report = { generation: number; healthy: boolean; issues: Issue[]; client?: string }

let lastKey = ""

const fingerprint = (r: Report) =>
  r.issues.map((i) => `${i.kind}:${i.subject}:${i.detail}`).sort().join("|")

const summarize = (issues: Issue[]) =>
  issues.slice(0, 3).map((i) => `${i.subject}: ${i.detail}`).join("\n") +
  (issues.length > 3 ? `\n…and ${issues.length - 3} more` : "")

const GecoTuiPlugin = async ({ client }: any) => {
  const log = (level: string, message: string) =>
    client.app.log({ body: { service: "geco-tui", level, message } }).catch(() => {})

  await log("info", "geco-tui plugin init")

  const check = async () => {
    let raw: string
    try { raw = fs.readFileSync(healthPath(), "utf-8") } catch { return }
    let r: Report
    try { r = JSON.parse(raw) } catch { return }
    if (r.client && r.client !== "opencode") return
    const key = fingerprint(r)
    if (key === lastKey) return
    const wasUnhealthy = lastKey !== ""
    lastKey = key
    const issues = r.issues
    const msg = issues.length === 0
      ? "geco: recovered — all backends healthy"
      : `geco: ${issues.length} issue${issues.length > 1 ? "s" : ""}\n${summarize(issues)}`
    const variant = issues.length === 0 ? "success"
      : issues.some((i) => i.severity === "error") ? "error" : "warning"
    await log("info", `toast: ${msg}`)
    try {
      await client.tui.showToast({ body: { message: msg, variant } })
      await log("info", "showToast OK")
    } catch (e: any) {
      await log("error", `showToast failed: ${e?.message ?? e}`)
    }
  }

  // No unref — let the timer keep the event loop alive.
  // The hooks.ts plugin uses fs.watch with unref, but setInterval
  // without unref should still fire and not block startup since
  // it's created inside the plugin function (after init).
  const timer = setInterval(() => { check().catch(() => {}) }, 5000)

  return {
    dispose: () => clearInterval(timer),
    event: async ({ event }: any) => {
      await log("debug", `event: ${event?.type}`)
      await check()
    },
    "tool.execute.after": async (input: any) => {
      await log("debug", `tool.after: ${input?.tool}`)
      await check()
    },
  }
}

export default GecoTuiPlugin

/**
 * geco-tui — surfaces geco's health as opencode toasts.
 *
 * The gateway runs headless; when a backend endpoint is down, an absorb
 * collides, or a refresh half-fails, that pain would otherwise live only in a
 * stderr log nobody reads. geco writes its current problem set to
 * `<data>/health.json` (level-triggered: empty == healthy). This plugin polls
 * that file and toasts when the problem set CHANGES — a new problem raises an
 * error/warning toast; recovery (problems → none) raises a success toast. A
 * persistent problem is shown once, not every tick.
 *
 * Install: copy to ~/.config/opencode/plugin/geco-tui.ts
 * Requires: the geco MCP server configured in opencode.
 */

import type { TuiPlugin } from "@opencode-ai/plugin/tui"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"

// geco's data dir (macOS + Linux + Windows). Override with GECO_HEALTH.
function healthPath(): string {
  if (process.env.GECO_HEALTH) return process.env.GECO_HEALTH
  const home = os.homedir()
  if (process.platform === "darwin") {
    return path.join(home, "Library", "Application Support", "dev.geco.geco", "health.json")
  }
  if (process.platform === "win32") {
    return path.join(process.env.APPDATA || path.join(home, "AppData", "Roaming"),
                     "geco", "geco", "data", "health.json")
  }
  return path.join(process.env.XDG_DATA_HOME || path.join(home, ".local", "share"),
                   "dev.geco.geco", "health.json")
}

const POLL_MS = 5000

type Issue = { kind: string; subject: string; detail: string; severity: string }
type Report = { generation: number; healthy: boolean; issues: Issue[]; client?: string }

const tuiPlugin: TuiPlugin = async (api) => {
  const { ui } = api
  const file = healthPath()
  let lastKey = ""          // fingerprint of the last problem set we showed

  const fingerprint = (r: Report) =>
    r.issues.map((i) => `${i.kind}:${i.subject}:${i.detail}`).sort().join("|")

  const summarize = (issues: Issue[]) => {
    const head = issues.slice(0, 3)
      .map((i) => `• ${i.subject}: ${i.detail}`).join("\n")
    const more = issues.length > 3 ? `\n…and ${issues.length - 3} more` : ""
    return head + more
  }

  const tick = () => {
    let raw: string
    try {
      raw = fs.readFileSync(file, "utf-8")
    } catch {
      return // gateway hasn't written yet, or isn't running — silent
    }
    let r: Report
    try {
      r = JSON.parse(raw)
    } catch {
      return
    }

    // Only toast for our own client — health.json is shared across agents.
    if (r.client && r.client !== "opencode") return

    const key = fingerprint(r)
    if (key === lastKey) return // same problem set, already shown
    const wasUnhealthy = lastKey !== ""
    lastKey = key

    if (r.issues.length === 0) {
      if (wasUnhealthy) {
        ui.toast({ variant: "success", title: "geco: recovered",
                   message: "All gateway backends healthy again." })
      }
      return
    }
    const worst = r.issues.some((i) => i.severity === "error") ? "error" : "warning"
    ui.toast({
      variant: worst,
      title: `geco: ${r.issues.length} issue${r.issues.length > 1 ? "s" : ""}`,
      message: summarize(r.issues) +
               "\n\nRun `geco inspect` or the health tool for detail.",
    })
  }

  tick()
  const timer = setInterval(tick, POLL_MS)
  return { dispose: () => clearInterval(timer) }
}

export default tuiPlugin

/**
 * geco-tui — surfaces geco's health as opencode toasts.
 *
 * Uses the opencode event system instead of setInterval to avoid
 * blocking startup. Hooks into session events and checks health on
 * each session update.
 *
 * Status: PARKED — see TODO.md. It loads, but no hooks ever fire.
 *
 * Install: copy to ~/.config/opencode/plugins/geco-tui.ts — which is what
 * install.sh and install.ps1 do. Note TODO.md records the opposite finding
 * (that opencode scans the singular `plugin/`); the uninstallers clean both,
 * and settling which is right is part of un-parking this.
 */

import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"

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
  // Linux: bare lowercased app name, NOT the reverse-DNS bundle id. The
  // directories crate ignores qualifier/organization on Linux (verified in
  // directories 6.0.0 src/lin.rs: project_dirs_from(_qualifier,
  // _organization, application)), so Rust writes $XDG_DATA_HOME/geco/…
  // while macOS gets dev.geco.geco. uninstall.sh documents the same shape.
  // This branch has now been wrong in BOTH directions — check the crate
  // source, not the other platforms' branches, before touching it again.
  return path.join(process.env.XDG_DATA_HOME || path.join(home, ".local", "share"),
                   "geco", "health.json")
}

type Issue = { kind: string; subject: string; detail: string; severity: string }
type Report = { generation: number; healthy: boolean; issues: Issue[]; client?: string }

export default async ({ client }) => {
  const file = healthPath()
  let lastKey = ""

  const fingerprint = (r: Report) =>
    r.issues.map((i) => `${i.kind}:${i.subject}:${i.detail}`).sort().join("|")

  const summarize = (issues: Issue[]) => {
    const head = issues.slice(0, 3)
      .map((i) => `${i.subject}: ${i.detail}`).join("\n")
    const more = issues.length > 3 ? `\n…and ${issues.length - 3} more` : ""
    return head + more
  }

  const check = async () => {
    let raw: string
    try {
      raw = fs.readFileSync(file, "utf-8")
    } catch {
      return
    }
    let r: Report
    try {
      r = JSON.parse(raw)
    } catch {
      return
    }

    if (r.client && r.client !== "opencode") return

    const key = fingerprint(r)
    if (key === lastKey) return
    const wasUnhealthy = lastKey !== ""
    lastKey = key

    if (r.issues.length === 0) {
      if (wasUnhealthy) {
        try {
          await client.tui.showToast({ body: { message: "geco: recovered — all backends healthy", variant: "success" } })
        } catch {}
      }
      return
    }
    const worst = r.issues.some((i) => i.severity === "error") ? "error" : "warning"
    const msg = `geco: ${r.issues.length} issue${r.issues.length > 1 ? "s" : ""}\n${summarize(r.issues)}`
    try {
      await client.tui.showToast({ body: { message: msg, variant: worst } })
    } catch {}
  }

  return {
    event: async (input) => {
      // Check health on session events (session.idle, session.updated, etc.)
      if (input.type === "session.idle" || input.type === "session.created") {
        await check()
      }
    },
  }
}

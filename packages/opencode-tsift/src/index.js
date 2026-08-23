import { execFile } from "node:child_process";
import { promisify } from "node:util";

import { installCommands } from "./install.js";

const execFileAsync = promisify(execFile);

async function resolveTsiftBin() {
  const binName = process.platform === "win32" ? "tsift.exe" : "tsift";
  try {
    const { stdout } = await execFileAsync("which", [binName], { timeout: 5000 });
    return stdout.trim();
  } catch {
    return binName;
  }
}

export const TsiftOpenCodePlugin = async (ctx = {}) => {
  const projectDir = ctx.directory || ctx.worktree || process.cwd();
  const log = ctx.client?.app?.log?.bind(ctx.client.app) ?? (() => {});

  async function ensureInstalled() {
    const updates = await installCommands(projectDir);
    const changed = updates.filter((update) => update.action !== "already present");
    if (changed.length > 0) {
      await log({
        body: {
          service: "opencode-tsift",
          level: "info",
          message: "tsift OpenCode commands installed",
          extra: {
            projectDir,
            files: changed.map((update) => update.name),
          },
        },
      });
    }
  }

  async function ensureIndexFresh() {
    let tsiftBin;
    try {
      tsiftBin = await resolveTsiftBin();
    } catch {
      return;
    }

    let report;
    try {
      const { stdout } = await execFileAsync(tsiftBin, ["status", "--json", projectDir], { timeout: 15000 });
      report = JSON.parse(stdout);
    } catch {
      return;
    }

    const state = report?.index?.state;
    if (state !== "stale" && state !== "missing") return;

    try {
      await execFileAsync(tsiftBin, ["status", projectDir], { timeout: 120000 });
      await log({
        body: {
          service: "opencode-tsift",
          level: "info",
          message: "tsift auto-reindex completed",
          extra: { projectDir, previousState: state },
        },
      });
    } catch (error) {
      await log({
        body: {
          service: "opencode-tsift",
          level: "warn",
          message: `tsift auto-reindex failed: ${error.message}`,
          extra: { projectDir },
        },
      });
    }
  }

  await ensureInstalled();
  await ensureIndexFresh();

  return {
    "installation.updated": async () => {
      await ensureInstalled();
      await ensureIndexFresh();
    },
  };
};

export default TsiftOpenCodePlugin;

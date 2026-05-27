import { installCommands } from "./install.js";

export const TsiftOpenCodePlugin = async (ctx = {}) => {
  const projectDir = ctx.directory || ctx.worktree || process.cwd();

  async function ensureInstalled() {
    const updates = await installCommands(projectDir);
    const changed = updates.filter((update) => update.action !== "already present");
    if (changed.length > 0 && ctx.client?.app?.log) {
      await ctx.client.app.log({
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

  await ensureInstalled();

  return {
    "installation.updated": ensureInstalled,
  };
};

export default TsiftOpenCodePlugin;

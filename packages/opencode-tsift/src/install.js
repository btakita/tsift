import { mkdir, readdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const COMMAND_MARKER = "<!-- tsift:opencode-command";

function packageRoot() {
  return dirname(dirname(fileURLToPath(import.meta.url)));
}

async function readIfPresent(path) {
  try {
    return await readFile(path, "utf8");
  } catch (error) {
    if (error?.code === "ENOENT") {
      return undefined;
    }
    throw error;
  }
}

export async function installCommands(projectDir = process.cwd()) {
  const root = resolve(projectDir);
  const sourceDir = join(packageRoot(), "commands");
  const targetDir = join(root, ".opencode", "commands");
  await mkdir(targetDir, { recursive: true });

  const files = (await readdir(sourceDir))
    .filter((file) => file.endsWith(".md"))
    .sort();

  const updates = [];
  for (const file of files) {
    const content = await readFile(join(sourceDir, file), "utf8");
    const target = join(targetDir, file);
    const existing = await readIfPresent(target);
    let action = "created";

    if (existing !== undefined) {
      if (existing === content) {
        action = "already present";
      } else if (!existing.includes(COMMAND_MARKER)) {
        throw new Error(
          `${target} already exists and is not managed by tsift; move it or add the tsift marker before installing opencode-tsift`,
        );
      } else {
        action = "updated";
      }
    }

    if (action !== "already present") {
      await writeFile(target, content);
    }

    updates.push({
      action,
      file: target,
      name: file.replace(/\.md$/, ""),
    });
  }

  return updates;
}

export async function installFromCli(args = process.argv.slice(2)) {
  const projectDir = resolve(args[0] ?? process.cwd());
  const updates = await installCommands(projectDir);
  for (const update of updates) {
    console.log(`${relative(projectDir, update.file)}: ${update.action} (OpenCode /${update.name} tsift shortcut)`);
  }
}

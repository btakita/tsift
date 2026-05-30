import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, readdir, rm, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import test from "node:test";

import { installCommands } from "../src/install.js";

const execFileAsync = promisify(execFile);
const __dirname = dirname(fileURLToPath(import.meta.url));
const packageRoot = resolve(__dirname, "..");

async function tempProject() {
  return mkdtemp(join(tmpdir(), "opencode-tsift-"));
}

function tsiftBinaryPath() {
  return resolve(packageRoot, "..", "..", "target", "debug", "tsift");
}

test("installs marker-owned tsift commands", async () => {
  const project = await tempProject();
  try {
    const updates = await installCommands(project);
    assert.equal(updates.length, 7);
    assert.ok(updates.every((update) => update.action === "created"));

    const status = await readFile(
      join(project, ".opencode", "commands", "tsift-status.md"),
      "utf8",
    );
    assert.match(status, /tsift:opencode-command/);
    assert.match(status, /tsift status --fix/);

    const rewriteRun = await readFile(
      join(project, ".opencode", "commands", "tsift-rewrite-run.md"),
      "utf8",
    );
    assert.match(rewriteRun, /tsift rewrite --run/);
    assert.match(rewriteRun, /digest-runner/);
  } finally {
    await rm(project, { recursive: true, force: true });
  }
});

test("is idempotent when command files already match", async () => {
  const project = await tempProject();
  try {
    await installCommands(project);
    const updates = await installCommands(project);
    assert.ok(updates.every((update) => update.action === "already present"));
  } finally {
    await rm(project, { recursive: true, force: true });
  }
});

test("refuses unmanaged command conflicts", async () => {
  const project = await tempProject();
  try {
    const commands = join(project, ".opencode", "commands");
    await mkdir(commands, { recursive: true });
    await writeFile(join(commands, "tsift-status.md"), "---\ndescription: custom\n---\n");

    await assert.rejects(
      installCommands(project),
      /not managed by tsift/,
    );
  } finally {
    await rm(project, { recursive: true, force: true });
  }
});

test("npm package output matches tsift init --opencode output", async () => {
  const tsiftBin = tsiftBinaryPath();
  let project;
  try {
    const { stdout } = await execFileAsync(tsiftBin, ["--version"], {
      timeout: 5000,
    });
  } catch {
    return;
  }

  project = await tempProject();
  try {
    await execFileAsync(tsiftBin, ["init", "--opencode", project], {
      timeout: 10000,
    });

    const tsiftCommandsDir = join(project, ".opencode", "commands");
    const tsiftFiles = (await readdir(tsiftCommandsDir))
      .filter((f) => f.endsWith(".md"))
      .sort();

    const npmProject = await tempProject();
    try {
      await installCommands(npmProject);
      const npmCommandsDir = join(npmProject, ".opencode", "commands");
      const npmFiles = (await readdir(npmCommandsDir))
        .filter((f) => f.endsWith(".md"))
        .sort();

      assert.deepStrictEqual(tsiftFiles, npmFiles, "command file names should match");

      for (const file of tsiftFiles) {
        const tsiftContent = await readFile(join(tsiftCommandsDir, file), "utf8");
        const npmContent = await readFile(join(npmCommandsDir, file), "utf8");
        assert.strictEqual(tsiftContent, npmContent, `${file} content should match`);
      }
    } finally {
      await rm(npmProject, { recursive: true, force: true });
    }
  } finally {
    await rm(project, { recursive: true, force: true });
  }
});

test("npm package is idempotent after tsift init --opencode", async () => {
  const tsiftBin = tsiftBinaryPath();
  try {
    await execFileAsync(tsiftBin, ["--version"], { timeout: 5000 });
  } catch {
    return;
  }

  const project = await tempProject();
  try {
    await execFileAsync(tsiftBin, ["init", "--opencode", project], {
      timeout: 10000,
    });

    const updates = await installCommands(project);
    assert.ok(
      updates.every((update) => update.action === "already present"),
      "npm install should detect tsift-init files as already present",
    );
  } finally {
    await rm(project, { recursive: true, force: true });
  }
});

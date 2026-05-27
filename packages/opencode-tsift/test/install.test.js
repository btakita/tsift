import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { installCommands } from "../src/install.js";

async function tempProject() {
  return mkdtemp(join(tmpdir(), "opencode-tsift-"));
}

test("installs marker-owned tsift commands", async () => {
  const project = await tempProject();
  try {
    const updates = await installCommands(project);
    assert.equal(updates.length, 6);
    assert.ok(updates.every((update) => update.action === "created"));

    const status = await readFile(
      join(project, ".opencode", "commands", "tsift-status.md"),
      "utf8",
    );
    assert.match(status, /tsift:opencode-command/);
    assert.match(status, /tsift status --fix/);
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

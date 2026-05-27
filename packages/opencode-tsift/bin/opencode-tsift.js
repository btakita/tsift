#!/usr/bin/env node
import { installFromCli } from "../src/install.js";

installFromCli().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
});

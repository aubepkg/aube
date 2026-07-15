#!/usr/bin/env bun

import path from "node:path"
import { createRequire } from "node:module"

const requireFromProject = createRequire(path.resolve("package.json"))
const { bunPlugin } = requireFromProject("@jdxcode/aube-node/bun-plugin")

const result = await Bun.build({
  files: { "aube-node-entry.js": 'require("@jdxcode/aube-node")\n' },
  entrypoints: ["aube-node-entry.js"],
  plugins: [bunPlugin({
    os: process.env.AUBE_NODE_OS,
    arch: process.env.AUBE_NODE_ARCH,
    libc: process.env.AUBE_NODE_LIBC,
  })],
  compile: { outfile: path.resolve("aube-node-smoke") },
})

if (!result.success) throw new AggregateError(result.logs, "failed to compile aube Node-API smoke test")

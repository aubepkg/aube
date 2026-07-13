import { access, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"

const { install } = require("./aube.node") as {
  install(
    projectDir: string,
    input?: {
      add?: { name: string; version?: string }[]
      force?: boolean
      offline?: boolean
    },
  ): Promise<{ projectDir: string; added: string[] }>
}

const projectDir = await mkdtemp(path.join(tmpdir(), "aube-node-poc-"))
const concurrentDir = await mkdtemp(path.join(tmpdir(), "aube-node-poc-concurrent-"))
const lifecycleMarker = path.join(projectDir, "postinstall-ran")

try {
  await writeFile(
    path.join(projectDir, "package.json"),
    JSON.stringify(
      {
        private: true,
        scripts: {
          postinstall: `node -e "require('fs').writeFileSync(${JSON.stringify(lifecycleMarker)}, '')"`,
        },
      },
      null,
      2,
    ) + "\n",
  )

  const first = await install(projectDir, {
    add: [{ name: "is-number", version: "7.0.0" }],
  })
  if (first.added.join(",") !== "is-number@7.0.0") {
    throw new Error(`unexpected add result: ${JSON.stringify(first)}`)
  }

  const installed = JSON.parse(
    await readFile(path.join(projectDir, "node_modules", "is-number", "package.json"), "utf8"),
  ) as { version?: string }

  if (installed.version !== "7.0.0") {
    throw new Error(`expected is-number@7.0.0, found ${installed.version ?? "unknown"}`)
  }

  const lifecycleRan = await access(lifecycleMarker).then(
    () => true,
    () => false,
  )
  if (lifecycleRan) throw new Error("aube Node-API install executed the root postinstall script")

  await writeFile(path.join(concurrentDir, "package.json"), "{}\n")

  // OpenCode starts dependency installs for multiple config directories in
  // parallel. Exercise that call pattern: the addon safely serializes the
  // command layer until aube's command-scoped globals become reentrant.
  await Promise.all([
    install(projectDir),
    install(concurrentDir, { add: [{ name: "is-number", version: "7.0.0" }] }),
  ])

  const concurrentInstalled = JSON.parse(
    await readFile(path.join(concurrentDir, "node_modules", "is-number", "package.json"), "utf8"),
  ) as { version?: string }
  if (concurrentInstalled.version !== "7.0.0") {
    throw new Error(`concurrent install found ${concurrentInstalled.version ?? "unknown"}`)
  }

  console.log(`aube Node-API install succeeded in ${projectDir}`)
} finally {
  await Promise.all([
    rm(projectDir, { recursive: true, force: true }),
    rm(concurrentDir, { recursive: true, force: true }),
  ])
}

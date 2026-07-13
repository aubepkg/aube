import { access, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"

const { install } = require("./aube.node") as {
  install(projectDir: string): Promise<void>
}

const projectDir = await mkdtemp(path.join(tmpdir(), "aube-node-poc-"))
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
        dependencies: {
          "is-number": "7.0.0",
        },
      },
      null,
      2,
    ) + "\n",
  )

  await install(projectDir)

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

  // A second call exercises aube's install-state fast path through the same
  // async Node-API export.
  await install(projectDir)

  console.log(`aube Node-API install succeeded in ${projectDir}`)
} finally {
  await rm(projectDir, { recursive: true, force: true })
}

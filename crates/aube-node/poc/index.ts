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
const parallelDirA = await mkdtemp(path.join(tmpdir(), "aube-node-poc-parallel-a-"))
const parallelDirB = await mkdtemp(path.join(tmpdir(), "aube-node-poc-parallel-b-"))
const lifecycleMarker = path.join(projectDir, "postinstall-ran")
let registry: ReturnType<typeof Bun.serve> | undefined

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

  let registryArrivals = 0
  let releaseRegistryBarrier: (() => void) | undefined
  const registryBarrier = new Promise<void>((resolve) => {
    releaseRegistryBarrier = resolve
  })
  const packuments = new Map<string, string>()
  await Promise.all(
    ["is-arrayish", "is-odd", "is-number"].map(async (name) => {
      const response = await fetch(`https://registry.npmjs.org/${name}`)
      if (!response.ok) throw new Error(`failed to fetch ${name} packument: ${response.status}`)
      packuments.set(name, await response.text())
    }),
  )
  registry = Bun.serve({
    port: 0,
    idleTimeout: 30,
    async fetch(request) {
      registryArrivals += 1
      if (registryArrivals === 2) releaseRegistryBarrier?.()

      let timeout: ReturnType<typeof setTimeout> | undefined
      try {
        await Promise.race([
          registryBarrier,
          new Promise<never>((_, reject) => {
            timeout = setTimeout(
              () => reject(new Error("parallel installs did not reach the registry together")),
              5_000,
            )
          }),
        ])
      } finally {
        if (timeout) clearTimeout(timeout)
      }

      const name = decodeURIComponent(new URL(request.url).pathname.slice(1))
      const packument = packuments.get(name)
      if (!packument) return new Response("not found", { status: 404 })
      return new Response(packument, { headers: { "content-type": "application/json" } })
    },
  })
  const registryUrl = `http://127.0.0.1:${registry.port}/`
  await Promise.all(
    [parallelDirA, parallelDirB].map(async (dir) => {
      await writeFile(path.join(dir, "package.json"), "{}\n")
      await writeFile(path.join(dir, ".npmrc"), `registry=${registryUrl}\n`)
    }),
  )

  // Each registry request waits for the other install to arrive. A
  // process-wide addon mutex would deadlock here and trip the timeout.
  await Promise.all([
    install(parallelDirA, { add: [{ name: "is-odd", version: "3.0.1" }] }),
    install(parallelDirB, { add: [{ name: "is-arrayish", version: "0.3.2" }] }),
  ])

  const [parallelA, parallelB] = await Promise.all([
    readFile(path.join(parallelDirA, "node_modules", "is-odd", "package.json"), "utf8"),
    readFile(path.join(parallelDirB, "node_modules", "is-arrayish", "package.json"), "utf8"),
  ])
  if ((JSON.parse(parallelA) as { version?: string }).version !== "3.0.1") {
    throw new Error("parallel install A did not materialize is-odd@3.0.1")
  }
  if ((JSON.parse(parallelB) as { version?: string }).version !== "0.3.2") {
    throw new Error("parallel install B did not materialize is-arrayish@0.3.2")
  }

  console.log(`aube Node-API install succeeded in ${projectDir}`)
} finally {
  registry?.stop(true)
  await Promise.all([
    rm(projectDir, { recursive: true, force: true }),
    rm(parallelDirA, { recursive: true, force: true }),
    rm(parallelDirB, { recursive: true, force: true }),
  ])
}

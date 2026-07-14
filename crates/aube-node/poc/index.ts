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
      onEvent?: (event: InstallEvent) => void
      signal?: AbortSignal
    },
  ): Promise<{ projectDir: string; added: string[] }>
}

type InstallEvent = {
  kind: "phase" | "progress" | "output"
  phase?: "resolving" | "fetching" | "linking" | "complete"
  level?: "info" | "warning" | "error"
  code?: string
  message?: string
  resolved?: number
  total?: number
  reused?: number
  downloaded?: number
  downloadedBytes?: number
  estimatedBytes?: number
}

const projectDir = await mkdtemp(path.join(tmpdir(), "aube-node-poc-"))
const parallelDirA = await mkdtemp(path.join(tmpdir(), "aube-node-poc-parallel-a-"))
const parallelDirB = await mkdtemp(path.join(tmpdir(), "aube-node-poc-parallel-b-"))
const abortDir = await mkdtemp(path.join(tmpdir(), "aube-node-poc-abort-"))
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

  const events: InstallEvent[] = []
  const first = await install(projectDir, {
    add: [{ name: "is-number", version: "7.0.0" }],
    onEvent: (event) => events.push(event),
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
  if (!events.some((event) => event.kind === "phase" && event.phase === "complete")) {
    throw new Error(`install did not emit a complete phase: ${JSON.stringify(events)}`)
  }
  if (!events.some((event) => event.kind === "progress" && event.resolved === 1)) {
    throw new Error(`install did not emit resolved progress: ${JSON.stringify(events)}`)
  }
  if (!events.some((event) => event.kind === "output" && event.message?.startsWith("Resolving "))) {
    throw new Error(`install did not route add output through events: ${JSON.stringify(events)}`)
  }

  const invalidParent = path.join(projectDir, "not-a-directory")
  await writeFile(invalidParent, "file\n")
  try {
    await install(path.join(invalidParent, "child"))
    throw new Error("invalid project install unexpectedly succeeded")
  } catch (error) {
    const structured = error as Error & { code?: string; diagnostic?: string }
    if (structured.code !== "ERR_AUBE_EMBED_INVALID_PROJECT") {
      throw new Error(`missing structured error code: ${JSON.stringify(structured)}`)
    }
    if (!structured.diagnostic?.includes("invalid project directory")) {
      throw new Error(`missing structured diagnostic: ${JSON.stringify(structured)}`)
    }
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
      const name = decodeURIComponent(new URL(request.url).pathname.slice(1))
      const packument = packuments.get(name)
      if (!packument) return new Response("not found", { status: 404 })

      if (name === "is-number") {
        await Bun.sleep(100)
        return new Response(packument, { headers: { "content-type": "application/json" } })
      }

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

      return new Response(packument, { headers: { "content-type": "application/json" } })
    },
  })
  const registryUrl = `http://127.0.0.1:${registry.port}/`
  await Promise.all(
    [parallelDirA, parallelDirB, abortDir].map(async (dir) => {
      await writeFile(path.join(dir, "package.json"), "{}\n")
      await writeFile(path.join(dir, ".npmrc"), `registry=${registryUrl}\n`)
    }),
  )

  const controller = new AbortController()
  let abortRequested = false
  try {
    await install(abortDir, {
      add: [{ name: "is-number", version: "7.0.0" }],
      force: true,
      signal: controller.signal,
      onEvent(event) {
        if (event.kind === "phase" && event.phase === "resolving") {
          abortRequested = true
          controller.abort()
        }
      },
    })
    throw new Error("cancelled install unexpectedly succeeded")
  } catch (error) {
    const structured = error as Error & { code?: string; diagnostic?: string }
    if (!abortRequested || structured.code !== "ERR_AUBE_INSTALL_CANCELLED") {
      throw error
    }
    if (!structured.diagnostic?.includes("install cancelled")) {
      throw new Error(`cancelled install omitted diagnostic: ${JSON.stringify(structured)}`)
    }
  }

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
    rm(abortDir, { recursive: true, force: true }),
  ])
}

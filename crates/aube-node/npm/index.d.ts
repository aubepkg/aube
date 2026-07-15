export type InstallEvent =
  | { kind: "phase"; phase: "resolving" | "fetching" | "linking" | "complete" }
  | {
      kind: "progress"
      resolved: number
      total: number
      reused: number
      downloaded: number
      downloadedBytes: number
      estimatedBytes?: number
    }
  | { kind: "output"; level: "info" | "warning" | "error"; code?: string; message: string }

export interface AubeError extends Error {
  code: string
  diagnostic: string
}

export type InstallInput = {
  add?: { name: string; version?: string; dev?: boolean }[]
  force?: boolean
  offline?: boolean
  onEvent?: (event: InstallEvent) => void
  signal?: AbortSignal
}
export type InstallResult = {
  projectDir: string
  added: string[]
  resolved: number
  reused: number
  downloaded: number
  durationMs: number
}
export function install(projectDir: string, input?: InstallInput): Promise<InstallResult>

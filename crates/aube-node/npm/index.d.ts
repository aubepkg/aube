export type InstallEvent = {
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
export type InstallInput = {
  add?: { name: string; version?: string }[]
  force?: boolean
  offline?: boolean
  onEvent?: (event: InstallEvent) => void
  signal?: AbortSignal
}
export type InstallResult = { projectDir: string; added: string[] }
export function install(projectDir: string, input?: InstallInput): Promise<InstallResult>

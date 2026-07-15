export type AubeNodeTarget = {
  os: "darwin" | "linux" | "win32"
  arch: "arm64" | "x64"
  libc?: "glibc" | "musl"
}

export type AubeNodeBunPlugin = {
  name: string
  setup(build: {
    onResolve(
      options: { filter: RegExp },
      callback: () => { path: string },
    ): void
  }): void
}

export function platformPackage(target: AubeNodeTarget): string
export function bunPlugin(target: AubeNodeTarget): AubeNodeBunPlugin

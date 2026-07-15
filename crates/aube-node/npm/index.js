const fs = require("node:fs")

function isMusl() {
  let musl = fs.existsSync("/etc/alpine-release")
  try { musl = !process.report.getReport().header.glibcVersionRuntime } catch (_) {}
  return musl
}

function load() {
  const key = `${process.platform}-${process.arch}`
  if (key === "darwin-arm64") return require("@jdxcode/aube-node-darwin-arm64")
  if (key === "darwin-x64") return require("@jdxcode/aube-node-darwin-x64")
  if (key === "win32-arm64") return require("@jdxcode/aube-node-win32-arm64")
  if (key === "win32-x64") return require("@jdxcode/aube-node-win32-x64")
  if (key === "linux-arm64" && isMusl()) return require("@jdxcode/aube-node-linux-arm64-musl")
  if (key === "linux-arm64") return require("@jdxcode/aube-node-linux-arm64")
  if (key === "linux-x64" && isMusl()) return require("@jdxcode/aube-node-linux-x64-musl")
  if (key === "linux-x64") return require("@jdxcode/aube-node-linux-x64")
  throw new Error(`Unsupported aube Node-API platform: ${key}`)
}

module.exports = load()

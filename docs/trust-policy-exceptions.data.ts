import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const TRUST_SOURCE_PATH = path.resolve(
  __dirname,
  '../crates/aube-resolver/src/trust.rs',
)

declare const data: string[]
export { data }

export default {
  watch: ['../crates/aube-resolver/src/trust.rs'],
  load(): string[] {
    const source = fs.readFileSync(TRUST_SOURCE_PATH, 'utf8')
    const block = source.match(
      /pub const DEFAULT_TRUST_POLICY_EXCLUDES:[\s\S]*?=\s*&\[(?<entries>[\s\S]*?)\];/,
    )?.groups?.entries

    if (!block) {
      throw new Error('could not find DEFAULT_TRUST_POLICY_EXCLUDES in trust.rs')
    }

    return [...block.matchAll(/^\s*"([^"]+)",\s*$/gm)]
      .map((match) => match[1])
      .sort((a, b) => a.localeCompare(b))
  },
}

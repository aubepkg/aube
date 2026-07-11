import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import path from 'node:path'
import { test } from 'node:test'

import { addDaysIso, todayIso } from './constants.mts'
import { checkDockerPrebake, checkPins } from './external-tools.mts'
import { DOCKER_PREBAKE, EXTERNAL_TOOLS_JSON, REPO_ROOT, RUST_TOOLCHAIN_TOML } from './paths.mts'

const GOOD_SRI =
  'sha512-waLrsPG2a7EOv0XuvXDQZGgCZ4MTtOfZh8TmGbM6gn2B6Nh6HI+15jaoKdAS9wgdTyIqTuqU+O+NtVYd+kuFaA=='

test('the repo external-tools.json passes checkPins', () => {
  const tools = JSON.parse(readFileSync(EXTERNAL_TOOLS_JSON, 'utf8')).tools
  assert.deepEqual(checkPins(tools), [])
})

test('checkPins flags missing pins, bad SRIs, and asset entries with no integrity', () => {
  assert.equal(checkPins({ a: {} }).length, 1)
  assert.equal(checkPins({ a: { version: '1.0.0', integrity: 'sha256-abc' } }).length, 1)
  assert.equal(checkPins({ a: { version: '1.0.0', release: 'asset' } }).length, 1)
})

test('checkPins validates soakBypass dates, arithmetic, and expiry', () => {
  const pub = addDaysIso(todayIso(), -1)
  const good = {
    a: {
      version: '1.0.0',
      integrity: GOOD_SRI,
      soakBypass: { version: '1.0.0', published: pub, removable: addDaysIso(pub, 7) },
    },
  }
  assert.deepEqual(checkPins(good), [])
  const wrongMath = structuredClone(good)
  wrongMath.a.soakBypass.removable = addDaysIso(pub, 3)
  assert.match(checkPins(wrongMath)[0]!, /removable/)
  const expired = structuredClone(good)
  expired.a.soakBypass = { version: '1.0.0', published: '2020-01-01', removable: '2020-01-08' }
  assert.match(checkPins(expired)[0]!, /expired/)
  const impossible = structuredClone(good)
  impossible.a.soakBypass = { version: '1.0.0', published: '2026-13-45', removable: '2026-13-52' }
  assert.match(checkPins(impossible)[0]!, /calendar/)
})

test('the repo Dockerfile prebake (when present) matches the tracked pins', t => {
  if (!DOCKER_PREBAKE || !existsSync(path.join(REPO_ROOT, DOCKER_PREBAKE))) {
    t.skip('repo has no prebake image')
    return
  }
  const tools = JSON.parse(readFileSync(EXTERNAL_TOOLS_JSON, 'utf8')).tools
  const docker = readFileSync(path.join(REPO_ROOT, DOCKER_PREBAKE), 'utf8')
  const toolchain = readFileSync(path.join(REPO_ROOT, RUST_TOOLCHAIN_TOML), 'utf8')
  assert.deepEqual(checkDockerPrebake(docker, tools, toolchain), [])
  // and drift in any direction is caught
  assert.ok(checkDockerPrebake(docker.replace(/sha=[0-9a-f]{8}/, 'sha=deadbeef'), tools, toolchain).length > 0)
  assert.ok(
    checkDockerPrebake(docker, tools, toolchain.replace(/channel = ".*"/, 'channel = "nightly-1999-01-01"')).length >
      0,
  )
})

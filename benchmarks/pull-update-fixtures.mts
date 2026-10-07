#!/usr/bin/env node
// Write the two commits the pull-update scenario moves between, as
// `before-package.json` and `after-package.json` in <outDir>: the benchmark
// fixture with ten direct dependencies pinned to an older and a newer exact
// version, and two dependencies the earlier commit had not added yet. Pins,
// as Renovate uses for applications, are what make the update move the
// lockfile at all; a range the older version still satisfies leaves every
// package manager's lockfile unchanged.
//
// Usage:
//   node benchmarks/pull-update-fixtures.mts <fixture.package.json> <outDir>

import { readFileSync, writeFileSync } from 'node:fs'

interface PackageJson {
  dependencies: Record<string, string>
}

const [fixturePath, outDir] = process.argv.slice(2)
if (!fixturePath || !outDir) {
  console.error('usage: node benchmarks/pull-update-fixtures.mts <fixture.package.json> <outDir>')
  process.exit(2)
}

// name: [previous commit, pulled commit]. Every version is inside the
// fixture's own range and past the 24h minimum release age.
const bumps: Record<string, [string, string]> = {
  '@babel/core': ['7.24.0', '7.29.7'],
  '@babel/preset-env': ['7.24.0', '7.29.7'],
  '@mui/material': ['5.15.7', '5.18.0'],
  axios: ['1.6.7', '1.20.0'],
  eslint: ['9.9.0', '9.39.5'],
  postcss: ['8.4.33', '8.5.28'],
  'react-router': ['6.22.0', '6.30.6'],
  'react-router-dom': ['6.22.0', '6.30.6'],
  typescript: ['5.4.5', '5.9.3'],
  webpack: ['5.90.1', '5.111.1'],
}
const added = ['zod', 'zustand']

const read = (): PackageJson => JSON.parse(readFileSync(fixturePath, 'utf8'))
const before = read()
const after = read()
for (const [name, [previous, pulled]] of Object.entries(bumps)) {
  before.dependencies[name] = previous
  after.dependencies[name] = pulled
}
for (const name of added) {
  delete before.dependencies[name]
}

writeFileSync(`${outDir}/before-package.json`, JSON.stringify(before, null, 2) + '\n')
writeFileSync(`${outDir}/after-package.json`, JSON.stringify(after, null, 2) + '\n')

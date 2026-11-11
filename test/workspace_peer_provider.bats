#!/usr/bin/env bats

# A registry package whose peer is provided by a `workspace:` dependency.
# See https://github.com/aubepkg/aube/discussions/1945.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_make_workspace_with_registry_peer_provider() {
	local protocol="${1:-workspace:*}" section="${2:-dependencies}" strict="${3:-true}"
	mkdir -p packages/app packages/react
	printf 'packages:\n  - packages/*\nstrictPeerDependencies: %s\nautoInstallPeers: false\n' "$strict" >pnpm-workspace.yaml
	echo '{"name":"root","private":true}' >package.json
	echo '{"name":"react","version":"18.2.0","main":"index.js"}' >packages/react/package.json
	echo "module.exports = 'local-react';" >packages/react/index.js
	local provider="\"react\":\"$protocol\"" registry='"use-sync-external-store":"1.2.0"'
	if [ "$section" = dependencies ]; then
		echo "{\"name\":\"app\",\"private\":true,\"dependencies\":{$provider,$registry}}" >packages/app/package.json
	else
		echo "{\"name\":\"app\",\"private\":true,\"$section\":{$provider},\"dependencies\":{$registry}}" >packages/app/package.json
	fi
}

# pnpm's own lockfile for the workspace above.
_write_pnpm_lockfile_with_linked_peer() {
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'

settings:
  autoInstallPeers: false
  excludeLinksFromLockfile: false

importers:

  .: {}

  packages/app:
    dependencies:
      react:
        specifier: workspace:*
        version: link:../react
      use-sync-external-store:
        specifier: 1.2.0
        version: 1.2.0(react@packages+react)

  packages/react: {}

packages:

  use-sync-external-store@1.2.0:
    resolution: {integrity: sha512-eEgnFxGQ1Ife9bzYs6VLi8/4X6CObHMw9Qr9tPY43iKwsPw8xE8+EFsf/2cFZ5S3esXgpWgtSCtLNS41F+sKPA==}
    peerDependencies:
      react: ^16.8.0 || ^17.0.0 || ^18.0.0

snapshots:

  use-sync-external-store@1.2.0(react@packages+react):
    dependencies:
      react: link:packages/react
EOF
}

_assert_registry_peer_resolves_to_workspace_package() {
	local consumer
	consumer="$(cd packages/app/node_modules/use-sync-external-store && pwd -P)"
	cd "$consumer" || exit
	run node -e "console.log(require('react'))"
	assert_success
	assert_output "local-react"
}

@test "aube install links a registry package's peer to a workspace: provider" {
	_make_workspace_with_registry_peer_provider

	run aube install
	assert_success
	run grep -A3 '^  use-sync-external-store@1.2.0(react@18.2.0):' aube-lock.yaml
	assert_output --partial 'react: link:packages/react'
	_assert_registry_peer_resolves_to_workspace_package
}

@test "aube install links a registry package's peer to a workspace:^ devDependency" {
	_make_workspace_with_registry_peer_provider 'workspace:^' devDependencies

	run aube install
	assert_success
	_assert_registry_peer_resolves_to_workspace_package
}

@test "aube install links a workspace: peer provider with strict-peer-dependencies off" {
	_make_workspace_with_registry_peer_provider workspace:\* dependencies false

	run aube install
	assert_success
	_assert_registry_peer_resolves_to_workspace_package
}

@test "aube install --frozen-lockfile keeps a workspace peer link from aube-lock.yaml" {
	_make_workspace_with_registry_peer_provider

	run aube install
	assert_success
	rm -rf node_modules packages/app/node_modules packages/react/node_modules

	run aube install --frozen-lockfile
	assert_success
	_assert_registry_peer_resolves_to_workspace_package
}

@test "aube install writes a registry package's workspace peer to pnpm-lock.yaml as link:" {
	_make_workspace_with_registry_peer_provider
	echo 'default-lockfile-format=pnpm' >.npmrc

	run aube install
	assert_success
	run grep -A3 '^  use-sync-external-store@1.2.0(react@18.2.0):' pnpm-lock.yaml
	assert_output --partial 'react: link:packages/react'
	_assert_registry_peer_resolves_to_workspace_package
}

@test "aube install --frozen-lockfile accepts pnpm's workspace peer link under strict peers" {
	_make_workspace_with_registry_peer_provider
	_write_pnpm_lockfile_with_linked_peer
	cp pnpm-lock.yaml pnpm-lock.yaml.orig

	run aube install --frozen-lockfile
	assert_success
	run cmp pnpm-lock.yaml pnpm-lock.yaml.orig
	assert_success
	_assert_registry_peer_resolves_to_workspace_package
}

@test "aube add keeps a registry package's workspace peer linked from pnpm's lockfile" {
	_make_workspace_with_registry_peer_provider
	_write_pnpm_lockfile_with_linked_peer

	cd packages/app
	run aube add is-odd@3.0.1
	assert_success
	cd ../..
	run grep 'react: link:packages/react' pnpm-lock.yaml
	assert_success
	_assert_registry_peer_resolves_to_workspace_package
}

var test = require('node:test');
var assert = require('node:assert/strict');
var fs = require('node:fs');
var os = require('node:os');
var path = require('node:path');
var spawnSync = require('node:child_process').spawnSync;

var childNpmEnv = require('./installArchSpecificPackage.js').childNpmEnv;

test('child npm env drops the outer allow-scripts policy', function() {
    var parentEnv = {
        npm_config_allow_scripts: '@endevco/aube',
        NPM_CONFIG_ALLOW_SCRIPTS: '@endevco/aube',
        npm_config_registry: 'https://registry.example.test',
        npm_config_global: 'true',
        NPM_CONFIG_GLOBAL: 'true',
    };

    var childEnv = childNpmEnv(parentEnv);

    assert.equal(childEnv.npm_config_allow_scripts, undefined);
    assert.equal(childEnv.NPM_CONFIG_ALLOW_SCRIPTS, undefined);
    assert.equal(childEnv.npm_config_global, 'false');
    assert.equal(childEnv.NPM_CONFIG_GLOBAL, undefined);
    assert.equal(childEnv.npm_config_registry, parentEnv.npm_config_registry);
    assert.equal(parentEnv.npm_config_allow_scripts, '@endevco/aube');
    assert.equal(parentEnv.npm_config_global, 'true');
    assert.equal(parentEnv.NPM_CONFIG_GLOBAL, 'true');
});

for (const platform of ['linux', 'win32']) {
    test('one platform binary supplies all commands on ' + platform, function(t) {
        var root = fs.mkdtempSync(path.join(os.tmpdir(), 'aube-npm-bin-'));
        t.after(function() { fs.rmSync(root, { recursive: true, force: true }); });

        var installerPath = path.join(root, 'installArchSpecificPackage.js');
        fs.copyFileSync(path.join(__dirname, 'installArchSpecificPackage.js'), installerPath);
        var packageDir = path.join(root, 'node_modules', '@endevco', 'aube-test');
        var packageBinDir = path.join(packageDir, 'bin');
        fs.mkdirSync(packageBinDir, { recursive: true });
        var suffix = platform === 'win32' ? '.exe' : '';
        var source = path.join(packageBinDir, 'aube' + suffix);
        var nativeWindowsBinary = platform === 'win32' && process.platform === 'win32' && process.env.AUBE_TEST_BINARY;
        if (nativeWindowsBinary) {
            fs.copyFileSync(path.resolve(process.env.AUBE_TEST_BINARY), source);
        } else {
            fs.writeFileSync(source, '#!/bin/sh\nprintf "%s" "${0##*/}"\n', { mode: 0o755 });
        }
        fs.writeFileSync(path.join(packageDir, 'package.json'), JSON.stringify({
            name: '@endevco/aube-test',
            bin: { aube: 'bin/aube' + suffix },
        }));

        require(installerPath).linkSubpkgBins('@endevco/aube-test', platform);
        for (var name of ['aube', 'aubr', 'aubx']) {
            var destination = path.join(root, 'bin', name + suffix);
            if (nativeWindowsBinary) {
                assert.equal(fs.statSync(destination).size, fs.statSync(source).size);
            } else {
                assert.equal(fs.readFileSync(destination, 'utf8'), fs.readFileSync(source, 'utf8'));
            }
            if (platform === 'win32') {
                assert.equal(fs.readFileSync(path.join(root, 'bin', name), 'utf8'), '#!' + destination.replace(/\\/g, '/') + '\n');
                if (nativeWindowsBinary) {
                    var help = spawnSync(destination, ['--help'], { encoding: 'utf8' });
                    assert.equal(help.status, 0, help.stderr);
                    var usage = name === 'aube' ? 'Usage: aube [FLAGS]' : name === 'aubr' ? 'Usage: aube run' : 'Usage: aube dlx';
                    assert.ok(help.stdout.includes(usage), help.stdout);
                }
            } else if (process.platform !== 'win32') {
                var result = spawnSync(destination, { encoding: 'utf8' });
                assert.equal(result.status, 0);
                assert.equal(result.stdout, name);
            }
        }
    });
}

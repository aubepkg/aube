#!/usr/bin/env bats
# A tarball body that the connection cuts short mid-stream is fetched again
# instead of failing the install. A python proxy in front of the test
# registry serves the first tarball request with a Content-Length it never
# finishes, then serves later requests whole.

setup() {
	load 'test_helper/common_setup'
	_common_setup
	command -v python3 >/dev/null 2>&1 || skip "python3 not available for the proxy"
	_start_flaky_proxy
}

teardown() {
	if [ -n "${PROXY_PID:-}" ]; then
		kill "$PROXY_PID" 2>/dev/null || true
	fi
	_common_teardown
}

_start_flaky_proxy() {
	local script="$TEST_TEMP_DIR/flaky_proxy.py"
	cat >"$script" <<'PY'
import http.server, json, sys, urllib.request

upstream = sys.argv[1].rstrip("/")
seen = set()

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        host = self.headers["Host"]
        with urllib.request.urlopen(urllib.request.Request(upstream + self.path)) as r:
            body = r.read()
            ctype = r.headers.get("Content-Type", "application/octet-stream")
        if self.path.endswith(".tgz"):
            first = self.path not in seen
            seen.add(self.path)
            self.send_response(200)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if first:
                self.wfile.write(body[: len(body) // 2])
                self.wfile.flush()
                self.close_connection = True
                self.connection.close()
                return
            self.wfile.write(body)
            return
        # Point tarball URLs back at this proxy.
        body = body.replace(upstream.encode(), ("http://" + host).encode())
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print("PORT", srv.server_address[1], flush=True)
srv.serve_forever()
PY
	local log="$TEST_TEMP_DIR/proxy.log"
	python3 -u "$script" "$AUBE_TEST_REGISTRY" >"$log" 2>&1 &
	PROXY_PID=$!
	local port=""
	for _ in $(seq 1 50); do
		port="$(sed -n 's/^PORT \([0-9]*\)$/\1/p' "$log" | head -1)"
		[ -n "$port" ] && break
		sleep 0.1
	done
	[ -n "$port" ] || skip "flaky proxy failed to start"
	echo "registry=http://127.0.0.1:$port/" >"$TEST_TEMP_DIR/.npmrc"
}

@test "install refetches a tarball whose body is cut off mid-stream" {
	cat >package.json <<-'JSON'
		{ "name": "stream-retry", "version": "0.0.0", "dependencies": { "is-odd": "3.0.1" } }
	JSON
	run aube install
	assert_success
	assert_output --partial "retrying"
	assert_dir_exists node_modules/is-odd
}

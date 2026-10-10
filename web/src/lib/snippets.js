// Pre-formatted code-block contents. These are injected with {@html} inside a
// <pre><code>, so: HTML metacharacters are entity-escaped (&lt; &gt; &amp;),
// comments are wrapped in <span class="cm">…</span>, a literal backslash is
// written \\ and a literal "\n" shown to the reader is written \\n.

export const buildCmd = `<span class="cm"># clone and build the whole workspace</span>
git clone https://github.com/artech-git/holon.git
cd holon
cargo build --workspace            <span class="cm"># produces target/debug/txpd and target/debug/txp</span>
cargo build --release --workspace  <span class="cm"># optimized build (debug=1 symbols retained)</span>`;

export const daemonStart = `sudo env TXP_DATA_DIR=/var/lib/txp TXP_SOCKET=/run/txpd.sock \\
     target/debug/txpd --socket-mode 0666 &amp;
export TXP_SOCKET=/run/txpd.sock`;

export const selfTest = `txp self-test`;

export const validateCmd = `txp validate examples/rebuild-and-publish.toml`;

export const runCmd = `txp run examples/rebuild-and-publish.toml   <span class="cm"># commits</span>
txp run examples/failing-build.toml         <span class="cm"># aborts; /srv/site untouched</span>`;

export const inspectCmd = `txp list                 <span class="cm"># all known transactions</span>
txp status &lt;txid&gt;         <span class="cm"># one transaction's phase + steps</span>
txp in-doubt             <span class="cm"># committed-but-not-done (needs recovery)</span>
txp checkpoint           <span class="cm"># snapshot + truncate WAL segments</span>
txp shutdown             <span class="cm"># graceful stop</span>`;

export const manifestExample = `<span class="cm"># the transaction itself</span>
[txn]
name = "rebuild-and-publish"
timeout = "10m"

<span class="cm"># a managed filesystem tree</span>
[[resource]]
id = "site"
kind = "fs.tree"
path = "/srv/site"
mode = "write"

<span class="cm"># run a build inside a sandbox, against a staged overlay of \`site\`</span>
[[step]]
id = "build"
kind = "process"
argv = ["make", "site"]
cwd = "/srv/site"
mounts = [{ resource = "site" }]   <span class="cm"># staged overlay appears at the real path</span>
network = "deny"                   <span class="cm"># the only option today</span>
timeout = "5m"

<span class="cm"># then write a file atomically with the build output</span>
[[step]]
id = "stamp"
kind = "fs.put"
resource = "site"
after = ["build"]
path = "DEPLOY"
content = "$txid\\n"`;

export const rebuildToml = `[txn]
name = "rebuild-and-publish"
timeout = "10m"

[[resource]]
id = "site"
kind = "fs.tree"
path = "/srv/site"
mode = "write"

[[resource]]
id = "log"
kind = "fs.tree"
path = "/srv/deploylog"
mode = "write"

[[step]]
id = "build"
kind = "process"
argv = ["/bin/sh", "-c", "set -e; mkdir -p dist; for f in src/*.md; do n=$(basename $f .md); printf '&lt;h1&gt;%s&lt;/h1&gt;\\n' \\"$(cat $f)\\" &gt; dist/$n.html; done; rm -rf old; echo built-by-$TXP_TXID &gt; dist/BUILD"]
cwd = "/srv/site"
mounts = [{ resource = "site" }]
network = "deny"
timeout = "5m"

[[step]]
id = "verify"
kind = "process"
argv = ["/bin/sh", "-c", "test -s dist/index.html &amp;&amp; grep -q built-by dist/BUILD"]
cwd = "/srv/site"
mounts = [{ resource = "site" }]
after = ["build"]

[[step]]
id = "stamp"
kind = "fs.put"
resource = "log"
path = "last-deploy.txt"
content = "txid=$txid\\n"
after = ["verify"]`;

export const failingToml = `[txn]
name = "failing-build"
timeout = "1m"

[[resource]]
id = "site"
kind = "fs.tree"
path = "/srv/site"

[[step]]
id = "build"
kind = "process"
argv = ["/bin/sh", "-c", "echo junk &gt; dist/junk.html; rm -rf src; exit 7"]
cwd = "/srv/site"
mounts = [{ resource = "site" }]`;

export const testBins = `cargo test --workspace          <span class="cm"># unit, property, WAL crash-sim, engine, crash-point harness</span>
./scripts/test-root.sh          <span class="cm"># root-only suites (builds as you, runs with sudo)</span>
./scripts/tlc.sh                <span class="cm"># TLA+ model check (needs ~/tla/tla2tools.jar + a JRE)</span>

<span class="cm"># crash every coordinator point with a sandboxed step present</span>
sudo target/debug/txp-crashtest run --with-process`;

export const tlcCmd = `scripts/tlc.sh     <span class="cm"># ≈ a few seconds for RM = {r1, r2, r3}</span>`;

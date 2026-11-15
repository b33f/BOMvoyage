/// UI smoke test: run the real, embedded app.js in node against a minimal
/// DOM + fetch stub, let init() execute end-to-end (offline), and assert on
/// the wiring — most notably that the detailed-mode vulnerable-deps console
/// preset exists exactly once (a double-insert once shipped: the preset was
/// both spliced into the array AND button-injected, and the wiring loop then
/// appended it again), and that detailed mode recolors normal dependencies
/// green. Run: node tests/ui_smoke.mjs
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { URL } from "node:url";

const src = readFileSync(new URL("../src/ui/app.js", import.meta.url), "utf8");

function makeEl(id) {
  const el = {
    id,
    textContent: "",
    innerHTML: "",
    hidden: false,
    value: "0",
    max: "0",
    checked: false,
    disabled: false,
    children: [],
    className: "",
    style: {},
    dataset: {},
    appendChild(c) {
      this.children.push(c);
    },
    insertBefore(c, ref) {
      const i = ref ? this.children.indexOf(ref) : this.children.length;
      this.children.splice(i < 0 ? this.children.length : i, 0, c);
    },
    addEventListener() {},
    removeEventListener() {},
    querySelectorAll() {
      return [];
    },
    querySelector() {
      return null;
    },
    remove() {},
  };
  const classes = new Set();
  el.classList = {
    toggle(c, force) {
      const on = force === undefined ? !classes.has(c) : !!force;
      if (on) classes.add(c);
      else classes.delete(c);
      el.className = [...classes].join(" ");
    },
    add(c) {
      this.toggle(c, true);
    },
    remove(c) {
      this.toggle(c, false);
    },
    contains(c) {
      return classes.has(c);
    },
  };
  return el;
}

/// Boot a fresh app.js world. `detailedPackages` feeds /api/meta so init()
/// sets the real flag through its normal path.
async function boot(detailedPackages) {
  const els = new Map();
  const el = (id) => {
    if (!els.has(id)) els.set(id, makeEl(id));
    return els.get(id);
  };
  const fetchJsonFor = (url) => {
    if (url.includes("api/meta"))
      return {
        name: "smoke",
        version: "1.2.3",
        total_packages: 0,
        detailed_packages: detailedPackages,
        ecosystems: [],
        scan_quality: "transitive",
        history_current: true,
        last_scanned_at: "2026-01-01T00:00:00Z",
        head_sha: "abc",
      };
    if (url.includes("api/commits")) return { rows: [] };
    if (url.includes("api/graph"))
      return { as_of: null, elements: { nodes: [], edges: [] } };
    return {};
  };
  const sandbox = {
    URL,
    performance: { now: () => 0 },
    setTimeout: () => 0,
    clearTimeout() {},
    setInterval: () => 0,
    clearInterval() {},
    fetch: async (url) => ({
      json: async () => fetchJsonFor(url),
    }),
    console,
    document: {
      getElementById: el,
      createElement: () => makeEl("tmp"),
      addEventListener() {},
      querySelectorAll() {
        return [];
      },
    },
  };
  sandbox.window = sandbox;
  vm.createContext(sandbox);
  vm.runInContext(src, sandbox, { filename: "app.js" });

  // init() is async: yield to the event loop until the wiring has happened
  // (or give up after ~50 turns and let the assertions report what ran).
  const holder = el("cypher-presets");
  for (let i = 0; i < 50 && holder.children.length === 0; i++) {
    await new Promise((r) => setImmediate(r));
  }
  return { sandbox, holder, el };
}

const vulnCount = (holder) =>
  holder.children.filter((b) =>
    String(b.innerHTML).includes("Vulnerable deps + who added them")
  ).length;

// --- world 1: a detailed graph -----------------------------------------
const { sandbox, holder, el } = await boot(5);

if (vulnCount(holder) !== 1)
  throw new Error(
    `expected exactly 1 vulnerable-deps preset button at load, found ${vulnCount(holder)}`
  );

// Re-invocation (the late-arrival path used by the history poll) must be a
// no-op — the guard, not the DOM, prevents a second insert.
sandbox.addVulnerablePreset();
if (vulnCount(holder) !== 1)
  throw new Error(
    `expected exactly 1 vulnerable-deps preset button after re-invoke, found ${vulnCount(holder)}`
  );

// Total preset count sanity: 8 built-ins + 1 detailed-mode preset.
if (holder.children.length !== 9)
  throw new Error(
    `expected 9 preset buttons, found ${holder.children.length}`
  );

// Detailed mode recolors packages green so the red triangles pop — and the
// scheme is universal: undetailed graphs are green too.
const fakeNode = (data) => ({ data: (key) => (key === undefined ? data : data[key]) });
const green = sandbox.nodeColor(fakeNode({ ecosystem: "python" }));
if (green !== "#3fb950")
  throw new Error(`package nodes must be green, got ${green}`);

// --- world 2: an undetailed graph --------------------------------------
const plain = await boot(0);

if (vulnCount(plain.holder) !== 0)
  throw new Error("undetailed graph must not offer the vulnerable-deps preset");
if (plain.holder.children.length !== 8)
  throw new Error(
    `expected 8 preset buttons without detailed data, found ${plain.holder.children.length}`
  );
const plainGreen = plain.sandbox.nodeColor(fakeNode({ ecosystem: "python" }));
if (plainGreen !== "#3fb950")
  throw new Error(`package nodes are green in every mode, got ${plainGreen}`);

// The version chip renders in both worlds, subtly, from /api/meta.
const checkVersion = (el, label) => {
  const chip = el("version");
  if (chip.hidden || chip.textContent !== "v1.2.3")
    throw new Error(
      `${label}: version chip must show v1.2.3, got hidden=${chip.hidden} "${chip.textContent}"`
    );
};
checkVersion(el, "detailed");
checkVersion(plain.el, "plain");

// --- diffElements: the basis of smooth time travel ---------------------
const els = (nodes, edges) => ({ nodes, edges });
const nd = (id) => ({ data: { id, label: id } });
const eg = (s, t, k = "lockfile") => ({ data: { source: s, target: t, source_kind: k } });

// identical sets → unchanged
{
  const d = sandbox.diffElements(els([nd("a"), nd("b")], [eg("a", "b")]), els([nd("a"), nd("b")], [eg("a", "b")]));
  if (!d.unchanged) throw new Error("identical element sets must be unchanged");
  if (d.keptNodeIds.length !== 2) throw new Error("both nodes kept");
}
// node + edge added
{
  const d = sandbox.diffElements(els([nd("a")], []), els([nd("a"), nd("b")], [eg("a", "b")]));
  if (d.unchanged || d.addedNodes.length !== 1 || d.addedNodes[0].data.id !== "b")
    throw new Error("new node must be an addition");
  if (d.addedEdges.length !== 1) throw new Error("new edge must be an addition");
  if (d.keptNodeIds.length !== 1 || d.keptNodeIds[0] !== "a") throw new Error("a must persist");
}
// node removed (and its edge with it)
{
  const d = sandbox.diffElements(els([nd("a"), nd("b")], [eg("a", "b")]), els([nd("a")], []));
  if (d.removedNodeIds.length !== 1 || d.removedNodeIds[0] !== "b")
    throw new Error("b must be removed");
  if (d.removedEdgeKeys.length !== 1) throw new Error("b's edge must be removed");
}
// same endpoints, different edge kind = a different edge (not conflated)
{
  const d = sandbox.diffElements(els([nd("a"), nd("b")], [eg("a", "b", "lockfile")]), els([nd("a"), nd("b")], [eg("a", "b", "attribution")]));
  if (d.unchanged || d.addedEdges.length !== 1 || d.removedEdgeKeys.length !== 1)
    throw new Error("edge identity must include source_kind");
}

// --- layoutOptions: seeded (drift) vs first-render ----------------------
{
  const seeded = sandbox.layoutOptions(10, true);
  if (seeded.randomize !== false || seeded.animate !== false)
    throw new Error(`seeded layout must be silent + seeded (drift is manual): ${JSON.stringify(seeded)}`);
  const fresh = sandbox.layoutOptions(10);
  if (fresh.randomize !== true || fresh.animate !== false)
    throw new Error(`first-render layout must randomize, unanimated: ${JSON.stringify(fresh)}`);
}

// version-bump detection: the purl base (everything before the version)
{
  const cases = [
    ["pkg:cargo/serde@1.0.1", "pkg:cargo/serde"],
    ["pkg:npm/@types/node@20.1.0", "pkg:npm/@types/node"], // last @ wins, not the scope's
    ["pkg:maven/junit/junit@4.12", "pkg:maven/junit/junit"],
    ["pkg:cargo/serde@", "pkg:cargo/serde"], // version-less manifest dep
    ["dev:Zhen", null], // hubs have no version part
    ["n42", null], // query node ids
  ];
  for (const [id, want] of cases) {
    const got = sandbox.baseOf(id);
    if (got !== want) throw new Error(`baseOf(${id}) = ${got}, want ${want}`);
  }
}

// 4× toggle flips the button state (duration scaling is covered by the
// browser checks — the drift math needs a live canvas)
{
  sandbox.toggleRate();
  if (!sandbox.document.getElementById("rate-btn").className.includes("active"))
    throw new Error("rate button must be flagged active at 4×");
  sandbox.toggleRate();
  if (sandbox.document.getElementById("rate-btn").className.includes("active"))
    throw new Error("rate button must not be active at 1×");
}

// --- relaxTargets: the drift-target relaxation --------------------------
{
  // an edge spanning 600px relaxes toward the IDEAL length (70px) — from
  // BOTH ends, and the midpoint stays put (symmetric)
  const out = sandbox.relaxTargets(
    [
      { id: "a", x: -300, y: 0 },
      { id: "b", x: 300, y: 0 },
    ],
    [{ source: "a", target: "b" }],
    150
  );
  const d = Math.hypot(out.get("a").x - out.get("b").x, out.get("a").y - out.get("b").y);
  if (d > 300 || d < 20) throw new Error(`relaxation should pull the pair toward its ideal length, got ${Math.round(d)}px`);
  const mid = (out.get("a").x + out.get("b").x) / 2;
  if (Math.abs(mid) > 60) throw new Error(`symmetric pair must stay centered, midpoint ${Math.round(mid)}`);
  // locked nodes never move
  const out2 = sandbox.relaxTargets(
    [
      { id: "a", x: -300, y: 0, locked: true },
      { id: "b", x: 300, y: 0 },
    ],
    [{ source: "a", target: "b" }],
    150
  );
  if (out2.get("a").x !== -300 || out2.get("a").y !== 0)
    throw new Error("locked anchor must not move");
}

// --- wasm-backend.js: the static-site backend contract -------------------
{
  const src2 = readFileSync(new URL("../src/ui/wasm-backend.js", import.meta.url), "utf8");
  // a fake engine whose execute() returns canned wire envelopes keyed by
  // distinctive query substrings — no WASM, no network
  const envelope = (columns, rows) => JSON.stringify({ columns, rows, error: null });
  const fake = () => ({
    execute(query) {
      if (query.includes("max(c.date)")) return envelope(["d"], [["2024-03-01T00:00:00Z"]]);
      if (query.includes("count(DISTINCT")) return envelope(["eco", "n"], [["java", 5]]);
      if (query.includes("depsdev_status IS NOT NULL")) return envelope(["n"], [[3]]);
      if (query.includes("count(p)")) return envelope(["n"], [[7]]);
      if (query.includes("STARTS WITH $sha")) return envelope(["date"], [["2024-01-01T00:00:00Z"]]);
      if (query.includes("p.vulns AS vulns"))
        return envelope(
          ["purl", "name", "version", "ecosystem", "scope", "manifest", "added_by",
           "added_date", "requirement", "vulns", "license", "depsdev_url", "homepage",
           "repository", "advisories"],
          [[
            "pkg:maven/junit/junit@4.12", "junit", "4.12", "java", "direct", "pom.xml",
            "Ada", "2024-01-01", "4.12", 2, "MIT", "https://deps.dev/x", "", "",
            '[{"id":"GHSA-1","aliases":["CVE-1"]}]',
          ]]
        );
      if (query.includes("DEPENDS_ON"))
        return envelope(["from", "to", "source"], [["a", "b", "lockfile"]]);
      if (query.includes("r.symbol AS symbol")) return envelope(["file", "symbol"], [["Main.java", "junit.framework"]]);
      if (query.includes("added_by_name AS added_by_name")) return envelope(["purl", "added_by_name"], [["p", "Ada"]]);
      if (query.includes("c.author_name AS author")) return envelope(["sha", "date", "summary", "author"], [["s1", "2024-01-01", "m", "Ada"]]);
      if (query.includes("MATCH (n)") || query.includes("MATCH (x")) {
        // raw-console query returning a node cell (wire format) + stub case
        return envelope(["n"], [[{ _labels: ["Package"], purl: "p1", name: "p1", version: "1" }]]);
      }
      return envelope([], []);
    },
  });

  // boot the backend with the fake engine + a fake snapshot fetch
  const sb = sandbox; // reuse world 1 (detailed)
  // The generated queries.js would provide this; the fake db matches on
  // the same distinctive substrings the real constants carry.
  sb.window.BOMVOYAGE_QUERIES = {
    META: "META", META_COUNTS: "count(DISTINCT", META_TOTAL: "count(p)",
    META_DETAILED: "depsdev_status IS NOT NULL", LIVE_PACKAGES: "p.vulns AS vulns",
    LIVE_DEPENDS_ON: "DEPENDS_ON", HISTORY: "added_by_name AS added_by_name",
    USAGE: "r.symbol AS symbol", COMMITS: "c.author_name AS author",
    SHA_DATE: "STARTS WITH $sha", MERGE_PACKAGES: "MERGE (p:Package",
  };
  sb.window.BOMVOYAGE_RELATE_FACTORY = fake;
  const origFetch = sb.fetch;
  sb.fetch = async (url) => ({
    json: async () => (String(url).includes("snapshot.json")
      ? { format: 1, name: "demo", tool_version: "0.2.1", stages: { packages: [{ purl: "p1", name: "p1", eco: "java", version: "1", scope: "direct" }] } }
      : origFetch(url).then((r) => r.json())),
  });
  vm.runInContext(src2, sandbox, { filename: "wasm-backend.js" }); // boots immediately
  await sb.window.BOMVOYAGE_STATIC.ready;

  const meta = await sb.window.BOMVOYAGE_STATIC.fetchJson("api/meta");
  if (meta.history_current !== true) throw new Error("static meta: history_current must always be true");
  if (meta.name !== "demo" || meta.detailed_packages !== 3 || meta.total_packages !== 7)
    throw new Error(`static meta shape wrong: ${JSON.stringify(meta)}`);

  const g = await sb.window.BOMVOYAGE_STATIC.fetchJson("api/graph?as_of=2024-01-15");
  const node = g.elements.nodes[0].data;
  if (node.id !== "pkg:maven/junit/junit@4.12" || node.label !== "junit@4.12")
    throw new Error(`static graph node shape wrong: ${JSON.stringify(node)}`);
  if (node.vulns !== 2 || !Array.isArray(node.advisories) || node.advisories[0].id !== "GHSA-1")
    throw new Error(`static graph advisories must be a parsed array: ${JSON.stringify(node.advisories)}`);
  if (g.as_of !== "2024-01-15T23:59:59Z") throw new Error(`as-of date expansion wrong: ${g.as_of}`);

  const upd = await sb.window.BOMVOYAGE_STATIC.fetchJson("api/update");
  if (upd.available !== false || upd.checking !== false) throw new Error("static update shim wrong");

  let refusedMessage = null;
  try {
    await sb.window.BOMVOYAGE_STATIC.fetchJson("api/query", {
      method: "POST",
      body: JSON.stringify({ cypher: "DELETE (n)" }),
    });
  } catch (e) {
    refusedMessage = e.message;
  }
  if (!refusedMessage || !refusedMessage.includes("read-only endpoint"))
    throw new Error(`static query console must refuse writes: ${refusedMessage}`);

  const q = await sb.window.BOMVOYAGE_STATIC.fetchJson("api/query", {
    method: "POST",
    body: JSON.stringify({ cypher: "MATCH (x:Package) RETURN x" }),
  });
  if (!q.elements || q.elements.nodes.length !== 1 || q.elements.nodes[0].data.kind !== "Package")
    throw new Error(`static console elements wrong: ${JSON.stringify(q.elements)}`);

  let threw = false;
  try {
    await sb.window.BOMVOYAGE_STATIC.fetchJson("api/nope");
  } catch (e) {
    threw = true;
  }
  if (!threw) throw new Error("unknown endpoints must surface as errors");
}


// --- ondemand.js: classify / parseManifest / scanRepo -------------------
{
  const src3 = readFileSync(new URL("../src/ui/ondemand.js", import.meta.url), "utf8");
  if (!sandbox.window.BOMVOYAGE_ONDEMAND) {
    vm.runInContext(src3, sandbox, { filename: "ondemand.js" }); // wires the box + registers
  }
  const od = sandbox.window.BOMVOYAGE_ONDEMAND;
  if (!od) throw new Error("ondemand.js must register BOMVOYAGE_ONDEMAND in static mode");

  // classification mirrors the Rust filename table (+ skip dirs)
  if (od.classify("Cargo.toml") !== "cargo-toml") throw new Error("classify cargo");
  if (od.classify("sub/pom.xml") !== "pom") throw new Error("classify pom");
  if (od.classify("requirements-dev.txt") !== "requirements") throw new Error("classify requirements");
  if (od.classify("node_modules/x/package.json") !== null) throw new Error("skip dirs must apply");
  if (od.classify("random.txt") !== null) throw new Error("non-manifests are none");

  // parsers: current-only, name+version+scope+eco
  const pj = od.parseManifest("package-json", JSON.stringify({
    dependencies: { express: "^4.18.2" }, devDependencies: { jest: "^29" },
  }));
  if (pj.length !== 2 || pj[0].name !== "express" || pj[0].scope !== "direct") throw new Error("package.json parse wrong");
  const lock = od.parseManifest("cargo-lock", "[[package]]\nname = \"libc\"\nversion = \"0.2.1\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.1\"\n");
  if (lock.length !== 2 || lock[0].scope !== "transitive" || lock[0].version !== "0.2.1") throw new Error("cargo.lock parse wrong");
  const gomod = od.parseManifest("go-mod", "require (\n\tgithub.com/x/y v1.2.3\n\tgithub.com/a/b v0.1.0 // indirect\n)\n");
  if (gomod.length !== 2 || gomod[0].scope !== "direct" || gomod[1].scope !== "transitive") throw new Error("go.mod parse wrong");
  const req = od.parseManifest("requirements", "flask==3.0.0\n# comment\n-r other.txt\nrequests\n");
  if (req.length !== 2 || req[0].version !== "3.0.0" || req[1].version !== "") throw new Error("requirements parse wrong");

  // purls: npm scope + maven namespace
  if (od.purlFor("npm", "@types/node", "20.1.0") !== "pkg:npm/@types/node@20.1.0") throw new Error("npm purl wrong");
  if (od.purlFor("maven", "junit:junit", "4.12") !== "pkg:maven/junit/junit@4.12") throw new Error("maven purl wrong");

  // spec validation
  if (od.validSpec("neo4j/neo4j-java-driver") !== true) throw new Error("valid spec rejected");
  if (od.validSpec("a/b/c") || od.validSpec("../x") || od.validSpec("a/b; x")) throw new Error("invalid spec accepted");

  // scanRepo against a stubbed fetch (tree + contents + HEAD sha)
  const treeJson = JSON.stringify({
    tree: [
      { type: "blob", path: "pom.xml" },
      { type: "blob", path: "lib/package.json" },
      { type: "blob", path: "node_modules/junk.json" },
    ],
    truncated: false,
  });
  const pomText = '<project><dependencies><dependency><groupId>junit</groupId><artifactId>junit</artifactId><version>4.12</version></dependency></dependencies></project>';
  sandbox.fetch = async (url) => ({
    ok: true, status: 200,
    json: async () => (String(url).includes("/git/trees/") ? JSON.parse(treeJson)
      : String(url).includes("/commits/HEAD") ? { sha: "deadbeef" } : Promise.reject(new Error("unexpected " + url))),
    text: async () => (String(url).includes("pom.xml") ? pomText
      : JSON.stringify({ dependencies: { lodash: "4.17.21" } })),
  });
  const result = await od.scanRepo("neo4j/neo4j-java-driver");
  const purls = result.stages.packages.map((p) => p.purl);
  if (!purls.includes("pkg:maven/junit/junit@4.12")) throw new Error("maven dep missing from stages");
  if (!purls.includes("pkg:npm/lodash@4.17.21")) throw new Error("npm dep missing from stages");
  if (!result.stages.packages.some((p) => p.eco === "generic" && p.purl.startsWith("pkg:generic/")))
    throw new Error("root package missing");
  if (!result.stages.declares.every((d) => d.since_date === "1970-01-01T00:00:00Z"))
    throw new Error("current-only declares must be always-live");
}

console.log("ui smoke: ok — preset exactly once (detailed only); packages green in all modes; version chip renders; diff + seeded-layout verified");

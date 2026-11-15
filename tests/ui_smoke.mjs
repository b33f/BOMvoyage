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
  return {
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
    classList: {
      toggle() {},
      add() {},
      remove() {},
      contains() {
        return false;
      },
    },
  };
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
    if (url.includes("/api/meta"))
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
    if (url.includes("/api/commits")) return { rows: [] };
    if (url.includes("/api/graph"))
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

// --- layoutOptions: seeded vs first-render ------------------------------
{
  const seeded = sandbox.layoutOptions(10, true);
  if (seeded.randomize !== false || seeded.animate === false)
    throw new Error(`seeded layout must not randomize and must animate: ${JSON.stringify(seeded)}`);
  const fresh = sandbox.layoutOptions(10);
  if (fresh.randomize !== true || fresh.animate !== false)
    throw new Error(`first-render layout must randomize, unanimated: ${JSON.stringify(fresh)}`);
}

console.log("ui smoke: ok — preset exactly once (detailed only); packages green in all modes; version chip renders; diff + seeded-layout verified");

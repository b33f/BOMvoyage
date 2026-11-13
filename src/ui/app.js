/* BOMvoyage: interactive graph + time slider + inspector + raw Cypher. */
"use strict";

const ECOSYSTEM_COLORS = {
  python: "#4b8bbe",
  java: "#e76f00",
  rust: "#dea584",
  go: "#00add8",
  generic: "#8b949e",
};

let cy = null;
let commits = [];            // [{sha, date, summary, author}] oldest-first
let selectedPackage = null;  // purl
let showTransitive = null;   // null = auto-decide on the first load
let lastElements = null;     // full as-of element set from /api/graph
let view = "devs";           // 'devs' (default) | 'areas' | 'deps'
let queryElements = null;    // raw-Cypher result shown in the main canvas
const DIRECT_ONLY_THRESHOLD = 20;

async function fetchJson(url, opts) {
  const res = await fetch(url, opts);
  const body = await res.json();
  if (body.error) throw new Error(body.error);
  return body;
}

function asOfValue() {
  // The slider picks a commit; the date picker overrides.
  const date = document.getElementById("datepick").value;
  if (date) return date;
  const idx = Number(document.getElementById("slider").value);
  if (commits.length === 0 || idx >= commits.length - 1) return null; // HEAD
  return commits[idx].sha;
}

function asOfLabel(asOf) {
  return asOf ?? "HEAD";
}

function setAsOfLabel(asOf) {
  const label = asOfLabel(asOf);
  const commit = commits.find((c) => c.sha === asOf);
  document.getElementById("asof-label").textContent = commit
    ? `${commit.sha.slice(0, 10)} · ${commit.date.slice(0, 10)} · ${commit.summary}`
    : label;

  // The banner is the unmissable time-travel signal: gold when viewing the
  // past, absent at HEAD.
  const banner = document.getElementById("travel-banner");
  if (!commit) {
    banner.hidden = true;
    banner.textContent = "";
  } else {
    banner.hidden = false;
    banner.textContent =
      `⏱ TIME TRAVEL — viewing dependencies as of ${commit.date.slice(0, 10)} ` +
      `(commit ${commit.sha.slice(0, 10)}: ${commit.summary}) — press "reset to HEAD" to return`;
  }
}

/// Fit all elements, but never zoom a small graph into absurdity.
function fitReasonably() {
  cy.fit(undefined, 40);
  if (cy.zoom() > 1.2) {
    cy.zoom(1.2);
    cy.center();
  }
}

function visibleElements(elements) {
  const nodes = elements.nodes.map((n) => ({
    data: Object.assign({ type: "package" }, n.data),
  }));
  if (showTransitive) return { nodes, edges: elements.edges };
  const direct = nodes.filter((n) => n.data.scope === "direct");
  const ids = new Set(direct.map((n) => n.data.id));
  const edges = elements.edges.filter(
    (e) => ids.has(e.data.source) && ids.has(e.data.target)
  );
  return { nodes: direct, edges };
}

/// Repo sub-area of one manifest path: its directory ("(root)" for top-level).
function areaOf(manifest) {
  const dir = manifest.includes("/") ? manifest.slice(0, manifest.lastIndexOf("/")) : "";
  return dir || "(root)";
}

/// Build the elements for the active view from the as-of package set.
/// All three views share the package nodes (scope-filtered); they differ in
/// what the packages are linked to:
///   devs  — the developer who added each package (bipartite person→package)
///   areas — the repo sub-area whose manifest declares each package
///   deps  — the DEPENDS_ON edges from the lockfile graph
function buildViewElements(elements) {
  if (view === "deps") return elements;
  const groupOf = view === "devs"
    ? (d) => d.added_by || "(unknown)"
    : (d) => areaOf(d.manifest || "");
  const prefix = view === "devs" ? "dev:" : "area:";

  const nodes = [];
  const edges = [];
  const groups = new Map(); // group name -> node
  for (const n of elements.nodes) {
    const group = groupOf(n.data);
    if (!groups.has(group)) {
      groups.set(group, {
        data: {
          id: prefix + group,
          label: group,
          type: prefix.slice(0, -1),
          ecosystem: view === "devs" ? "person" : "area",
        },
      });
    }
    edges.push({
      data: {
        source: prefix + group,
        target: n.data.id,
        source_kind: "attribution",
      },
    });
    nodes.push(n);
  }
  return { nodes: [...groups.values()].concat(nodes), edges };
}

function buildGraph(elements) {
  if (!cy) {
    cy = cytoscape({
      container: document.getElementById("cy"),
      elements,
      style: [
        {
          selector: "node",
          style: {
            "background-color": (n) => ECOSYSTEM_COLORS[n.data("ecosystem")] || "#8b949e",
            label: "data(label)",
            color: "#e6e8eb",
            "font-size": 9,
            "text-valign": "bottom",
            "text-margin-y": 4,
            width: 18,
            height: 18,
          },
        },
        {
          selector: "node[scope = 'transitive']",
          style: { width: 12, height: 12, "border-width": 1, "border-color": "#2a3038" },
        },
        {
          selector: "node[type = 'dev']",
          style: {
            shape: "round-rectangle",
            width: "label",
            height: 22,
            "background-color": "#4c8dff",
            "font-size": 11,
            "text-valign": "center",
            "text-margin-y": 0,
            padding: "6px",
          },
        },
        {
          selector: "node[type = 'area']",
          style: {
            shape: "round-rectangle",
            width: "label",
            height: 22,
            "background-color": "#7a5cff",
            "font-size": 10,
            "text-valign": "center",
            "text-margin-y": 0,
            padding: "6px",
          },
        },
        {
          selector: "edge",
          style: {
            width: 1.5,
            "line-color": "#2a3038",
            "target-arrow-color": "#2a3038",
            "target-arrow-shape": "triangle",
            "curve-style": "bezier",
          },
        },
        {
          selector: "edge[source_kind = 'synthesized']",
          style: { "line-style": "dashed" },
        },
        {
          selector: "edge[source_kind = 'attribution']",
          style: { "line-color": "rgba(76, 141, 255, 0.55)", "target-arrow-color": "rgba(76, 141, 255, 0.55)" },
        },
      ],
      layout: { name: "cose", animate: false, randomize: false },
      wheelSensitivity: 0.2,
    });
    fitReasonably();
    cy.on("tap", "node[type]", (evt) => {
      if (evt.target.data("type") === "package") showPackage(evt.target.data("id"));
    });
  } else {
    cy.elements().remove();
    cy.add(elements);
    cy.layout({ name: "cose", animate: false }).run();
    fitReasonably();
  }
}

async function refresh() {
  const asOf = asOfValue();
  setAsOfLabel(asOf);
  const q = asOf ? `?as_of=${encodeURIComponent(asOf)}` : "";
  try {
    const payload = await fetchJson(`/api/graph${q}`);
    lastElements = payload.elements;
    // Auto-decide once: big graphs start direct-only.
    if (showTransitive === null) {
      showTransitive = lastElements.nodes.length <= DIRECT_ONLY_THRESHOLD;
    }
    updateScopeToggle();
    renderCurrentView();
    if (selectedPackage) showPackage(selectedPackage, asOf);
  } catch (e) {
    document.getElementById("asof-label").textContent = `error: ${e.message}`;
  }
}

/// The main canvas shows the active view — or, while one is active, the raw
/// query result.
function renderCurrentView() {
  if (queryElements) {
    buildGraph(queryElements);
    return;
  }
  buildGraph(buildViewElements(visibleElements(lastElements)));
}

function clearQueryResult() {
  queryElements = null;
  document.getElementById("clear-query").hidden = true;
  renderCurrentView();
}

function updateScopeToggle() {
  const btn = document.getElementById("scope-toggle");
  if (!btn || !lastElements) return;
  const total = lastElements.nodes.length;
  const direct = lastElements.nodes.filter((n) => n.data.scope === "direct").length;
  btn.textContent = showTransitive
    ? `all dependencies (${total}) — direct only`
    : `direct dependencies (${direct}) — show all (${total})`;
}

function toggleScope() {
  showTransitive = !showTransitive;
  updateScopeToggle();
  renderCurrentView();
}

async function showPackage(purl, asOfOverride) {
  selectedPackage = purl;
  const details = document.getElementById("pkg-details");
  details.innerHTML = "<em>loading…</em>";
  const asOf = asOfOverride ?? asOfValue();
  const suffix = asOf ? `&as_of=${encodeURIComponent(asOf)}` : "";
  try {
    const [history, usage] = await Promise.all([
      fetchJson(`/api/history?name=${encodeURIComponent(purl)}${suffix}`),
      fetchJson(`/api/usage?purl=${encodeURIComponent(purl)}`),
    ]);
    const rows = history.rows || [];
    const current = rows.filter((r) => !r.removed_date);
    const latest = current[current.length - 1] || rows[rows.length - 1] || {};
    const usages = (usage.rows || []);

    const timeline = rows
      .map(
        (r) =>
          `<li><code>${r.version || r.requirement || "?"}</code> — added ${fmtDate(r.added_date)} by ${escapeHtml(r.added_by_name || "?")}` +
          (r.removed_date ? ` · removed ${fmtDate(r.removed_date)}` : " · <strong>live</strong>") +
          `</li>`
      )
      .join("");

    details.innerHTML = `
      <dt>purl</dt><dd>${escapeHtml(purl)}</dd>
      <dt>requirement</dt><dd>${escapeHtml(latest.requirement || "?")} in ${escapeHtml(latest.manifest || "?")}</dd>
      <dt>added by</dt><dd>${escapeHtml(latest.added_by_name || "?")} (${escapeHtml(latest.added_by_email || "?")}) · ${fmtDate(latest.added_date)}</dd>
      <dt>version timeline</dt><dd><ul>${timeline}</ul></dd>
      <dt>used in ${usages.length} file${usages.length === 1 ? "" : "s"}</dt>
      <dd><ul>${usages.map((u) => `<li><code>${escapeHtml(u.file)}</code> <em>${escapeHtml(u.symbol)}</em></li>`).join("") || "<em>no usage sites found</em>"}</ul></dd>
    `;
  } catch (e) {
    details.innerHTML = `<em>${escapeHtml(e.message)}</em>`;
  }
}

function fmtDate(iso) {
  return (iso || "?").slice(0, 10);
}

function escapeHtml(s) {
  const div = document.createElement("div");
  div.textContent = String(s);
  return div.innerHTML;
}

async function init() {
  const meta = await fetchJson("/api/meta");
  const badges = (meta.ecosystems || [])
    .map((e) => `<span class="badge">${e.ecosystem}: ${e.names} deps</span>`)
    .join("");
  document.getElementById("title").textContent = `${meta.name} — BOMvoyage`;
  document.getElementById("meta").innerHTML =
    `${meta.total_packages} package versions · last scanned ${fmtDate(meta.last_scanned_at)} ` +
    `· resolution: <strong>${meta.scan_quality}</strong> ${badges}`;

  const commitPayload = await fetchJson("/api/commits");
  commits = commitPayload.rows || []; // ascending by date: slider left = oldest, right = HEAD
  const slider = document.getElementById("slider");
  slider.max = Math.max(commits.length - 1, 0);
  slider.value = slider.max;
  slider.addEventListener("input", () => {
    document.getElementById("datepick").value = "";
    refresh();
  });
  document.getElementById("datepick").addEventListener("change", refresh);
  document.getElementById("reset-btn").addEventListener("click", () => {
    document.getElementById("datepick").value = "";
    slider.value = slider.max;
    refresh();
  });
  document.getElementById("run-cypher").addEventListener("click", runCypher);
  document.getElementById("scope-toggle").addEventListener("click", toggleScope);
  document.getElementById("clear-query").addEventListener("click", clearQueryResult);
  for (const btn of document.querySelectorAll("[data-view]")) {
    btn.addEventListener("click", () => setView(btn.dataset.view));
  }
  wireCypherPresets();

  await refresh();

  // The current SBOM is already on screen; poll until the background history
  // walk completes, then light up time travel.
  pollHistoryStatus(meta.history_current);
}

function pollHistoryStatus(current) {
  if (current) {
    document.getElementById("meta").dataset.history = "ready";
    return;
  }
  const badge = document.createElement("span");
  badge.className = "badge";
  badge.id = "history-badge";
  badge.textContent = "history loading… time travel not ready yet";
  document.getElementById("meta").appendChild(badge);
  const timer = setInterval(async () => {
    try {
      const meta = await fetchJson("/api/meta");
      if (meta.history_current) {
        clearInterval(timer);
        badge.textContent = "time travel ready";
        badge.remove();
        const commitPayload = await fetchJson("/api/commits");
        commits = commitPayload.rows || [];
        const slider = document.getElementById("slider");
        slider.max = Math.max(commits.length - 1, 0);
        slider.value = slider.max;
        refresh();
      }
    } catch (_) {
      // server momentarily busy — keep polling
    }
  }, 2000);
}

const CYPHER_PRESETS = [
  {
    name: "most-used packages",
    cypher:
      "MATCH (f:File)-[r:IMPORTS]->(p:Package)\nRETURN p.name AS package, count(DISTINCT f) AS files\nORDER BY files DESC LIMIT 10",
  },
  {
    name: "who added the most",
    cypher:
      "MATCH (x:Person)-[r:ADDED]->(p:Package)\nRETURN x.name AS developer, count(DISTINCT p) AS added\nORDER BY added DESC LIMIT 10",
  },
  {
    name: "recently added",
    cypher:
      "MATCH (m:Manifest)-[r:DECLARES]->(p:Package)\nWHERE r.removed_date IS NULL\nRETURN p.purl AS purl, r.since_date AS added, r.added_by_name AS by\nORDER BY r.since_date DESC LIMIT 10",
  },
  {
    name: "duplicate versions",
    cypher:
      "MATCH (p:Package)\nWITH p.name AS name, count(*) AS versions, collect(p.version) AS vs\nWHERE versions > 1\nRETURN name, versions, vs ORDER BY versions DESC",
  },
  {
    name: "removed dependencies",
    cypher:
      "MATCH (m:Manifest)-[r:DECLARES]->(p:Package)\nWHERE r.removed_date IS NOT NULL\nRETURN DISTINCT p.purl AS purl, r.removed_date AS removed, r.added_by_name AS added_by\nORDER BY removed DESC LIMIT 10",
  },
];

function wireCypherPresets() {
  const holder = document.getElementById("cypher-presets");
  for (const preset of CYPHER_PRESETS) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.textContent = preset.name;
    btn.addEventListener("click", () => {
      document.getElementById("cypher-input").value = preset.cypher;
      runCypher();
    });
    holder.appendChild(btn);
  }
}

function setView(newView) {
  view = newView;
  queryElements = null;
  document.getElementById("clear-query").hidden = true;
  for (const btn of document.querySelectorAll("[data-view]")) {
    btn.classList.toggle("active", btn.dataset.view === view);
  }
  renderCurrentView();
}

async function runCypher() {
  const cypherInput = document.getElementById("cypher-input").value;
  const out = document.getElementById("cypher-output");
  out.textContent = "running…";
  try {
    const res = await fetchJson("/api/query", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ cypher: cypherInput }),
    });
    const elements = res.elements;
    if (elements && elements.nodes.length > 0) {
      // Graph-shaped result: take over the main canvas.
      queryElements = elements;
      const clearBtn = document.getElementById("clear-query");
      clearBtn.hidden = false;
      clearBtn.textContent =
        `clear query result (${elements.nodes.length} nodes, ${elements.edges.length} edges)`;
      renderCurrentView();
    }
    out.textContent = JSON.stringify(res.rows ?? res, null, 2).slice(0, 20000);
  } catch (e) {
    out.textContent = `error: ${e.message}`;
  }
}

init().catch((e) => {
  document.getElementById("meta").textContent = `failed to load: ${e.message}`;
});

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
}

function visibleElements(elements) {
  if (showTransitive) return elements;
  const nodes = elements.nodes.filter((n) => n.data.scope === "direct");
  const ids = new Set(nodes.map((n) => n.data.id));
  const edges = elements.edges.filter(
    (e) => ids.has(e.data.source) && ids.has(e.data.target)
  );
  return { nodes, edges };
}

function buildGraph(payload) {
  const elements = visibleElements(payload.elements);
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
      ],
      layout: { name: "cose", animate: false, randomize: false },
      wheelSensitivity: 0.2,
    });
    cy.on("tap", "node", (evt) => showPackage(evt.target.data("id")));
  } else {
    cy.elements().remove();
    cy.add(elements);
    cy.layout({ name: "cose", animate: false }).run();
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
    buildGraph(payload);
    if (selectedPackage) showPackage(selectedPackage, asOf);
  } catch (e) {
    document.getElementById("asof-label").textContent = `error: ${e.message}`;
  }
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
  if (lastElements) buildGraph({ elements: lastElements });
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

  await refresh();
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
    out.textContent = JSON.stringify(res.rows ?? res, null, 2).slice(0, 20000);
  } catch (e) {
    out.textContent = `error: ${e.message}`;
  }
}

init().catch((e) => {
  document.getElementById("meta").textContent = `failed to load: ${e.message}`;
});

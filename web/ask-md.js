// ask-md.js — client-side markdown + ReAct-plan renderer for the Ask composer.
// Vendored (adapted) from the Mosaic /aide page (ADR 0153): a small dependency-
// free GFM renderer (headings, bold/italic, inline + fenced code, lists, links,
// blockquotes, tables) + a mermaid loader with a graceful code-block fallback.
// Exposes window.AskMd so the Leptos Ask component can format assistant answers.
(function () {
  function esc(s) {
    return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }
  function inline(s) {
    s = esc(s);
    s = s.replace(/`([^`]+)`/g, "<code>$1</code>");
    s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
    s = s.replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>");
    s = s.replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
    return s;
  }
  function renderMarkdown(src) {
    const lines = String(src).replace(/\r\n/g, "\n").split("\n");
    let html = "", i = 0, inCode = false, codeLang = "", codeBuf = [], listType = null, para = [], inTable = false, tableHead = null, tableAlign = null, tableRows = [];
    function flushPara() { if (para.length) { html += "<p>" + inline(para.join(" ")) + "</p>"; para = []; } }
    function flushList() { if (listType) { html += "</" + listType + ">"; listType = null; } }
    function flushTable() {
      if (!inTable) return;
      inTable = false;
      html += "<table><thead><tr>";
      for (const h of tableHead) html += "<th>" + inline(h) + "</th>";
      html += "</tr></thead><tbody>";
      for (const row of tableRows) { html += "<tr>"; for (let c = 0; c < row.length; c++) html += "<td>" + inline(row[c]) + "</td>"; html += "</tr>"; }
      html += "</tbody></table>";
      tableHead = null; tableRows = [];
    }
    function codeOut() {
      if (codeLang === "mermaid") { html += '<pre class="mermaid-src" data-mermaid="1">' + esc(codeBuf.join("\n")) + "</pre>"; }
      else { html += "<pre><code>" + esc(codeBuf.join("\n")) + "</code></pre>"; }
    }
    while (i < lines.length) {
      const line = lines[i];
      if (/^```/.test(line)) {
        if (!inCode) { flushPara(); flushList(); flushTable(); inCode = true; codeLang = line.slice(3).trim(); codeBuf = []; }
        else { inCode = false; codeOut(); }
        i++; continue;
      }
      if (inCode) { codeBuf.push(line); i++; continue; }
      const h = line.match(/^(#{1,6})\s+(.*)$/);
      if (h) { flushPara(); flushList(); flushTable(); const lv = h[1].length; html += "<h" + lv + ">" + inline(h[2]) + "</h" + lv + ">"; i++; continue; }
      const ul = line.match(/^\s*[-*]\s+(.*)$/);
      const ol = line.match(/^\s*\d+\.\s+(.*)$/);
      if (ul || ol) {
        flushPara(); flushTable();
        const want = ul ? "ul" : "ol";
        if (listType !== want) { flushList(); html += "<" + want + ">"; listType = want; }
        html += "<li>" + inline((ul || ol)[1]) + "</li>";
        i++; continue;
      }
      const bq = line.match(/^>\s?(.*)$/);
      if (bq) { flushPara(); flushList(); flushTable(); html += "<blockquote>" + inline(bq[1]) + "</blockquote>"; i++; continue; }
      // table row (| a | b |)
      if (/^\s*\|.*\|\s*$/.test(line)) {
        flushPara(); flushList();
        const cells = line.trim().replace(/^\|/, "").replace(/\|$/, "").split("|").map((c) => c.trim());
        if (cells.every((c) => /^:?-{2,}:?$/.test(c))) { inTable = true; tableHead = (i - 1 >= 0 && para.length) ? [] : tableHead || []; /* align row */ i++; continue; }
        if (!inTable) { inTable = true; tableHead = cells; }
        else { tableRows.push(cells); }
        i++; continue;
      } else if (inTable) { flushTable(); }
      if (line.trim() === "") { flushPara(); flushList(); flushTable(); i++; continue; }
      para.push(line.trim()); i++;
    }
    if (inCode) { codeOut(); }
    flushPara(); flushList(); flushTable();
    return html;
  }

  // The ReAct plan as a collapsible <details> (thought/action/observation/answer).
  const ICON = { thought: "💭", action: "⚙️", observation: "👁️", answer: "✅", timeout: "⏱️", step: "•" };
  function renderPlan(steps) {
    if (typeof steps === "string") { try { steps = JSON.parse(steps); } catch (e) { return ""; } }
    if (!Array.isArray(steps) || steps.length === 0) return "";
    let inner = "";
    for (const st of steps) {
      const k = st.kind || "step";
      const icon = ICON[k] || "•";
      let body = "";
      if (k === "action" && st.tool) {
        body = '<span class="rp-tool">' + esc(st.tool) + "</span>(" + esc(safeJson(st.args || {})) + ")";
      } else if (st.content) {
        body = esc(st.content);
      }
      inner += '<div class="rp-step ' + esc(k) + '"><span class="rp-ico">' + icon + "</span><span class=\"rp-kind\">" + esc(k) + "</span>" + (body ? '<span class="rp-body">' + body + "</span>" : "") + "</div>";
    }
    return '<details class="react-plan" ' + (steps.length <= 6 ? "open" : "") + "><summary>🧠 plan · " + steps.length + " step" + (steps.length === 1 ? "" : "s") + "</summary><div class=\"react-steps\">" + inner + "</div></details>";
  }
  function safeJson(v) { try { return JSON.stringify(v); } catch (e) { return String(v); } }

  // mermaid: on-demand load from /vendor/mermaid.min.js; graceful code fallback.
  let mermaidReady = false, mermaidLoading = false;
  function renderMermaid(root) {
    if (!mermaidReady) return;
    (root || document).querySelectorAll("pre.mermaid-src[data-mermaid='1']").forEach(function (el) {
      if (el.dataset.done) return;
      el.dataset.done = "1";
      const code = el.textContent;
      try {
        window.mermaid.render("mm" + Math.random().toString(36).slice(2), code).then(function (r) {
          const div = document.createElement("div"); div.className = "mermaid"; div.innerHTML = r.svg; el.replaceWith(div);
        }).catch(function () { el.className = ""; el.removeAttribute("data-mermaid"); });
      } catch (e) { el.className = ""; el.removeAttribute("data-mermaid"); }
    });
  }
  function maybeLoadMermaid() {
    if (mermaidReady || mermaidLoading) { renderMermaid(); return; }
    if (!document.querySelector("pre.mermaid-src[data-mermaid='1']")) return;
    mermaidLoading = true;
    const s = document.createElement("script");
    s.src = "/vendor/mermaid.min.js";
    s.onload = function () { try { if (window.mermaid) { window.mermaid.initialize({ startOnLoad: false, theme: "dark" }); mermaidReady = true; } } catch (e) {} renderMermaid(); };
    s.onerror = function () {
      document.querySelectorAll("pre.mermaid-src[data-mermaid='1']").forEach(function (el) { if (!el.dataset.done) { el.dataset.done = "1"; el.className = ""; el.removeAttribute("data-mermaid"); } });
    };
    document.head.appendChild(s);
  }

  window.AskMd = { esc: esc, inline: inline, renderMarkdown: renderMarkdown, renderPlan: renderPlan, renderMermaid: renderMermaid, maybeLoadMermaid: maybeLoadMermaid };
})();

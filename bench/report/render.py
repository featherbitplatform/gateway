"""Render a run's run.json into one self-contained report.html (inline SVG, no external requests)."""
from __future__ import annotations

import html
import json
import math
from datetime import datetime, timezone
from pathlib import Path

GATEWAY_SLOTS = ["featherbit", "apisix", "kong", "envoy", "tyk", "krakend", "traefik", "nginx"]
BASELINE = "direct"
STATUS_LABEL = {"n/a": "N/A", "invalid": "invalid", "error": "error", "pending": "not run"}
BOUND = ("loadgen-bound", "upstream-bound")

CSS = """
:root { color-scheme: light;
  --surface-1: #fcfcfb; --surface-2: #f3f2ef; --grid: #e4e3df; --text-primary: #0b0b0b;
  --text-secondary: #52514e; --text-muted: #7a7973; --baseline: #7a7973; --warn-bg: #fdf1dc;
  --series-1: #2a78d6; --series-2: #eb6834; --series-3: #1baf7a; --series-4: #eda100;
  --series-5: #e87ba4; --series-6: #008300; --series-7: #4a3aa7; --series-8: #e34948; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { color-scheme: dark;
  --surface-1: #1a1a19; --surface-2: #242423; --grid: #383835; --text-primary: #ffffff;
  --text-secondary: #c3c2b7; --text-muted: #9a998f; --baseline: #9a998f; --warn-bg: #3a2e14;
  --series-1: #3987e5; --series-2: #d95926; --series-3: #199e70; --series-4: #c98500;
  --series-5: #d55181; --series-6: #008300; --series-7: #9085e9; --series-8: #e66767; } }
:root[data-theme="dark"] { color-scheme: dark;
  --surface-1: #1a1a19; --surface-2: #242423; --grid: #383835; --text-primary: #ffffff;
  --text-secondary: #c3c2b7; --text-muted: #9a998f; --baseline: #9a998f; --warn-bg: #3a2e14;
  --series-1: #3987e5; --series-2: #d95926; --series-3: #199e70; --series-4: #c98500;
  --series-5: #d55181; --series-6: #008300; --series-7: #9085e9; --series-8: #e66767; }
* { box-sizing: border-box; }
body { margin: 0; background: var(--surface-1); color: var(--text-primary);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 1100px; margin: 0 auto; padding: 24px 16px 64px; }
h1 { font-size: 26px; margin: 0 0 4px; } h2 { font-size: 20px; margin: 40px 0 8px; }
h3 { font-size: 16px; margin: 24px 0 8px; color: var(--text-secondary); }
.banner { background: var(--warn-bg); border-radius: 8px; padding: 12px 16px; margin: 16px 0; font-weight: 600; }
.muted { color: var(--text-muted); }
.scroll { overflow-x: auto; }
table { border-collapse: collapse; font-variant-numeric: tabular-nums; font-size: 14px; }
th, td { padding: 6px 10px; border-bottom: 1px solid var(--grid); text-align: right; white-space: nowrap; }
th:first-child, td:first-child { text-align: left; }
td.status { color: var(--text-muted); } td.bound { color: var(--text-secondary); font-style: italic; }
.swatch { display: inline-block; width: 10px; height: 10px; border-radius: 2px; margin-right: 6px; }
.legend { display: flex; flex-wrap: wrap; gap: 4px 16px; font-size: 13px; color: var(--text-secondary); margin: 4px 0; }
.legend .dash { display: inline-block; width: 14px; border-top: 2px dashed var(--baseline); margin-right: 6px; vertical-align: middle; }
figure { margin: 16px 0; } figcaption { font-weight: 600; margin-bottom: 4px; }
svg { width: 100%; height: auto; display: block; }
svg text { fill: var(--text-muted); font-size: 11px; }
.grid line { stroke: var(--grid); stroke-width: 1; }
.crosshair { stroke: var(--text-muted); stroke-width: 1; visibility: hidden; }
details { margin-top: 4px; } summary { cursor: pointer; color: var(--text-secondary); font-size: 13px; }
.tip { position: fixed; pointer-events: none; background: var(--surface-2); color: var(--text-primary);
  border-radius: 6px; padding: 6px 10px; font-size: 13px; box-shadow: 0 2px 8px rgb(0 0 0 / .2); z-index: 10; }
.charts { display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 0 24px; }
"""

JS = """
(() => {
  const tip = document.createElement("div"); tip.className = "tip"; tip.hidden = true; document.body.appendChild(tip);
  function show(evt, lines) {
    tip.replaceChildren(...lines.map(([value, label]) => {
      const row = document.createElement("div"); const b = document.createElement("strong");
      b.textContent = value; row.append(b, document.createTextNode(label ? " " + label : "")); return row;
    }));
    tip.hidden = false;
    const pad = 12, r = tip.getBoundingClientRect();
    let x = evt.clientX + pad, y = evt.clientY + pad;
    if (x + r.width > innerWidth) x = evt.clientX - r.width - pad;
    if (y + r.height > innerHeight) y = evt.clientY - r.height - pad;
    tip.style.left = x + "px"; tip.style.top = y + "px";
  }
  document.querySelectorAll("figure[data-chart]").forEach(fig => {
    const data = JSON.parse(fig.dataset.chart), svg = fig.querySelector("svg"), hair = svg.querySelector(".crosshair");
    svg.addEventListener("pointermove", e => {
      const pt = svg.createSVGPoint(); pt.x = e.clientX; pt.y = e.clientY;
      const p = pt.matrixTransform(svg.getScreenCTM().inverse());
      let best = null;
      for (const [rate, px] of data.xpos) if (!best || Math.abs(px - p.x) < Math.abs(best[1] - p.x)) best = [rate, px];
      if (!best) return;
      hair.setAttribute("x1", best[1]); hair.setAttribute("x2", best[1]); hair.style.visibility = "visible";
      const lines = [[data.rateLabels[best[0]], "req/s offered"]];
      for (const s of data.series) { const v = s.values[best[0]]; if (v !== undefined) lines.push([v, s.name]); }
      show(e, lines);
    });
    svg.addEventListener("pointerleave", () => { hair.style.visibility = "hidden"; tip.hidden = true; });
  });
  document.querySelectorAll("[data-tip]").forEach(el => {
    el.addEventListener("pointermove", e => show(e, [[el.dataset.tip, el.dataset.tipLabel || ""]]));
    el.addEventListener("pointerleave", () => { tip.hidden = true; });
  });
})();
"""


def esc(v) -> str:
    return html.escape(str(v), quote=True)


def color(gw: str) -> str:
    return f"var(--series-{GATEWAY_SLOTS.index(gw) + 1})" if gw in GATEWAY_SLOTS else "var(--baseline)"


def fmt_rps(v) -> str:
    if v is None:
        return "–"
    return f"{v / 1000:.1f}k" if v >= 10_000 else f"{v:,.0f}"


def fmt_ms(v) -> str:
    if v is None:
        return "–"
    return f"{v:.2f} ms" if v < 10 else f"{v:.1f} ms"


def med(summary) -> float | None:
    return summary["median"] if summary else None


def ordered_gateways(run: dict) -> list[str]:
    names = {c["gateway"] for c in run["cells"].values()}
    ranked = [BASELINE] if BASELINE in names else []
    ranked += [g for g in GATEWAY_SLOTS if g in names]
    return ranked + sorted(names - set(ranked))


def ordered_scenarios(run: dict) -> list[str]:
    present = {c["scenario"] for c in run["cells"].values()}
    order = [s for s in run.get("selection", {}).get("scenarios", []) if s in present]
    return order + sorted(present - set(order))


def profiles(run: dict) -> list[int]:
    return sorted({c["profile"] for c in run["cells"].values()})


def get(run: dict, gw: str, sc: str, prof: int) -> dict | None:
    return run["cells"].get(f"{gw}|{sc}|{prof}")


def summary_cell(c: dict | None) -> tuple[str, str, str]:
    """(text, css class, tooltip)"""
    if c is None:
        return "–", "status", ""
    st = c["status"]
    if st in STATUS_LABEL:
        return STATUS_LABEL[st], "status", c.get("na_reason", st)
    m = (c.get("summary") or {}).get("max_sustainable_rps")
    if not m:
        return "no result", "status", st
    summary = c["summary"]
    if m["median"] == 0:
        return "SLO unmet", "status", ("failed the SLO (p99 or error rate) even at 10% of the unpaced "
                                       "ceiling (flag slo-unmet-at-floor)")
    text = fmt_rps(m["median"])
    if m["n"] > 1:
        text += f" ({fmt_rps(m['min'])}–{fmt_rps(m['max'])})"
    if summary.get("reps_ok", 0) < summary.get("reps_total", 0):
        text += f" · {summary['reps_ok']}/{summary['reps_total']} reps"
    if st in BOUND:
        return "≥ " + text, "bound", f"{st}: the {st.split('-')[0]} saturated first; the gateway sustained at least this"
    if "search-ceiling-hit" in summary.get("flags", []):
        return "≥ " + text, "bound", "passed even at 1.2x the flood ceiling: the true maximum is higher"
    return text, "ok", ""


def ladder_map(c: dict | None, metric: str) -> dict[int, float]:
    if not c or c["status"] not in ("ok", *BOUND):
        return {}
    out = {}
    for pt in (c.get("summary") or {}).get("ladder", []):
        v = med(pt.get(metric))
        if v is not None:
            out[pt["rate"]] = v
    return out


def line_chart(title: str, series: list[tuple[str, dict[int, float]]]) -> str:
    W, H, L, R, T, B = 640, 280, 60, 16, 12, 40
    pts = [(x, y) for _, d in series for x, y in d.items() if y > 0]
    if not pts:
        return f'<p class="muted">{esc(title)}: no measured data.</p>'
    xs = sorted({x for x, _ in pts})
    lx0, lx1 = math.log10(xs[0]), math.log10(xs[-1])
    if lx0 == lx1:
        lx0, lx1 = lx0 - 0.5, lx1 + 0.5
    ys = [y for _, y in pts]
    ly0, ly1 = math.floor(math.log10(min(ys))), math.ceil(math.log10(max(ys)))
    if ly0 == ly1:
        ly1 += 1

    def sx(x):
        return L + (math.log10(x) - lx0) / (lx1 - lx0) * (W - L - R)

    def sy(y):
        return H - B - (math.log10(y) - ly0) / (ly1 - ly0) * (H - T - B)

    parts = [f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="{esc(title)}">', '<g class="grid">']
    for e in range(ly0, ly1 + 1):
        y = sy(10 ** e)
        parts.append(f'<line x1="{L}" x2="{W - R}" y1="{y:.1f}" y2="{y:.1f}"/>')
        parts.append(f'<text x="{L - 6}" y="{y + 4:.1f}" text-anchor="end">{esc(fmt_ms(10 ** e))}</text>')
    for x in xs:
        parts.append(f'<text x="{sx(x):.1f}" y="{H - B + 16}" text-anchor="middle">{esc(fmt_rps(x))}</text>')
    parts.append(f'<text x="{(L + W - R) / 2:.0f}" y="{H - 6}" text-anchor="middle">offered load (req/s)</text></g>')
    for name, d in series:
        if not d:
            continue
        coords = [(sx(x), sy(y)) for x, y in sorted(d.items()) if y > 0]
        dash = ' stroke-dasharray="5 4"' if name == BASELINE else ""
        path = " ".join(f"{'M' if i == 0 else 'L'}{px:.1f},{py:.1f}" for i, (px, py) in enumerate(coords))
        parts.append(f'<path d="{path}" fill="none" stroke="{color(name)}" stroke-width="2"{dash}/>')
        for px, py in coords:
            parts.append(f'<circle cx="{px:.1f}" cy="{py:.1f}" r="4" fill="{color(name)}" '
                         f'stroke="var(--surface-1)" stroke-width="2"/>')
    parts.append(f'<line class="crosshair" x1="0" x2="0" y1="{T}" y2="{H - B}"/></svg>')
    payload = {
        "xpos": [[x, round(sx(x), 1)] for x in xs],
        "rateLabels": {x: fmt_rps(x) for x in xs},
        "series": [{"name": n, "values": {x: fmt_ms(y) for x, y in d.items()}} for n, d in series if d],
    }
    legend = "".join(
        (f'<span><span class="dash"></span>{esc(n)} (no gateway)</span>' if n == BASELINE else
         f'<span><span class="swatch" style="background:{color(n)}"></span>{esc(n)}</span>')
        for n, d in series if d)
    head = "".join(f"<th>{esc(fmt_rps(x))}</th>" for x in xs)
    rows = "".join(f"<tr><td>{esc(n)}</td>" + "".join(f"<td>{esc(fmt_ms(d.get(x)))}</td>" for x in xs) + "</tr>"
                   for n, d in series if d)
    return (f'<figure data-chart="{esc(json.dumps(payload))}"><figcaption>{esc(title)}</figcaption>'
            f'<div class="legend">{legend}</div>{"".join(parts)}'
            f'<details><summary>Table view</summary><div class="scroll"><table><tr><th>gateway \\ req/s</th>{head}</tr>'
            f'{rows}</table></div></details></figure>')


def bar_chart(title: str, rows: list[tuple[str, float]], fmt, unit: str) -> str:
    rows = [(n, v) for n, v in rows if v is not None]
    if not rows:
        return f'<p class="muted">{esc(title)}: no measured data.</p>'
    W, L, R, ROW = 640, 110, 90, 30
    H = ROW * len(rows) + 8
    top = max(v for _, v in rows) or 1
    parts = [f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="{esc(title)}">']
    for i, (name, v) in enumerate(rows):
        y = 4 + i * ROW
        w = max(2.0, (W - L - R) * v / top)
        parts.append(f'<text x="{L - 8}" y="{y + 18}" text-anchor="end">{esc(name)}</text>')
        # Rounded data end, square baseline end.
        r = min(4.0, w / 2)
        d = (f"M{L},{y + 4} H{L + w - r:.1f} Q{L + w:.1f},{y + 4} {L + w:.1f},{y + 4 + r:.1f} "
             f"V{y + 22 - r:.1f} Q{L + w:.1f},{y + 22} {L + w - r:.1f},{y + 22} H{L} Z")
        dash = ' stroke="var(--baseline)" stroke-dasharray="4 3" fill-opacity="0.25"' if name == BASELINE else ""
        parts.append(f'<path d="{d}" fill="{color(name)}"{dash} data-tip="{esc(fmt(v))} {esc(unit)}" '
                     f'data-tip-label="{esc(name)}"/>')
        parts.append(f'<text x="{L + w + 6:.1f}" y="{y + 18}">{esc(fmt(v))}</text>')
    parts.append("</svg>")
    table = "".join(f"<tr><td>{esc(n)}</td><td>{esc(fmt(v))} {esc(unit)}</td></tr>" for n, v in rows)
    return (f'<figure><figcaption>{esc(title)}</figcaption>{"".join(parts)}'
            f'<details><summary>Table view</summary><table>{table}</table></details></figure>')


def common_rate(run: dict, gws: list[str], sc: str, prof: int) -> int | None:
    """Highest ladder rate every measured gateway in this scenario/profile sustained."""
    sets = [set(ladder_map(get(run, g, sc, prof), "p50_ms")) for g in gws]
    sets = [s for s in sets if s]
    if not sets:
        return None
    shared = set.intersection(*sets)
    return max(shared) if shared else None


def baseline_cell(run: dict, sc: str, prof: int) -> dict | None:
    return get(run, BASELINE, sc, prof) or get(run, BASELINE, "core.proxy", prof)


def section_summary(run: dict, gws: list[str], scs: list[str]) -> str:
    out = ['<h2>Max sustainable throughput</h2>',
           f'<p class="muted">Median over repetitions of the highest offered rate with p99 ≤ '
           f'{esc(run["params"]["slo_p99_ms"])} ms and &lt; {esc(run["params"]["max_error_rate"] * 100)}% errors '
           f'(min–max in brackets). “≥”: the load generator or upstream saturated first.</p>']
    reasons = []
    for prof in profiles(run):
        out.append(f'<h3>{prof} core{"s" if prof > 1 else ""} for the gateway</h3><div class="scroll"><table><tr><th>scenario</th>')
        out += [f'<th><span class="swatch" style="background:{color(g)}"></span>{esc(g)}</th>' for g in gws]
        out.append("</tr>")
        for sc in scs:
            out.append(f"<tr><td>{esc(sc)}</td>")
            for g in gws:
                c = get(run, g, sc, prof)
                text, cls, tip = summary_cell(c)
                out.append(f'<td class="{cls}" title="{esc(tip)}">{esc(text)}</td>')
                if c and c["status"] == "n/a" and (g, c.get("na_reason")) not in reasons:
                    reasons.append((g, c.get("na_reason")))
            out.append("</tr>")
        out.append("</table></div>")
    if reasons:
        out.append("<h3>Not applicable</h3><ul>")
        out += [f"<li><strong>{esc(g)}</strong>: {esc(r)}</li>" for g, r in reasons]
        out.append("</ul>")
    return "".join(out)


def section_scenario(run: dict, gws: list[str], sc: str) -> str:
    out = [f'<h2>{esc(sc)}</h2>']
    for prof in profiles(run):
        cells = {g: get(run, g, sc, prof) for g in gws}
        if not any(ladder_map(c, "p50_ms") for c in cells.values()):
            continue
        out.append(f'<h3>{prof} core{"s" if prof > 1 else ""}</h3><div class="charts">')
        for metric, label in (("p50_ms", "p50"), ("p99_ms", "p99")):
            out.append(line_chart(f"{label} latency vs offered load",
                                  [(g, ladder_map(c, metric)) for g, c in cells.items()]))
        out.append("</div>")
        rate = common_rate(run, [g for g in gws if g != BASELINE], sc, prof)
        if rate is None:
            out.append('<p class="muted">No ladder rate was sustained by every gateway; per-core and overhead views skipped.</p>')
            continue
        at = lambda g, m: ladder_map(cells[g], m).get(rate)
        out.append('<div class="charts">')
        out.append(bar_chart(f"Throughput per gateway core at {fmt_rps(rate)} req/s",
                             [(g, at(g, "rps_per_core")) for g in gws if g != BASELINE],
                             fmt_rps, "req/s per core"))
        out.append(bar_chart(f"Peak memory at {fmt_rps(rate)} req/s",
                             [(g, at(g, "gateway_rss_peak_mb")) for g in gws if g != BASELINE],
                             lambda v: f"{v:.0f}", "MiB"))
        out.append("</div>")
        base = baseline_cell(run, sc, prof)
        b50, b99 = ladder_map(base, "p50_ms").get(rate), ladder_map(base, "p99_ms").get(rate)
        if b50 is not None:
            note = "" if base and base["scenario"] == sc else " (baseline: direct on core.proxy)"
            out.append(f'<h3>Latency added over the direct baseline at {fmt_rps(rate)} req/s{esc(note)}</h3>'
                       '<table><tr><th>gateway</th><th>added p50</th><th>added p99</th></tr>')
            for g in gws:
                if g == BASELINE or at(g, "p50_ms") is None:
                    continue
                out.append(f"<tr><td>{esc(g)}</td><td>{esc(fmt_ms(at(g, 'p50_ms') - b50))}</td>"
                           f"<td>{esc(fmt_ms(at(g, 'p99_ms') - b99) if b99 is not None and at(g, 'p99_ms') is not None else chr(8211))}</td></tr>")
            out.append("</table>")
    return "".join(out)


def section_environment(run: dict) -> str:
    fp = run.get("fingerprint", {})
    rows = "".join(
        f"<tr><td>{esc(name)}</td><td>{esc(h.get('cpu_model'))}</td><td>{esc(h.get('ncpu'))}</td>"
        f"<td>{esc(h.get('os'))}</td><td>{esc(h.get('kernel'))}</td><td>{esc(h.get('docker'))}</td>"
        f"<td>{esc(h.get('governor'))}</td><td>{esc(h.get('turbo'))}</td><td>{esc(h.get('loadavg'))}</td></tr>"
        for name, h in fp.get("hosts", {}).items())
    images = "".join(f"<tr><td>{esc(k)}</td><td>{esc(v)}</td></tr>" for k, v in run.get("images", {}).items())
    git = fp.get("git", {})
    return ('<h2>Environment</h2><div class="scroll"><table><tr><th>host</th><th>CPU</th><th>cpus</th><th>OS</th>'
            f'<th>kernel</th><th>docker</th><th>governor</th><th>turbo</th><th>load</th></tr>{rows}</table></div>'
            f'<p>Git: <code>{esc(git.get("sha"))}</code>{" (dirty)" if git.get("dirty") else ""} · '
            f'topology <code>{esc(run["topology"]["name"])}</code> ({esc(run["topology"]["kind"])})</p>'
            f'<h3>Images</h3><div class="scroll"><table>{images}</table></div>')


def section_methodology(run: dict) -> str:
    p = run["params"]
    links = "".join(f'<li><a href="configs/{esc(g)}/">configs/{esc(g)}/</a></li>'
                    for g in ordered_gateways(run) if g != BASELINE)
    return (
        "<h2>Methodology</h2><ul>"
        f"<li>Every cell first passes correctness probes; a failing cell is <em>invalid</em> and never measured.</li>"
        f"<li>Ceiling: {esc(p['ceiling_seconds'])} s unpaced flood (wrk / oha); warm-up {esc(p['warmup_seconds'])} s "
        f"at {esc(round(p['warmup_fraction'] * 100))}% of it, discarded.</li>"
        f"<li>Max sustainable rate: fixed-rate steps of {esc(p['search_step_seconds'])} s (wrk2 / oha, "
        f"coordinated-omission corrected), bisected between {esc(p['search_low'])}× and {esc(p['search_high'])}× "
        f"the ceiling to within {esc(p['search_tolerance'] * 100)}%; a step passes with p99 ≤ {esc(p['slo_p99_ms'])} ms "
        f"and &lt; {esc(p['max_error_rate'] * 100)}% errors.</li>"
        f"<li>Latency ladder: {esc(p['ladder_seconds'])} s at each of "
        f"{esc(', '.join(fmt_rps(r) for r in p['ladder']))} req/s up to the max sustainable rate.</li>"
        f"<li>{esc(p['connections'])} connections; {esc(p['reps'])} repetitions with a full restart each; "
        f"cells interleaved across gateways.</li>"
        f"<li>Saturation: a load-generator or upstream median CPU above {esc(round(p['saturation_threshold'] * 100))}% "
        f"of its cores marks the result “≥”.</li>"
        f"<li>Exact configs used:</li></ul><ul>{links}</ul>")


def fmt_started(iso: str) -> str:
    try:
        return datetime.fromisoformat(iso).astimezone(timezone.utc).strftime("%d %b %Y, %H:%M UTC")
    except ValueError:
        return iso


def render(run: dict) -> str:
    gws, scs = ordered_gateways(run), ordered_scenarios(run)
    banner = ("" if run.get("publish") else
              '<div class="banner">NOT FOR PUBLICATION — this run did not use the publish gate '
              '(dedicated Linux hosts, host networking, performance governor). Indicative only.</div>')
    body = [
        f'<h1>Gateway benchmark — {esc(fmt_started(run.get("started", "")))}</h1>',
        banner,
        section_summary(run, gws, scs),
        *[section_scenario(run, gws, sc) for sc in scs],
        section_environment(run),
        section_methodology(run),
    ]
    return ('<!doctype html><html lang="en"><head><meta charset="utf-8">'
            '<meta name="viewport" content="width=device-width, initial-scale=1">'
            f'<title>Gateway Benchmark Report</title><style>{CSS}</style></head>'
            f'<body><main>{"".join(body)}</main><script>{JS}</script></body></html>')


def write_report(run_dir: Path) -> Path:
    run = json.loads((run_dir / "run.json").read_text("utf-8"))
    out = run_dir / "report.html"
    out.write_text(render(run), encoding="utf-8")
    return out

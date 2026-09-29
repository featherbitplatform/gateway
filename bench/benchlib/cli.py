"""bench.py commands: plan, run, validate, report, clean."""
from __future__ import annotations

import argparse
import dataclasses
import math
import sys
from pathlib import Path

from .certs import generate_certs
from .config import (ROLES, ConfigError, Gateway, Params, load_gateways, load_scenarios,
                     load_topology, params_from_dict)
from .docker import Docker, DockerError
from .driver import DockerDriver, Images
from .fingerprint import git_info, host_fingerprint, provenance_mismatches, publish_blockers
from .matrix import expand, gateway_order, schedule, select
from .results import RunStore
from .runner import CellRunner

BENCH = Path(__file__).resolve().parent.parent
SCENARIOS = BENCH / "scenarios" / "scenarios.toml"
GATEWAYS = BENCH / "gateways"
TOPOLOGIES = BENCH / "topology"
RESULTS = BENCH / "results"


def _csv(value: str | None) -> list[str] | None:
    return [x.strip() for x in value.split(",") if x.strip()] if value else None


def topology_path(arg: str) -> Path:
    p = Path(arg)
    if p.suffix == ".toml" and p.is_file():
        return p.resolve()
    candidate = TOPOLOGIES / f"{arg}.toml"
    if candidate.is_file():
        return candidate
    raise ConfigError(f"topology {arg!r} not found (looked for {p} and {candidate})")


def estimate_rep_seconds(p: Params) -> int:
    """Worst case for one repetition: ~20 s boot/probes, ceiling, warm-up, search, full ladder."""
    search_steps = 2 + math.ceil(math.log2((p.search_high - p.search_low) / (p.search_tolerance * p.search_high)))
    return (20 + p.ceiling_seconds + p.warmup_seconds + search_steps * p.search_step_seconds
            + len(p.ladder) * p.ladder_seconds)


def parse_image_overrides(items: list[str], gateways: dict[str, Gateway]) -> dict[str, str]:
    out = {}
    for item in items:
        name, sep, image = item.partition("=")
        if not sep or not image:
            raise ConfigError(f"--image expects GATEWAY=IMAGE, got {item!r}")
        if name not in gateways:
            raise ConfigError(f"--image: unknown gateway {name!r}")
        out[name] = image
    return out


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="bench.py", description="Featherbit competitive benchmark suite")
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("plan", "run", "validate"):
        sp = sub.add_parser(name)
        sp.add_argument("--topology", default="local", help="name under bench/topology/ or a .toml path")
        sp.add_argument("--gateways", help="comma-separated names or globs")
        sp.add_argument("--scenarios", help="comma-separated ids or globs, e.g. 'core.*,plugin.jwt'")
        sp.add_argument("--profiles", help="comma-separated core counts, e.g. 1,4")
        if name != "validate":
            sp.add_argument("--reps", type=int)
            sp.add_argument("--quick", action="store_true", help="1 rep, 5 s steps: iteration only")
        if name != "plan":
            sp.add_argument("--image", action="append", default=[], metavar="GATEWAY=IMAGE")
    sub.choices["run"].add_argument("--publish", action="store_true")
    sub.choices["run"].add_argument("--resume", metavar="RUN_DIR")
    sub.add_parser("report").add_argument("run_dir")
    sub.add_parser("clean").add_argument("--topology", default="local")
    return ap


def _inputs():
    params, scenarios = load_scenarios(SCENARIOS)
    return params, scenarios, load_gateways(GATEWAYS, scenarios)


def _resolve(args, scenarios, gateways, topology) -> tuple[list[str], list[str], list[int]]:
    gws = select(_csv(args.gateways), gateway_order(gateways), "gateway")
    scs = select(_csv(args.scenarios), list(scenarios), "scenario")
    profs = sorted(topology.profiles)
    if args.profiles:
        try:
            wanted = sorted(int(x) for x in _csv(args.profiles))
        except ValueError:
            raise ConfigError(f"--profiles expects core counts, got {args.profiles!r}") from None
        unknown = [w for w in wanted if w not in topology.profiles]
        if unknown:
            raise ConfigError(f"unknown profile(s) {unknown}; {topology.name} defines {profs}")
        profs = wanted
    return gws, scs, profs


def _params(params: Params, args) -> Params:
    p = params.quick() if getattr(args, "quick", False) else params
    if getattr(args, "reps", None):
        p = dataclasses.replace(p, reps=args.reps)
    return p


def _dockers(topology) -> tuple[dict[str, Docker], dict[str | None, Docker]]:
    dk = {role: Docker(topology.roles[role].context) for role in ROLES}
    contexts: dict[str | None, Docker] = {}
    for role in ROLES:
        contexts.setdefault(topology.roles[role].context, dk[role])
    return dk, contexts


def _prepare(topology, gateways, needed: list[str], overrides: dict[str, str], store: RunStore):
    """Build/pull every image, record digests, make the run certificate, fingerprint the hosts."""
    images = Images()
    dk, contexts = _dockers(topology)
    local = contexts.get(None) or Docker(None)
    for d in {id(x): x for x in [*contexts.values(), local]}.values():
        print(f"building {images.loadgen} on {d.context or 'default'} ...", flush=True)
        d.ensure_image(images.loadgen, build=BENCH / "loadgen")
    print(f"building {images.upstream} ...", flush=True)
    dk["upstream"].ensure_image(images.upstream, build=BENCH / "upstream")
    gateway_images: dict[str, str] = {}
    for name in needed:
        gw = gateways[name]
        if gw.kind != "container":
            continue
        if name in overrides:
            gateway_images[name] = overrides[name]
            dk["gateway"].ensure_image(overrides[name])
        else:
            if gw.build:
                missing = [r for r in gw.build_requires if not (gw.build / r).exists()]
                if missing:
                    raise ConfigError(f"{name}: image build needs {missing}. {gw.build_hint}")
            print(f"preparing {gw.image} ...", flush=True)
            dk["gateway"].ensure_image(gw.image, build=gw.build, build_args=gw.build_args)
            gateway_images[name] = gw.image
        for dep in gw.dependencies:
            dk["upstream"].ensure_image(dep.image)
    ids = {
        "loadgen": dk["loadgen"].image_id(images.loadgen),
        "upstream": dk["upstream"].image_id(images.upstream),
        **{n: dk["gateway"].image_id(i) for n, i in gateway_images.items()},
    }
    mismatches = provenance_mismatches(store.data, ids, git_info(BENCH.parent))
    if mismatches:
        raise ConfigError("refusing to resume with a different build (results would mix):\n  "
                          + "\n  ".join(mismatches) + "\nstart a new run instead")
    store.data["images"] = {**store.data.get("images", {}), **ids}
    certs = generate_certs(local, images.loadgen)
    hosts = {ctx or "local": host_fingerprint(d, images.loadgen) for ctx, d in contexts.items()}
    return images, gateway_images, certs, hosts


def cmd_plan(args) -> int:
    params, scenarios, gateways = _inputs()
    topology = load_topology(topology_path(args.topology))
    gws, scs, profs = _resolve(args, scenarios, gateways, topology)
    params = _params(params, args)
    cells = expand(gws, scs, profs)
    runnable = [c for c in cells if gateways[c.gateway].supports(c.scenario)]
    reps = len(runnable) * params.reps
    secs = reps * estimate_rep_seconds(params)
    print(f"{len(runnable)} runnable cells, {len(cells) - len(runnable)} n/a; {reps} repetitions")
    print(f"worst-case duration ~{secs // 3600}h {secs % 3600 // 60}m")
    for c in cells:
        print(f"  {'run' if gateways[c.gateway].supports(c.scenario) else 'n/a'}  {c.key}")
    return 0


def cmd_run(args) -> int:
    params, scenarios, gateways = _inputs()
    if args.resume:
        if any([args.gateways, args.scenarios, args.profiles, args.reps, args.quick, args.publish, args.image]):
            raise ConfigError("--resume reuses the stored selection; drop the other flags")
        store = RunStore.open(Path(args.resume))
        sel = store.data["selection"]
        topology = load_topology(Path(sel["topology_path"]))
        params = params_from_dict(store.data["params"])
        gws, scs, profs = sel["gateways"], sel["scenarios"], sel["profiles"]
        overrides = sel.get("image_overrides", {})
        publish = store.data.get("publish", False)
    else:
        tpath = topology_path(args.topology)
        topology = load_topology(tpath)
        gws, scs, profs = _resolve(args, scenarios, gateways, topology)
        params = _params(params, args)
        overrides = parse_image_overrides(args.image, gateways)
        publish = args.publish
        store = RunStore.create(RESULTS, topology.name, {
            "publish": publish,
            "topology": {"name": topology.name, "kind": topology.kind},
            "selection": {"topology_path": str(tpath), "gateways": gws, "scenarios": scs,
                          "profiles": profs, "image_overrides": overrides},
            "params": dataclasses.asdict(params), "images": {}, "fingerprint": {},
        })
    cells = expand(gws, scs, profs)
    for c in cells:
        if not gateways[c.gateway].supports(c.scenario):
            store.record_na(c, gateways[c.gateway].na[c.scenario])
    plan = schedule(cells, gateways, params.reps)
    needed = sorted({c.gateway for c, _ in plan})
    try:
        images, gateway_images, certs, hosts = _prepare(topology, gateways, needed, overrides, store)
    except DockerError as e:
        raise ConfigError(f"preparing images failed: {e}") from None
    store.data["fingerprint"] = {"hosts": hosts, "git": git_info(BENCH.parent)}
    store.save()
    if publish:
        blockers = publish_blockers(topology, hosts, params, load_scenarios(SCENARIOS)[0])
        if blockers:
            raise ConfigError("--publish refused:\n  " + "\n  ".join(blockers))
    driver = DockerDriver(topology, gateways, scenarios, params, images, gateway_images, certs, store)
    runner = CellRunner(driver, params)
    todo = [(c, r) for c, r in plan if not store.done(c, r)]
    print(f"run dir: {store.path} ({len(todo)} of {len(plan)} repetitions to go)", flush=True)
    try:
        code = run_schedule(todo, runner, store, params.reps)
    except KeyboardInterrupt:
        print(f"\ninterrupted - resume with: python bench/bench.py run --resume {store.path}")
        return 130
    finally:
        driver.teardown()
    if code:
        print(f"resume once fixed: python bench/bench.py run --resume {store.path}")
        return code
    print(f"done - render the report with: python bench/bench.py report {store.path}")
    return 0


def run_schedule(todo, runner, store, reps: int, max_consecutive_errors: int = 3) -> int:
    """Run every (cell, rep); stop early (exit code 3) when the environment looks broken."""
    streak = 0
    for i, (cell, rep) in enumerate(todo, 1):
        print(f"[{i}/{len(todo)}] {cell.key} rep {rep + 1}/{reps} ...", flush=True)
        result = runner.run_rep(cell, rep)
        store.record_rep(cell, rep, result)
        print(f"    {result['status']}  max sustainable: {result.get('max_sustainable_rps', '-')} req/s"
              + (f"  ({result['error']})" if "error" in result else ""), flush=True)
        streak = streak + 1 if result["status"] == "error" else 0
        if streak >= max_consecutive_errors:
            print(f"stopping: {streak} consecutive errors - is Docker still running? "
                  "Error reps are retried on --resume.", flush=True)
            return 3
    return 0


def cmd_validate(args) -> int:
    params, scenarios, gateways = _inputs()
    tpath = topology_path(args.topology)
    topology = load_topology(tpath)
    gws, scs, profs = _resolve(args, scenarios, gateways, topology)
    overrides = parse_image_overrides(args.image, gateways)
    store = RunStore.create(RESULTS, f"{topology.name}-validate", {
        "publish": False, "topology": {"name": topology.name, "kind": topology.kind},
        "selection": {"topology_path": str(tpath), "gateways": gws, "scenarios": scs,
                      "profiles": profs, "image_overrides": overrides},
        "params": dataclasses.asdict(params), "images": {}, "fingerprint": {},
    })
    cells = [c for c in expand(gws, scs, profs[:1]) if gateways[c.gateway].supports(c.scenario)]
    try:
        images, gateway_images, certs, hosts = _prepare(topology, gateways, sorted({c.gateway for c in cells}),
                                                        overrides, store)
    except DockerError as e:
        raise ConfigError(f"preparing images failed: {e}") from None
    store.data["fingerprint"] = {"hosts": hosts, "git": git_info(BENCH.parent)}
    driver = DockerDriver(topology, gateways, scenarios, params, images, gateway_images, certs, store)
    runner = CellRunner(driver, params)
    failures = 0
    try:
        for cell in cells:
            result = runner.validate(cell)
            store.record_rep(cell, 0, result)
            bad = [p["detail"] for p in result.get("probes", []) if not p["ok"]]
            detail = result.get("error") or "; ".join(bad) or "all probes passed"
            print(f"{result['status']:8} {cell.key}  {detail}", flush=True)
            failures += result["status"] != "ok"
    finally:
        driver.teardown()
    print(f"{len(cells) - failures}/{len(cells)} cells valid - details in {store.path}")
    return 1 if failures else 0


def cmd_report(args) -> int:
    from report.render import write_report  # bench/ is on sys.path via bench.py
    print(write_report(Path(args.run_dir)))
    return 0


def cmd_clean(args) -> int:
    _, contexts = _dockers(load_topology(topology_path(args.topology)))
    removed = [cid for d in contexts.values() for cid in d.remove_labelled()]
    print(f"removed {len(removed)} container(s)")
    return 0


COMMANDS = {"plan": cmd_plan, "run": cmd_run, "validate": cmd_validate, "report": cmd_report, "clean": cmd_clean}


def main(argv: list[str]) -> int:
    args = build_parser().parse_args(argv)
    try:
        return COMMANDS[args.cmd](args)
    except ConfigError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2

#!/usr/bin/env python3
"""ter's helper for the sim86 simulator.

    ter_sim86.py run <request.json>    run a program and print the recording
    ter_sim86.py balance               the account's plan and credits
    ter_sim86.py version               the sim86 package found, no network

Prints exactly one JSON object on standard output, {"ok": true, ...} or
{"ok": false, "error": {"kind": ..., "message": ...}}, and exits 0 either
way: ter reads the object, never the exit code or any text.

Written by `ter install sim86` next to the Python environment that holds
the sim86 package, and rewritten by ter when it changes. Do not edit it.
"""
import json
import sys
import time
import warnings

HELPER = 1

# A trace comes back exact, every edge with its time, only while it has
# few enough edges; past that it is a summary. A slice is cut to hold
# about this many, well under the limit seen (about 1000).
EDGES_PER_SLICE = 600
# No slice is cut finer than this; a net busier than that is not read.
MIN_SLICE_NS = 1_000
# How many reads one net may take before ter gives up on it.
MAX_READS = 4_000

LOW, HIGH = 1, 2


class Failed(Exception):
    def __init__(self, kind, message):
        super().__init__(message)
        self.kind, self.message = kind, message


def classify(error):
    """The kind of a sim86 error, from its class and status, then its words."""
    from sim86 import ApiError, AuthError, RunEnded

    text = str(error)
    low = text.lower()
    if isinstance(error, AuthError):
        return "no_key" if low.startswith("no api key") else "auth"
    if any(word in low for word in ("credit", "balance", "quota", "payment")):
        return "credits"
    if isinstance(error, ApiError):
        if error.status == 426:
            return "version"
        if error.status == 402:
            return "credits"
        if error.status in (502, 503, 504) or "no simulator came free" in low:
            return "busy"
        return "api"
    if isinstance(error, RunEnded):
        return "ended"
    if "no-capacity" in low or "no capacity" in low:
        return "busy"
    if low.startswith("run refused"):
        return "refused"
    if "not a format" in low:
        return "image"
    return "api"


def level_of(state):
    return {LOW: 0, HIGH: 1}.get(state)


class Reader:
    """Every edge on one net from 0 to end_ns, or Failed("incomplete")."""

    def __init__(self, run, pin, end_ns):
        self.run, self.pin, self.end_ns = run, pin, end_ns
        self.reads = 0
        self.leaves = []

    def incomplete(self, why):
        return Failed("incomplete", f"{self.pin}: {why}")

    def trace(self, wire):
        trace = (wire or {}).get("wireTrace")
        if not isinstance(trace, dict) or "mode" not in trace:
            raise self.incomplete("sim86 returned no digital trace for this net")
        return trace

    def window(self, a, b):
        """The net's trace over a..b, read back from the paused run."""
        self.reads += 1
        if self.reads > MAX_READS:
            raise self.incomplete(f"more than {MAX_READS} reads needed, the signal is too dense to record")
        observe = getattr(self.run, "_observe", None)
        if observe is None:
            raise self.incomplete("this sim86 package cannot read a recording in slices")
        snapshot = observe(b - a, end_ns=b)
        wire = next((w for w in snapshot.get("wires", []) if w.get("id") == self.pin), None)
        return self.trace(wire)

    def exact(self, trace, a, b):
        return (
            trace.get("mode") == "exact"
            and not trace.get("buckets")
            and int(trace.get("fromNs", a)) <= a
            and int(trace.get("toNs", b)) >= b
            and int(trace.get("exactFromNs") or 0) <= a
            and int(trace.get("availableFromNs") or 0) <= a
        )

    def cuts(self, trace, a, b):
        """Where to cut a..b so each piece holds few enough edges."""
        points = []
        held = 0
        for bucket in trace.get("buckets") or []:
            changes = bucket.get("changes")
            start, stop = bucket.get("fromNs"), bucket.get("toNs")
            if not isinstance(changes, int) or start is None or stop is None:
                points = None
                break
            start, stop = max(a, int(start)), min(b, int(stop))
            if stop <= start:
                continue
            if changes > EDGES_PER_SLICE:
                # One busy bucket: close what is held, then cut it evenly.
                if held and start > a:
                    points.append(start)
                pieces = changes // EDGES_PER_SLICE + 1
                step = max(1, (stop - start) // pieces)
                points.extend(range(start + step, stop, step))
                points.append(stop)
                held = 0
            elif held + changes > EDGES_PER_SLICE:
                points.append(start)
                held = changes
            else:
                held += changes
        if not points:
            step = max(1, (b - a) // 8)
            points = list(range(a + step, b, step))
        points = sorted({p for p in points if a < p < b})
        return [a] + points + [b]

    def read(self, a, b, trace=None):
        if trace is None:
            trace = self.window(a, b)
        if self.exact(trace, a, b):
            self.leaves.append((a, b, trace))
            return
        if b - a <= MIN_SLICE_NS:
            raise self.incomplete(
                f"the recording between {a} and {b} ns is a summary, not every edge")
        bounds = self.cuts(trace, a, b)
        if len(bounds) < 3:
            raise self.incomplete(f"the recording between {a} and {b} ns cannot be cut finer")
        for start, stop in zip(bounds, bounds[1:]):
            self.read(start, stop)

    def edges(self, whole):
        """`whole` is the trace of 0..end_ns from the one public read."""
        self.read(0, self.end_ns, self.trace(whole))
        initial = level_of(self.leaves[0][2].get("initialState"))
        level = initial
        seen = set()
        out = []
        for a, b, trace in self.leaves:
            start = level_of(trace.get("initialState"))
            # A lost or doubled edge where two slices meet shows here: the
            # level a slice starts at is the one the last slice ended at.
            if start is not None and level is not None and start != level:
                raise self.incomplete(f"the slices of the recording do not join at {a} ns")
            if level is None:
                level = start
            for edge in trace.get("edges") or []:
                t = int(edge.get("timeNs", -1))
                if t < a or t > b or t > self.end_ns:
                    continue
                new = level_of(edge.get("state"))
                if new is None or (t, new) in seen:
                    continue
                seen.add((t, new))
                if new != level:
                    out.append([t, new])
                    level = new
        if any(x[0] > y[0] for x, y in zip(out, out[1:])):
            raise self.incomplete("the edges did not come back in time order")
        return {"initial": initial, "edges": out, "reads": self.reads}


def record(request):
    from sim86 import Client, Run, check_board, load_board

    timeout_ms = int(request["timeout_ms"])
    end_ns = timeout_ms * 1_000_000
    watch = list(request.get("watch") or [])
    try:
        board = load_board(request["board"], firmware_overrides=request.get("firmware") or {})
    except Exception as e:  # noqa: BLE001 - a board file sim86 cannot read
        raise Failed("board", str(e))
    client = Client()
    problems = check_board(board, client)
    if problems:
        text = "; ".join(str(p) for p in problems)
        raise Failed("image" if "not a format" in text or "firmware" in text.lower() else "board", text)

    started = time.monotonic()
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        run = Run.start(
            board, client,
            duration=float(request.get("duration_ms") or timeout_ms + 300),
            realtime=float(request.get("realtime_ms") or 120_000),
            scenario=request.get("scenario") or None,
            paused=True,
        )
    try:
        halt = run.status()
        answer = {"run": run.id, "started_ns": halt.time_ns, "paused_at_start": bool(halt.paused)}
        if not halt.paused or halt.time_ns != 0:
            # Nothing before this moment was recorded: not a run to judge.
            return answer
        if watch:
            run.watch(*watch)
        halt = run.until(at=timeout_ms)
        if halt.time_ns < end_ns:
            raise Failed("ended", f"the run stopped at {halt.time_ns / 1e6:.3f} ms, before {timeout_ms} ms")
        answer["end_ns"] = end_ns
        pins = {}
        for pin in watch:
            # One public read of 0..now; a net too busy for it is then read
            # in slices.
            whole = run.measure(pin, over=halt.time_ns / 1e6).raw
            pins[pin] = Reader(run, pin, end_ns).edges(whole)
        answer["pins"] = pins
        answer["serial"] = [
            [float(line.t), str(line.source), str(line.line)] for line in run.serial.lines
        ]
        answer["applied"] = [applied(frame) for frame in run.plan_events]
        answer["wall_s"] = round(time.monotonic() - started, 1)
        return answer
    finally:
        try:
            run.stop()
        except Exception:
            pass


def applied(frame):
    """One scenario event the simulator reported, as it reported it."""
    keep = {}
    for key in ("atNs", "timeNs", "simulationTimeNs", "part", "component", "action", "input", "value", "event",
                "kind", "status", "ok", "error", "message"):
        if key in frame:
            keep[key] = frame[key]
    return keep


def balance():
    from sim86 import Client

    data = Client().gql("{ getSubscriptionDetails { creditBalanceCents plan isActive } }")
    details = data.get("getSubscriptionDetails") or {}
    credits = details.get("creditBalanceCents")
    if not isinstance(credits, (int, float)):
        raise Failed("api", "sim86 did not say how many credits the account has")
    return {"balance": {"credits": int(credits), "plan": str(details.get("plan") or "")}}


def main(argv):
    out = {"helper": HELPER, "ok": False, "python": ".".join(str(n) for n in sys.version_info[:3])}
    try:
        try:
            import sim86
        except ImportError as e:
            raise Failed("missing", f"the sim86 package is not in this Python: {e}")
        out["sim86"] = str(getattr(sim86, "__version__", ""))
        mode = argv[1] if len(argv) > 1 else ""
        if mode == "run" and len(argv) > 2:
            with open(argv[2], encoding="utf-8") as f:
                out.update(record(json.load(f)))
        elif mode == "balance":
            out.update(balance())
        elif mode == "version":
            pass
        else:
            raise Failed("internal", "usage: ter_sim86.py run <request.json> | balance | version")
        out["ok"] = True
    except Failed as e:
        out["error"] = {"kind": e.kind, "message": e.message}
    except Exception as e:  # noqa: BLE001 - every failure becomes one JSON object
        kind = "internal"
        try:
            from sim86 import Sim86Error

            if isinstance(e, Sim86Error):
                kind = classify(e)
        except ImportError:
            pass
        out["error"] = {"kind": kind, "message": f"{e}" or type(e).__name__}
    sys.stdout.write(json.dumps(out) + "\n")
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

#!/usr/bin/env python3
"""ter's helper for the sim86 simulator.

    ter_sim86.py run <request.json>    run a program and print the recording
    ter_sim86.py live <request.json>   run a program until told to stop,
                                       printing what it does as it runs
    ter_sim86.py balance               the account's plan and credits
    ter_sim86.py version               the sim86 package found and whether
                                       it has a key, no network

Prints exactly one JSON object on standard output, {"ok": true, ...} or
{"ok": false, "error": {"kind": ..., "message": ...}}, and exits 0 either
way: ter reads the object, never the exit code or any text.

`live` prints one JSON object a line before that one, each with an "ev":
"started" (the run exists, paused at 0, and now runs), "pin" and "serial"
as they happen, "recording" once the check's time is up (its window in
full, in the shape `run` answers with; a run that ends sooner has none)
and "ended". It runs with no simulated-time limit and stops the run when it
reads the line `stop` on standard input, or when standard input closes, so
a ter that dies never leaves a run going.

Written by `ter install sim86` next to the Python environment that holds
the sim86 package, and rewritten by ter when it changes. Do not edit it.
"""
import json
import sys
import threading
import time
import warnings

HELPER = 3

# A trace comes back exact, every edge with its time, only while it has
# few enough edges; past that it is a summary. A slice is cut to hold
# about this many, well under the limit seen (about 1000).
EDGES_PER_SLICE = 600
# No slice is cut finer than this; a net busier than that is not read.
MIN_SLICE_NS = 1_000
# How many reads one net may take before ter gives up on it.
MAX_READS = 4_000

# sim86 answers for at most this long a window at once.
MAX_WINDOW_NS = 10_000_000_000
# A live run: how long to wait between two looks at the nets, how far each
# look reaches back over the one before, and how long past the check's end
# the board runs before its window is read (as `run` does).
POLL_S = 0.1
OVERLAP_NS = 300_000_000
TAIL_NS = 300_000_000

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
        return self.joined()

    def all(self):
        """Every edge of 0..end_ns, read in windows sim86 answers for."""
        a = 0
        while a < self.end_ns:
            b = min(self.end_ns, a + MAX_WINDOW_NS)
            self.read(a, b)
            a = b
        return self.joined()

    def joined(self):
        if not self.leaves:
            return {"initial": None, "edges": [], "reads": self.reads}
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


def opened(request):
    """The request's board with the program in it, and the client."""
    from sim86 import Client, check_board, load_board

    try:
        board = load_board(request["board"], firmware_overrides=request.get("firmware") or {})
    except Exception as e:  # noqa: BLE001 - a board file sim86 cannot read
        raise Failed("board", str(e))
    client = Client()
    problems = check_board(board, client)
    if problems:
        text = "; ".join(str(p) for p in problems)
        raise Failed("image" if "not a format" in text or "firmware" in text.lower() else "board", text)
    return board, client


def record(request):
    from sim86 import Run

    timeout_ms = int(request["timeout_ms"])
    end_ns = timeout_ms * 1_000_000
    watch = list(request.get("watch") or [])
    board, client = opened(request)

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


OUT = threading.Lock()


def say(event):
    with OUT:
        sys.stdout.write(json.dumps(event) + "\n")
        sys.stdout.flush()


class Told(threading.Thread):
    """Sets `stop` when ter says so, or is no longer there to say it."""

    def __init__(self):
        super().__init__(daemon=True)
        self.stop = threading.Event()

    def run(self):
        try:
            for line in sys.stdin:
                if line.strip() == "stop":
                    break
        finally:
            self.stop.set()


class Tail:
    """One net of a running board: each look says what changed since the
    last one. A stretch too busy to come back edge by edge is not shown;
    the recording reads it in slices."""

    def __init__(self, pin):
        self.pin = pin
        self.to_ns = 0
        self.last_ns = -1
        self.level = None
        self.begun = False

    def take(self, trace):
        to_ns = int(trace.get("toNs") or 0)
        need = max(0, self.to_ns - OVERLAP_NS)
        exact = (
            trace.get("mode") == "exact"
            and not trace.get("buckets")
            and int(trace.get("fromNs") or 0) <= need
            and int(trace.get("exactFromNs") or 0) <= need
            and int(trace.get("availableFromNs") or 0) <= need
        )
        if not exact:
            # What comes next starts from a level the next look gives.
            self.level = None
            self.to_ns = max(self.to_ns, to_ns)
            return
        if not self.begun:
            self.begun = True
            self.level = level_of(trace.get("initialState"))
            if self.level is not None:
                say({"ev": "pin", "t_ns": 0, "net": self.pin, "level": self.level})
        for edge in trace.get("edges") or []:
            t = int(edge.get("timeNs", -1))
            new = level_of(edge.get("state"))
            if new is None or t <= self.last_ns or t > to_ns:
                continue
            self.last_ns = t
            if new != self.level:
                self.level = new
                say({"ev": "pin", "t_ns": t, "net": self.pin, "level": new})
        self.to_ns = max(self.to_ns, to_ns)


def recording(run, watch, end_ns, pins):
    return {
        "ev": "recording",
        "run": run.id,
        "started_ns": 0,
        "paused_at_start": True,
        "end_ns": end_ns,
        "pins": pins,
        "serial": [[float(line.t), str(line.source), str(line.line)] for line in run.serial.lines],
        "applied": [applied(frame) for frame in run.plan_events],
    }


def live(request):
    from sim86 import Run, Sim86Error

    end_ns = int(request["timeout_ms"]) * 1_000_000
    watch = list(request.get("watch") or [])
    board, client = opened(request)
    # A board file may carry limits of its own, meant for sim86's command.
    # ter holds a live run's limit: no simulated-time limit at all.
    board.limits = {}
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        run = Run.start(
            board, client,
            duration=None,
            realtime=float(request["realtime_ms"]),
            scenario=request.get("scenario") or None,
            paused=True,
        )
    told = Told()
    try:
        halt = run.status()
        if not halt.paused or halt.time_ns != 0:
            raise Failed("not_paused", f"the recording starts {halt.time_ns / 1e6:.3f} ms in")
        if watch:
            run.watch(*watch)
        say({"ev": "started", "run": run.id})
        told.start()
        run.resume()

        tails = {pin: Tail(pin) for pin in watch}
        lines_said = 0
        recorded = False
        now_ns = 0
        looked = time.monotonic()
        while True:
            if told.stop.wait(POLL_S):
                break
            try:
                if run.ended is None:
                    wall = time.monotonic()
                    if watch:
                        reach = int((wall - looked) * 1.5e9) + OVERLAP_NS + 500_000_000
                        snapshot = run._observe(min(MAX_WINDOW_NS, reach))
                        for wire in snapshot.get("wires", []):
                            tail = tails.get(wire.get("id"))
                            trace = wire.get("wireTrace")
                            if tail is not None and isinstance(trace, dict) and "mode" in trace:
                                tail.take(trace)
                        now_ns = min(tail.to_ns for tail in tails.values())
                    else:
                        now_ns = run.status().time_ns
                    looked = wall
                lines = run.serial.lines
                for line in lines[lines_said:]:
                    say({"ev": "serial", "t_ms": float(line.t), "source": str(line.source), "line": str(line.line)})
                lines_said = len(lines)
                if not recorded and run.ended is None and now_ns >= end_ns + TAIL_NS:
                    say(recording(run, watch, end_ns, {pin: Reader(run, pin, end_ns).all() for pin in watch}))
                    recorded = True
            except Sim86Error:
                # A run that ended under a request is an ended run.
                if run.ended is None:
                    raise
            if run.ended is not None:
                break
        ended = run.ended
        say({"ev": "ended", "reason": "stopped" if ended in (None, "disconnected") else str(ended)})
        return {"run": run.id}
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


def has_key():
    """Whether the package finds a key (the environment, a token.env, or
    the one `sim86 login` stored). The key itself stays with the package."""
    from sim86 import Client

    return bool(Client().api_key)


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
        elif mode == "live" and len(argv) > 2:
            with open(argv[2], encoding="utf-8") as f:
                out.update(live(json.load(f)))
        elif mode == "balance":
            out.update(balance())
        elif mode == "version":
            out["key"] = has_key()
        else:
            raise Failed("internal", "usage: ter_sim86.py run <request.json> | live <request.json> | balance | version")
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
    say(out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

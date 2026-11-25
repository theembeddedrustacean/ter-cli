#!/usr/bin/env python3
"""ter's helper for the sim86 simulator.

    ter_sim86.py run <request.json>    run a program and print the recording
    ter_sim86.py live <request.json>   run a program until told to stop,
                                       printing what it does as it runs
    ter_sim86.py balance               the account's plan and credits
    ter_sim86.py view <run>            the address of sim86's own page
                                       for a run, signed in, no network
    ter_sim86.py version               the sim86 package found and whether
                                       it has a key, no network

Prints exactly one JSON object on standard output, {"ok": true, ...} or
{"ok": false, "error": {"kind": ..., "message": ...}}, and exits 0 either
way: ter reads the object, never the exit code or any text.

`live` prints one JSON object a line before that one, each with an "ev":
"started" (the run exists, paused at 0, and now runs; with the programmed
part's chip signal names when sim86's catalog gives them, and the
"controls" of every part with inputs or events), "pin" and
"serial" as they happen (pins of the request's "watch" and "show" nets;
on an analog net each "pin" also carries its "volts", said when they
change and at most once per 50 ms of simulated time),
"recording" once the check's time is up (its window in full, in the shape
`run` answers with; a run that ends sooner has none) and "ended". It runs with no simulated-time limit and stops the run when it
reads the line `stop` on standard input, or when standard input closes, so
a ter that dies never leaves a run going.

While a live run goes on, ter may also write a JSON object a line, each
with an "id" and what to "do": "pause" or "resume". The helper does it to
the run and answers with a line holding the same "id": "paused" or
"resumed" with the board's simulated time ("t_ns"), or "refused" with
sim86's "message". "input" sets a part's input ("part", "name", "value")
and "action" sends a part an event ("part", "action"); each answers
with its own name and the time it was done at. "reset" stops the run and starts a new one from the
same request, paused at 0 and then running, as at the start: the answer
is "reset" with the new run's id ("run"), and every line after it is the
new run's. A reset that cannot start the new run answers "refused" and
ends as any failed start does.

A part's "config" in the board file may carry "inputs", {name: value}:
what the part senses from time 0, set while the board is paused there.
It is ter's, checked against sim86's catalog and not sent to sim86.

A part whose catalog says its event is a JSON object of one number in a
range ('Event: {"distance_cm": 2..400} ...') takes that number as an
input of the same name, set by sending the part that object; null is
allowed when the catalog says {"distance_cm": null} does something.

Written by `ter install sim86` next to the Python environment that holds
the sim86 package, and rewritten by ter when it changes. Do not edit it.
"""
import json
import os
import re
import sys
import threading
import time
import warnings

HELPER = 9

# A trace comes back exact, every edge with its time, only while it has
# few enough edges; past that it is a summary. A slice is cut to hold
# about this many, well under the limit seen (about 1000).
EDGES_PER_SLICE = 600
# No slice is cut finer than this; a net busier than that is not read.
MIN_SLICE_NS = 1_000
# How many reads one net may take before ter gives up on it.
MAX_READS = 4_000

# sim86 answers for at most this long a window at once, and holds a net's
# recording for the last this long only.
MAX_WINDOW_NS = 10_000_000_000
# So a recording is read a piece of this long at a time, each piece while
# sim86 still holds all of it.
CHUNK_NS = 5_000_000_000
# A live run reads its recording as it goes, once this much more is there.
READ_AHEAD_NS = 2_000_000_000
# A live run: how long to wait between two looks at the nets, how far each
# look reaches back over the one before, and how long past the check's end
# the board runs before its window is read (as `run` does).
POLL_S = 0.1
OVERLAP_NS = 300_000_000
TAIL_NS = 300_000_000

# How long what sim86's catalog says of a part is kept beside the helper.
CATALOG_S = 24 * 3600

# What a part senses before it is sent the event that sets an input, where
# the catalog does not say: measured on sim86 0.2 (an HC-SR04 with no
# event sees a target at 100 cm). An input set by an event whose start is
# known neither here nor from the board file is not offered to the page.
EVENT_DEFAULTS = {"HCSR04": {"distance_cm": 100}}

# The ends of an event input's name that are its unit ("distance_cm").
UNITS = {"cm", "mm", "m", "in", "us", "ms", "s", "hz", "khz", "mv", "v", "ma", "lux", "deg", "pct", "rpm"}

LOW, HIGH = 1, 2

# An analog net's volts: said at most once per this much simulated time,
# to this many decimals.
VOLTS_EVERY_NS = 50_000_000
VOLTS_DECIMALS = 3

# Pin names that are a supply or a ground, as ter's own list is.
POWER = {"3V3", "3.3V", "5V", "U5V", "E5V", "VIN", "VCC", "VDD", "VDDA", "VBAT", "VBUS", "AVDD", "IOREF", "AREF", "VREF"}


def is_power(pin):
    pin = str(pin).upper()
    return pin.startswith("GND") or pin.endswith("GND") or pin in POWER


class Failed(Exception):
    def __init__(self, kind, message):
        super().__init__(message)
        self.kind, self.message = kind, message


def classify(error):
    """The kind of a sim86 error, from its class and status, then its words."""
    import sim86
    from sim86 import ApiError, AuthError, RunEnded

    text = str(error)
    low = text.lower()
    if isinstance(error, AuthError):
        return "no_key" if low.startswith("no api key") else "auth"
    # Newer packages name these; older ones say it in words.
    refused = getattr(sim86, "CircuitRefused", None)
    if refused is not None and isinstance(error, refused):
        return "board"
    out_of_credits = getattr(sim86, "OutOfCredits", None)
    if out_of_credits is not None and isinstance(error, out_of_credits):
        return "credits"
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
    """Every edge on one net from 0 to end_ns, or Failed("incomplete"),
    read a piece at a time while sim86 still holds it."""

    def __init__(self, run, pin, end_ns):
        self.run, self.pin, self.end_ns = run, pin, end_ns
        self.reads = 0
        self.leaves = []
        # Read in full up to here.
        self.read_to = 0

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

    def read(self, a, b):
        trace = self.window(a, b)
        if int(trace.get("availableFromNs") or 0) > a:
            raise self.incomplete(f"sim86 no longer holds the recording from {a} ns (it keeps the last 10 s)")
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

    def upto(self, b):
        """Read on from where the last read stopped, to b."""
        b = min(b, self.end_ns)
        while self.read_to < b:
            stop = min(b, self.read_to + CHUNK_NS)
            self.read(self.read_to, stop)
            self.read_to = stop

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
    """The request's board with the program in it, the client, the
    inputs the board's parts start with, and which of those inputs are
    set by an event."""
    from sim86 import Client, check_board, load_board

    try:
        board = load_board(request["board"], firmware_overrides=request.get("firmware") or {})
    except Exception as e:  # noqa: BLE001 - a board file sim86 cannot read
        raise Failed("board", str(e))
    client = Client()
    starts = starting(board, client)
    problems = check_board(board, client)
    if problems:
        text = "; ".join(str(p) for p in problems)
        raise Failed("image" if "not a format" in text or "firmware" in text.lower() else "board", text)
    return board, client, starts, by_event(board, client)


def catalog(client, kind):
    """What sim86's catalog says of a part type: its name, inputs,
    description and pins. Kept a day beside the helper; a failed look is
    not kept."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "catalog-cache.json")
    key = f"{getattr(client, 'api_url', '')}|{kind}"
    try:
        with open(path, encoding="utf-8") as f:
            cache = json.load(f)
    except (OSError, ValueError):
        cache = {}
    if not isinstance(cache, dict):
        cache = {}
    hit = cache.get(key)
    if isinstance(hit, dict) and isinstance(hit.get("entry"), dict):
        if 0 <= time.time() - float(hit.get("at") or 0) < CATALOG_S:
            return hit["entry"]
    full = client.component(kind)
    entry = {k: full[k] for k in ("name", "inputs", "description") if k in full}
    entry["pins"] = [
        {"name": p.get("name"), "signal": p.get("signal")} for p in full.get("pins") or [] if isinstance(p, dict)
    ]
    cache[key] = {"at": time.time(), "entry": entry}
    try:
        kept = f"{path}.{os.getpid()}"
        with open(kept, "w", encoding="utf-8") as f:
            json.dump(cache, f)
        os.replace(kept, path)
    except OSError:
        pass
    return entry


def actions_of(text):
    """The events a part takes, from its catalog description ("Events:
    'pressed', 'released'."): the catalog names them nowhere else."""
    found = re.search(r"\bEvents?:\s*(.*?)(?:\.\s|\.$|$)", text or "", re.S)
    if not found:
        return []
    return list(dict.fromkeys(re.findall(r"'([A-Za-z_][A-Za-z0-9_]*)'", found.group(1))))


def event_inputs(kind, text):
    """The input a part takes as an event of one number, from its catalog
    description ('Event: {"distance_cm": 2..400} sets the target,
    {"distance_cm": null} removes it'): the catalog says it nowhere else.
    Shaped as the catalog's inputs are, with "event": True."""
    number = r"(-?\d+(?:\.\d+)?)"
    found = re.search(
        r'\bEvents?:\s*\{\s*"([A-Za-z_][A-Za-z0-9_]*)"\s*:\s*' + number + r"\s*\.\.\s*" + number + r"\s*\}",
        text or "",
    )
    if not found:
        return []
    name, lo, hi = found.groups()
    whole = "." not in lo and "." not in hi
    lo, hi = (int(lo), int(hi)) if whole else (float(lo), float(hi))
    head, _, end = name.rpartition("_")
    unit = end if head and end.lower() in UNITS else ""
    words = (head if unit else name).replace("_", " ").strip()
    spec = {"name": name, "label": words[:1].upper() + words[1:], "unit": unit, "min": lo, "max": hi, "event": True}
    if whole:
        spec["step"] = 1
    if re.search(r'\{\s*"' + re.escape(name) + r'"\s*:\s*null\s*\}', text):
        spec["nullable"] = True
    default = EVENT_DEFAULTS.get(kind, {}).get(name)
    if default is not None:
        spec["default"] = default
    return [spec]


def inputs_of(kind, entry):
    """A part's inputs, by name: the catalog's, and the one its event sets."""
    specs = {str(s["name"]): s for s in entry.get("inputs") or [] if isinstance(s, dict) and s.get("name")}
    for spec in event_inputs(kind, entry.get("description")):
        specs.setdefault(spec["name"], spec)
    return specs


def by_event(board, client):
    """The inputs of the board's parts that are set by an event, {(part, name)}."""
    found = set()
    for part in board.parts:
        try:
            entry = catalog(client, part.type)
        except Exception:  # noqa: BLE001 - set as sim86 takes inputs then
            continue
        for name, spec in inputs_of(part.type, entry).items():
            if spec.get("event"):
                found.add((part.id, name))
    return found


def set_input(run, events, part, name, value):
    """Set a part's input: by its event when the part takes it so."""
    if (part, name) in events:
        run.event(part, json.dumps({name: value}))
    else:
        run[part].set_input(name, value)


def wrong_input(specs, name, value):
    """What is wrong with setting input `name` to `value`, by the catalog's
    inputs of the part (`specs`, by name); None when nothing is."""
    spec = specs.get(name)
    if spec is None:
        return f"there is no input {name!r}; it has {', '.join(sorted(specs)) or 'none'}"
    if value is None and spec.get("nullable"):
        return None
    if spec.get("event") and isinstance(value, list):
        return f"{name} is one number"
    values = value if isinstance(value, list) else [value]
    if not values or not all(isinstance(v, (int, float)) and not isinstance(v, bool) for v in values):
        return f"{name} is a number"
    lo, hi = spec.get("min"), spec.get("max")
    if any((lo is not None and v < lo) or (hi is not None and v > hi) for v in values):
        return f"{name} is from {lo} to {hi}"
    return None


def starting(board, client):
    """The inputs each part starts with, {part: {name: value}}: its
    config.inputs, taken out of the config sim86 is sent and checked
    against the catalog when the catalog answers."""
    starts = {}
    for part in board.parts:
        config = part.config if isinstance(part.config, dict) else {}
        inputs = config.pop("inputs", None)
        if inputs is None:
            continue
        if not isinstance(inputs, dict):
            raise Failed("board", f"{part.id} config.inputs: write it as {{\"name\": value}}")
        try:
            entry = catalog(client, part.type)
        except Exception:  # noqa: BLE001 - unchecked then, as sim86 takes it
            entry = None
        specs = inputs_of(part.type, entry or {})
        for name, value in inputs.items():
            problem = wrong_input(specs, str(name), value) if entry is not None else None
            if problem:
                raise Failed("board", f"{part.id} config.inputs: {problem}")
            starts.setdefault(part.id, {})[str(name)] = value
    return starts


def start_inputs(run, starts, events):
    """Set the inputs the parts start with, on a board paused at 0."""
    for part, inputs in starts.items():
        for name, value in inputs.items():
            set_input(run, events, part, name, value)


def part_controls(board, client, starts):
    """Each part the page can change while the board runs: its inputs,
    with the catalog's ranges and the value each starts at, and the
    events it takes. A part the catalog does not answer for has none."""
    parts = []
    for part in board.parts:
        try:
            entry = catalog(client, part.type)
        except Exception:  # noqa: BLE001 - controls are a nicety
            continue
        controls = []
        for name, spec in inputs_of(part.type, entry).items():
            start = starts.get(part.id) or {}
            if spec.get("event") and name not in start and "default" not in spec:
                # What it senses until set is not known: never shown wrong.
                continue
            control = {"kind": "input", "name": name}
            for key in ("label", "unit", "min", "max", "step", "widget", "axes", "advanced", "nullable"):
                if spec.get(key) is not None:
                    control[key] = spec[key]
            control["value"] = start.get(name, spec.get("default"))
            controls.append(control)
        actions = actions_of(entry.get("description"))
        if actions:
            controls.append({"kind": "action", "name": "events", "actions": actions})
        if controls:
            label = getattr(part, "label", None) or entry.get("name") or part.id
            parts.append({"id": part.id, "label": str(label), "type": part.type, "controls": controls})
    return parts


def observe(run, pins):
    """Have sim86 record these nets from now: `observe` in newer packages,
    `watch` in older ones."""
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        (getattr(run, "observe", None) or run.watch)(*pins)


def record(request):
    from sim86 import Run

    timeout_ms = int(request["timeout_ms"])
    end_ns = timeout_ms * 1_000_000
    watch = list(request.get("watch") or [])
    board, client, starts, events = opened(request)

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
        start_inputs(run, starts, events)
        if watch:
            observe(run, watch)
        readers = {pin: Reader(run, pin, end_ns) for pin in watch}
        # The board runs a piece at a time and waits while that piece is
        # read: sim86 holds only the last 10 s of a net.
        at_ns = 0
        while at_ns < end_ns:
            at_ns = min(end_ns, at_ns + CHUNK_NS)
            halt = run.until(at=at_ns / 1e6)
            if halt.time_ns < at_ns:
                raise Failed("ended", f"the run stopped at {halt.time_ns / 1e6:.3f} ms, before {timeout_ms} ms")
            for reader in readers.values():
                reader.upto(at_ns)
        answer["end_ns"] = end_ns
        answer["pins"] = {pin: reader.joined() for pin, reader in readers.items()}
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
    """Sets `stop` and stops the run when ter says so, or is no longer
    there to say it. It stops the run itself, from this thread, so a main
    loop that is waiting on sim86 does not keep the run going."""

    def __init__(self):
        super().__init__(daemon=True)
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.sim_run = None
        # The id of a reset ter asked for, until the main loop does it.
        self.reset = None
        # The parts' inputs set by an event, {(part, name)}.
        self.events = set()

    def attach(self, sim_run):
        """The run to stop, once it exists: at once if ter said so already."""
        with self.lock:
            self.sim_run = sim_run
        if self.stop.is_set():
            self.end()

    def end(self):
        with self.lock:
            sim_run, self.sim_run = self.sim_run, None
        if sim_run is not None:
            try:
                sim_run.client.stop_run(sim_run.id)
            except Exception:
                pass

    def detach(self):
        """The run is being replaced: nothing to stop from here meanwhile."""
        with self.lock:
            self.sim_run = None

    def take_reset(self):
        with self.lock:
            asked, self.reset = self.reset, None
        return asked

    def run(self):
        try:
            for line in sys.stdin:
                text = line.strip()
                if text == "stop":
                    break
                if text.startswith("{"):
                    self.command(text)
        finally:
            self.stop.set()
            self.end()

    def command(self, text):
        """Do what ter asked to the run, and answer under the same id."""
        try:
            asked = json.loads(text)
        except ValueError:
            return
        if not isinstance(asked, dict):
            return
        with self.lock:
            sim_run = self.sim_run
            # The main loop owns the run, so it makes the new one.
            if asked.get("do") == "reset" and sim_run is not None and self.reset is None:
                self.reset = asked.get("id")
                return
        try:
            if sim_run is None:
                raise Failed("not_started", "the run has not started")
            answer = do(sim_run, asked, self.events)
        except Exception as e:  # noqa: BLE001 - a refusal is an answer, never the end
            answer = {"ev": "refused", "message": str(e) or type(e).__name__}
        answer["id"] = asked.get("id")
        say(answer)


def do(run, asked, events):
    """One thing ter asks of a running board, done."""
    what = asked.get("do")
    if what == "pause":
        return {"ev": "paused", "t_ns": run.pause().time_ns}
    if what == "resume":
        return {"ev": "resumed", "t_ns": run.resume().time_ns}
    # An input or an event takes hold at once, at a board time a little
    # after the one read just before it is sent: what it brings about is
    # never before that time.
    if what == "input":
        t_ns = run.status().time_ns
        set_input(run, events, str(asked["part"]), str(asked["name"]), asked["value"])
        return {"ev": "input", "t_ns": t_ns}
    if what == "action":
        t_ns = run.status().time_ns
        run.event(str(asked["part"]), str(asked["action"]))
        return {"ev": "action", "t_ns": t_ns}
    raise Failed("internal", f"the helper cannot {what!r}")


def analog_nets(board, client, pins):
    """Which of these nets are analog: the net a wire joins it to holds a
    pin of a part that senses a continuous input (a potentiometer's
    position, a light or temperature sensor) and has no supply pin of its
    own, so the net carries the part's voltage rather than a digital
    signal. A part with a supply pin speaks a protocol; a divider of plain
    resistors is not told from a digital net."""
    parent = {}

    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    for a, b in getattr(board, "wires", None) or []:
        ra, rb = find(f"{a[0]}.{a[1]}"), find(f"{b[0]}.{b[1]}")
        if ra != rb:
            parent[ra] = rb
    sensing = set()
    for part in board.parts:
        try:
            entry = catalog(client, part.type)
        except Exception:  # noqa: BLE001 - an unknown part is not analog
            continue
        if any(is_power(p.get("name")) for p in entry.get("pins") or [] if isinstance(p, dict)):
            continue
        continuous = [
            spec for spec in entry.get("inputs") or []
            if isinstance(spec, dict)
            and isinstance(spec.get("min"), (int, float))
            and isinstance(spec.get("max"), (int, float))
            and not spec.get("widget")
            and not spec.get("axes")
            and not spec.get("event")
        ]
        if continuous:
            sensing.add(part.id)
    found = set()
    for pin in pins:
        if pin not in parent:
            continue
        root = find(pin)
        if any(find(other) == root and other.split(".", 1)[0] in sensing for other in list(parent)):
            found.add(pin)
    return found


class Tail:
    """One net of a running board: each look says what changed since the
    last one. A stretch too busy to come back edge by edge is not shown;
    the recording reads it in slices. A quiet tail says nothing: it keeps
    the levels for the net's volts to carry."""

    def __init__(self, pin, quiet=False):
        self.pin = pin
        self.to_ns = 0
        self.last_ns = -1
        self.level = None
        self.begun = False
        self.quiet = quiet
        # A quiet tail's levels, [(t_ns, level)], oldest first.
        self.changes = []

    def said(self, t, level):
        if self.quiet:
            self.changes.append((t, level))
        else:
            say({"ev": "pin", "t_ns": t, "net": self.pin, "level": level})

    def level_at(self, t):
        """The level the net had at `t`, as far as the looks said; None
        when they did not."""
        found = None
        for when, level in self.changes:
            if when > t:
                break
            found = level
        return found

    def forget(self, before):
        """Drop the levels no time from `before` on needs."""
        keep = 0
        for i, (when, _) in enumerate(self.changes):
            if when <= before:
                keep = i
        del self.changes[:keep]

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
                self.said(0, self.level)
        for edge in trace.get("edges") or []:
            t = int(edge.get("timeNs", -1))
            new = level_of(edge.get("state"))
            if new is None or t <= self.last_ns or t > to_ns:
                continue
            self.last_ns = t
            if new != self.level:
                self.level = new
                self.said(t, new)
        self.to_ns = max(self.to_ns, to_ns)


class Volts:
    """An analog net's voltage while the board runs: each look's exact
    samples, said as a "pin" event with "volts" when they change, at most
    once per VOLTS_EVERY_NS of simulated time. A change inside that time
    waits for its end, and only the newest such change is said."""

    def __init__(self, tail):
        self.tail = tail
        self.seen_ns = -1
        self.seen = None
        self.said_ns = None
        self.said = None
        self.pending = None
        self.receiver = {}

    def take(self, analog):
        if analog.get("mode") != "exact" or analog.get("buckets"):
            return
        if isinstance(analog.get("receiver"), dict):
            self.receiver = analog["receiver"]
        steps = {}
        for point in analog.get("points") or []:
            try:
                t, v = int(point[0]), round(float(point[1]), VOLTS_DECIMALS)
            except (TypeError, ValueError, IndexError):
                continue
            if t >= self.seen_ns:
                # The newest value at a time is the one that holds.
                steps.pop(t, None)
                steps[t] = v
        for t in sorted(steps):
            v = steps[t]
            if v != self.seen:
                self.change(t, v)
            self.seen_ns, self.seen = t, v
        to_ns = int(analog.get("toNs") or 0)
        self.flush(to_ns)
        self.tail.forget(self.pending[0] if self.pending is not None else min(self.seen_ns, to_ns))

    def slot(self):
        t, _ = self.pending
        return t if self.said_ns is None else max(t, self.said_ns + VOLTS_EVERY_NS)

    def change(self, t, v):
        if self.pending is not None and self.slot() <= t:
            self.say(self.slot(), self.pending[1])
            self.pending = None
        if self.said_ns is None or t >= self.said_ns + VOLTS_EVERY_NS:
            self.pending = None
            self.say(t, v)
        else:
            self.pending = (t, v)

    def flush(self, to_ns):
        if self.pending is not None and self.slot() <= to_ns:
            self.say(self.slot(), self.pending[1])
            self.pending = None

    def level(self, t, v):
        level = self.tail.level_at(t)
        if level is not None:
            return level
        try:
            if v >= float(self.receiver["vih"]):
                return 1
            if v <= float(self.receiver["vil"]):
                return 0
        except (KeyError, TypeError, ValueError):
            pass
        return self.said[1] if self.said is not None else 0

    def say(self, t, v):
        level = self.level(t, v)
        if (v, level) == self.said:
            return
        say({"ev": "pin", "t_ns": t, "net": self.tail.pin, "level": level, "volts": v})
        self.said = (v, level)
        self.said_ns = t


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


def signals(request, client):
    """The programmed part's pins and the chip signal each is, from
    sim86's catalog: {"D12": "PA6"}. Only names what a live run shows, so
    a catalog that does not answer leaves it empty."""
    try:
        with open(request["board"], encoding="utf-8") as f:
            board = json.load(f)
        mcu = next(iter(request.get("firmware") or {}))
        parts = board.get("parts") or board.get("components") or []
        kind = next(p["type"] for p in parts if p.get("id") == mcu)
        pins = catalog(client, kind).get("pins") or []
        return {str(p["name"]): str(p["signal"]) for p in pins if p.get("name") and p.get("signal")}
    except Exception:  # noqa: BLE001 - names are a nicety, never a failure
        return {}


def live(request):
    from sim86 import Run

    end_ns = int(request["timeout_ms"]) * 1_000_000
    watch = list(request.get("watch") or [])
    # Streamed beside the watched nets, never read into the recording: a
    # net shown only cannot fail or hold back the check's window.
    shown = [net for net in request.get("show") or [] if net not in watch]
    # ter can say stop while the run is still being made.
    told = Told()
    told.start()
    board, client, starts, events = opened(request)
    told.events = events
    # A board file may carry limits of its own, meant for sim86's command.
    # ter holds a live run's limit: no simulated-time limit at all.
    board.limits = {}
    names = signals(request, client) if shown else {}
    controls = part_controls(board, client, starts)
    try:
        analog = analog_nets(board, client, watch + shown)
    except Exception:  # noqa: BLE001 - volts are a nicety
        analog = set()
    if told.stop.is_set():
        raise Failed("stopped", "ter stopped the run before it started")

    def begin(shown):
        """A new run of the board, paused at 0 with its nets observed, and
        the shown nets sim86 took."""
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            new = Run.start(
                board, client,
                duration=None,
                realtime=float(request["realtime_ms"]),
                scenario=request.get("scenario") or None,
                paused=True,
            )
        told.attach(new)
        try:
            halt = new.status()
            if not halt.paused or halt.time_ns != 0:
                raise Failed("not_paused", f"the recording starts {halt.time_ns / 1e6:.3f} ms in")
            start_inputs(new, starts, events)
            if watch:
                observe(new, watch)
            if shown:
                try:
                    observe(new, shown)
                except Exception:  # noqa: BLE001 - shown nets are a nicety
                    shown = []
        except BaseException:
            try:
                new.stop()
            except Exception:
                pass
            raise
        return new, shown

    run, shown = begin(shown)
    try:
        observed = watch + shown
        say({"ev": "started", "run": run.id, "signals": names, "controls": controls})
        run.resume()

        def fresh():
            tails = {pin: Tail(pin, quiet=pin in analog) for pin in observed}
            volts = {pin: Volts(tails[pin]) for pin in observed if pin in analog}
            return tails, volts, {pin: Reader(run, pin, end_ns) for pin in watch}

        tails, volts, readers = fresh()
        read_to = 0
        lines_said = 0
        recorded = False
        now_ns = 0
        looked = time.monotonic()
        while True:
            if told.stop.wait(POLL_S):
                break
            reset = told.take_reset()
            if reset is not None:
                told.detach()
                try:
                    run.stop()
                except Exception:
                    pass
                try:
                    run, shown = begin(shown)
                except Exception as e:  # noqa: BLE001 - said, then the end of the run
                    say({"ev": "refused", "id": reset, "message": str(e) or type(e).__name__})
                    if told.stop.is_set():
                        break
                    raise
                if told.stop.is_set():
                    break
                observed = watch + shown
                say({"ev": "reset", "id": reset, "run": run.id})
                run.resume()
                tails, volts, readers = fresh()
                read_to = 0
                lines_said = 0
                recorded = False
                now_ns = 0
                looked = time.monotonic()
                continue
            try:
                if run.ended is None:
                    wall = time.monotonic()
                    if observed:
                        reach = int((wall - looked) * 1.5e9) + OVERLAP_NS + 500_000_000
                        snapshot = run._observe(min(MAX_WINDOW_NS, reach))
                        wires = snapshot.get("wires", [])
                        for wire in wires:
                            tail = tails.get(wire.get("id"))
                            trace = wire.get("wireTrace")
                            if tail is not None and isinstance(trace, dict) and "mode" in trace:
                                tail.take(trace)
                        # After every level of the look is in.
                        for wire in wires:
                            net = volts.get(wire.get("id"))
                            if net is not None and isinstance(wire.get("analog"), dict):
                                net.take(wire["analog"])
                    # How far the board is: the watched nets' looks, so a
                    # shown net that never comes back holds nothing up.
                    if watch:
                        now_ns = min(tails[pin].to_ns for pin in watch)
                    else:
                        now_ns = run.status().time_ns
                    looked = wall
                lines = run.serial.lines
                for line in lines[lines_said:]:
                    say({"ev": "serial", "t_ms": float(line.t), "source": str(line.source), "line": str(line.line)})
                lines_said = len(lines)
                # The recording is read as the board makes it, while sim86
                # still holds every piece, and a little behind the board.
                ready = min(end_ns, now_ns - TAIL_NS)
                if not recorded and run.ended is None and (ready >= end_ns or ready - read_to >= READ_AHEAD_NS):
                    for reader in readers.values():
                        reader.upto(ready)
                    read_to = ready
                if not recorded and run.ended is None and read_to >= end_ns:
                    say(recording(run, watch, end_ns, {pin: reader.joined() for pin, reader in readers.items()}))
                    recorded = True
            except Exception:
                # A run that ended, or was stopped, under a request is an
                # ended run.
                if run.ended is None and not told.stop.is_set():
                    raise
            if run.ended is not None:
                break
        ended = run.ended
        if told.stop.is_set():
            reason = "stopped"
        elif ended in (None, "disconnected"):
            reason = "disconnected"
        else:
            reason = str(ended)
        say({"ev": "ended", "reason": reason})
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


def view(run_id):
    """The address of sim86's own page for a run, signed in the way the
    package's `open_live` does it: the key rides in the #fragment, which a
    browser never sends to a server."""
    import urllib.parse

    from sim86 import Client

    client = Client()
    key = client.api_key
    if not key:
        raise Failed("no_key", "sim86 has no key on this machine")
    return {"view": f"{client.live_url(run_id)}#{urllib.parse.urlencode({'key': key})}"}


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
        elif mode == "view" and len(argv) > 2:
            out.update(view(argv[2]))
        elif mode == "version":
            out["key"] = has_key()
        else:
            raise Failed("internal", "usage: ter_sim86.py run <request.json> | live <request.json> | balance | view <run> | version")
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

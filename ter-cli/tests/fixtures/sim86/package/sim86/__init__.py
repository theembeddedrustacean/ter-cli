"""A stand-in for the sim86 package, for ter's tests: ter's helper runs
against it as it runs against sim86's own, with no network.

Every board is a heartbeat on mcu.D1, two 100 ms flashes a second from
143 ms in, with a boot banner and a line a second on its serial port. As
sim86 does, it answers for windows of at most 10 s, holds the last 10 s
of a net only, and keeps a paused board at its time.

It keeps what it is asked in the folder $FAKE_SIM86_DIR, where files
change what it does:

    speed         how fast a running board goes against the wall clock (1)
    hang-observe  a look at a running board never answers
    slow-start    how many seconds a run takes to be made

and where it writes `runs` (a line per run made) and `stopped` (a line per
run stopped through the account, as `stop_run` does).
"""
import json
import os
import threading
import time

__version__ = "0.2.0"

LOW, HIGH = 1, 2
KEEPS_NS = 10_000_000_000
# Where in each second the LED changes, and to what.
BEAT = [(143_000_000, 1), (243_000_000, 0), (343_000_000, 1), (443_000_000, 0)]

_runs = {}
_lock = threading.Lock()


class Sim86Error(Exception):
    pass


class AuthError(Sim86Error):
    pass


class ApiError(Sim86Error):
    def __init__(self, message, status=None):
        super().__init__(message)
        self.status = status


class CircuitRefused(ApiError):
    pass


class OutOfCredits(ApiError):
    pass


class RunEnded(Sim86Error):
    pass


def _here():
    return os.environ["FAKE_SIM86_DIR"]


def _setting(name):
    path = os.path.join(_here(), name)
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as f:
        return f.read().strip()


def _note(name, line):
    with _lock, open(os.path.join(_here(), name), "a", encoding="utf-8") as f:
        f.write(line + "\n")


class Client:
    def __init__(self):
        self.api_key = os.environ.get("SIM86_API_KEY")

    def live_url(self, run_id):
        return f"https://app.sim86.com/live/{run_id}"

    def gql(self, query, variables=None):
        return {"getSubscriptionDetails": {"creditBalanceCents": 20184, "plan": "free", "isActive": True}}

    def component(self, type_name):
        catalog = _setting("catalog")
        if catalog is None:
            raise ApiError(f"no part {type_name}", status=404)
        return {"type": type_name, "pins": json.loads(catalog)}

    def stop_run(self, run_id):
        _note("stopped", run_id)
        run = _runs.get(run_id)
        if run is not None:
            run._end("stopped")


class Board:
    def __init__(self, data):
        self.name = data.get("name", "")
        self.limits = data.get("limits") or {}


def load_board(path, firmware_overrides=None):
    with open(path, encoding="utf-8") as f:
        data = json.load(f)
    for part, image in (firmware_overrides or {}).items():
        if not os.path.isfile(image):
            raise Sim86Error(f"{part}: no firmware at {image}")
    return Board(data)


def check_board(board, client):
    return []


class Halt:
    def __init__(self, paused, time_ns):
        self.paused, self.time_ns = paused, time_ns


class LogLine:
    def __init__(self, t, source, line):
        self.t, self.source, self.line = t, source, line


class Serial:
    def __init__(self, run):
        self.run = run

    @property
    def lines(self):
        now_ms = self.run._now() / 1e6
        said = [LogLine(0.0, "mcu:usbserial", "ESP-ROM:esp32c3-api1-20210207")]
        s = 0
        while s * 1000 + 500 <= now_ms:
            said.append(LogLine(float(s * 1000 + 500), "mcu:usbserial", f"beat {s}"))
            s += 1
        return said


def _level(t):
    """The LED's level just after time t."""
    level = 0
    second, into = divmod(t, 1_000_000_000)
    for at, new in BEAT:
        if into >= at:
            level = new
    return level


def _edges(a, b):
    """The LED's edges in (a, b]."""
    out = []
    second = a // 1_000_000_000
    while second * 1_000_000_000 <= b:
        for at, new in BEAT:
            t = second * 1_000_000_000 + at
            if a < t <= b:
                out.append({"timeNs": t, "state": HIGH if new else LOW})
        second += 1
    return out


class Run:
    def __init__(self, client, run_id, paused):
        self.client, self.id = client, run_id
        self._base_ns = 0
        self._paused = paused
        self._resumed = time.monotonic()
        self._ended = None
        self._observed = []
        self._since_ns = None
        self.plan_events = []
        self.speed = float(_setting("speed") or 1)

    @classmethod
    def start(cls, board, client=None, *, duration=None, realtime=None, scenario=None, seed=None, paused=False):
        slow = _setting("slow-start")
        if slow:
            time.sleep(float(slow))
        with _lock:
            run_id = f"fake-run-{len(_runs) + 1}"
        run = cls(client or Client(), run_id, paused)
        _runs[run_id] = run
        _note("runs", json.dumps({"id": run_id, "duration": duration, "realtime": realtime, "paused": paused}))
        return run

    def _now(self):
        with _lock:
            if self._paused or self._ended is not None:
                return self._base_ns
            return self._base_ns + int((time.monotonic() - self._resumed) * self.speed * 1e9)

    def _end(self, reason):
        now = self._now()
        with _lock:
            if self._ended is None:
                self._base_ns, self._ended = now, reason

    def _alive(self):
        if self._ended is not None:
            raise RunEnded(f"run {self.id} has ended ({self._ended})")

    @property
    def ended(self):
        return self._ended

    @property
    def serial(self):
        return Serial(self)

    def status(self):
        self._alive()
        return Halt(self._paused, self._now())

    def pause(self):
        self._alive()
        now = self._now()
        with _lock:
            self._base_ns, self._paused = now, True
        return Halt(True, now)

    def resume(self):
        self._alive()
        now = self._now()
        with _lock:
            self._base_ns, self._paused, self._resumed = now, False, time.monotonic()
        return Halt(False, now)

    def until(self, edge=None, *, kind="any", nth=1, at=None):
        self._alive()
        at_ns = max(self._now(), int(float(at) * 1e6))
        with _lock:
            self._base_ns, self._paused = at_ns, True
        return Halt(True, at_ns)

    def observe(self, *pins):
        self._alive()
        refused = _setting("refuse-observe")
        if refused is not None and refused in pins:
            raise Sim86Error(f"no net {refused}")
        if len(set(self._observed) | set(pins)) > 64:
            raise Sim86Error("at most 64 nets can be observed at once")
        _note("observed", json.dumps(list(pins)))
        if self._since_ns is None:
            self._since_ns = self._now()
        self._observed.extend(p for p in pins if p not in self._observed)

    def watch(self, *pins):
        self.observe(*pins)

    def _observe(self, window_ns, end_ns=None, pixels=1024, measure=None):
        self._alive()
        if not 1 <= int(window_ns) <= KEEPS_NS:
            raise Sim86Error("Observe view requires windowNs 1..10000000000")
        if _setting("hang-observe") is not None and not self._paused:
            while True:
                time.sleep(0.1)
        now = self._now()
        to = now if end_ns is None else min(int(end_ns), now)
        start = max(0, to - int(window_ns))
        held = max(self._since_ns or 0, now - KEEPS_NS)
        wires = []
        for pin in self._observed:
            beat = pin == "mcu.D1"
            wires.append({"id": pin, "wireTrace": {
                "mode": "exact",
                "fromNs": start,
                "toNs": to,
                "exactFromNs": held,
                "availableFromNs": held,
                "initialState": HIGH if beat and _level(start) else LOW,
                "edges": _edges(start, to) if beat else [],
            }})
        return {"simulationTimeNs": now, "wires": wires}

    def stop(self):
        if self._ended is None:
            self.client.stop_run(self.id)

    def close(self):
        pass

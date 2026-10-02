"""Local application JSON-lines bridge. Only explicit connect may attach.

Uses the independent Client unchanged. Values are normalized; cache publication
and process consumption are diagnostics, never proof of audible correction.
"""
from __future__ import annotations
import copy
import ctypes
import json
import ntpath
from pathlib import Path
import secrets
import sys

import client

CONTINUOUS_ROLES = ("retune", "flex", "vibrato", "humanize")
DISCRETE_ROLES = ("key", "scale")  # option labels declared by the profile, never free values
ROLES = CONTINUOUS_ROLES + DISCRETE_ROLES
MAX_LINE = 65536


def empty_state(error=None):
    return {"phase": "error" if error else "disconnected", "connectionId": None,
            "target": None, "capabilities": [], "instanceCount": 0,
            "delivery": None, "error": error, "audioVerified": False}


class AppBridge:
    def __init__(self, *, profiles=None, scan_fn=None, identity_reader=None,
                 module_reader=None, transport=None):
        self.profiles = [client.Profile.load(p) for p in (
            profiles if profiles is not None else sorted((Path(__file__).parent / "profiles").glob("*.json")))]
        self.scan_fn = scan_fn or client.scan
        self.identity_reader = identity_reader or client.process_identity
        self.module_reader = module_reader or client.modules
        self.transport = transport
        self.candidates = {}
        self.controller = None
        self.selected = None
        self.sequence = 0
        self.revision = None
        self.state = empty_state()

    def _close(self, error=None):
        self.controller = self.selected = None
        self.sequence = 0
        self.revision = None
        self.state = empty_state(error)

    def _guard(self, candidate):
        expected = candidate["identity"]
        current = self.identity_reader(expected["pid"])
        client._same_identity(expected, current)
        if current.get("same_user") is not True or expected.get("same_user") is not True:
            raise RuntimeError("Target user identity changed")
        loaded = self.module_reader(expected["pid"])
        client.validate_target(expected["pid"], loaded)
        agent = ntpath.normcase(str((Path(__file__).parent / "build" /
                                   candidate["profile"].architecture / "reference_agent.dll").resolve()))
        if any(m["name"].lower() == "reference_agent.dll"
               and ntpath.normcase(ntpath.abspath(m["path"])) != agent for m in loaded):
            raise RuntimeError("A different reference agent is loaded")
        path = ntpath.normcase(ntpath.abspath(candidate["path"]))
        if not any(ntpath.normcase(ntpath.abspath(m["path"])) == path for m in loaded):
            raise RuntimeError("Selected plugin was unloaded")
        candidate["profile"].verify_file()
        client._same_identity(expected, self.identity_reader(expected["pid"]))

    def _scan(self):
        self.candidates = {}
        result = self.scan_fn()
        if not result.get("ok"):
            raise RuntimeError(result.get("error", "Scan failed"))
        visible = []
        skipped = len(result.get("errors", []))
        for target in result["targets"]:
            identity = target["identity"]
            for module in target["plugins"]:
                name, path = module["name"], module["path"]
                profile, reason = None, None
                try:
                    profile = client.select_profile(self.profiles, path, identity["architecture"])
                    if identity.get("same_user") is not True:
                        raise ValueError("Only same-user local targets are supported")
                    client.validate_target(identity["pid"], target["plugins"])
                    if self.transport is None and (identity["architecture"] != "x64" or ctypes.sizeof(ctypes.c_void_p) != 8):
                        raise ValueError("This debug loader supports x64 targets with x64 Python only")
                except (OSError, ValueError, RuntimeError) as exc:
                    reason = str(exc)
                if profile is None and "autotune" not in name.lower().replace("-", "").replace(" ", ""):
                    continue
                token = secrets.token_urlsafe(24)
                public = {"candidateId": token, "pid": identity["pid"],
                          "processName": ntpath.basename(identity["executable"]), "pluginName": name,
                          "profileId": profile.profile_id if profile else None,
                          "compatible": profile is not None and reason is None, "reason": reason}
                visible.append(public)
                self.candidates[token] = {"identity": dict(identity), "path": path,
                                          "profile": profile, "public": public}
        return {"candidates": visible, "skippedCount": skipped}

    def _status(self):
        if self.controller is None:
            return
        self._guard(self.selected)
        status = self.controller.status()
        if status.get("route") != "direct_process_queue":
            raise RuntimeError("Unexpected agent delivery route")
        if status.get("instance_overflow", 0) != 0:
            raise RuntimeError("Plugin instance tracking overflow; cannot confirm all instances")
        group = next((g for g in status.get("groups", []) if g["group"] == self.controller.group), None)
        if group is None:
            raise RuntimeError("Prepared profile group is missing")
        instances = [i for i in status.get("instances", [])
                     if i.get("group") == self.controller.group and i.get("state") == "matched"]
        if any(i.get("state") == "ambiguous" for i in status.get("instances", [])):
            raise RuntimeError("Conflicting plugin instance identification")
        self.state["instanceCount"] = len(instances)
        self.state["phase"] = "ready" if instances else "awaiting"
        self.state["error"] = None
        if group.get("snapshot_available") is not True:
            # A failed try-read is not a zero revision or an external mutation.
            self.state["phase"] = "awaiting"
            if self.state["delivery"] is not None:
                self.state["delivery"]["stage"] = "cached"
            return False
        if self.revision is not None:
            if group.get("cache_revision") != self.revision:
                raise RuntimeError("Parameter cache changed outside this connection")
            consumed = bool(instances) and all(
                i.get("consumed_revision") == self.revision and i.get("submitted", 0) > 0
                and i.get("completed_revision") == self.revision
                and i.get("completion_result") == 0 for i in instances)
            self.state["delivery"]["stage"] = "submitted" if consumed else "cached"
        return True

    def command(self, request):
        try:
            if not isinstance(request, dict):
                raise ValueError("Request must be an object")
            op = request.get("op")
            fields = {"status": {"op"}, "scan": {"op"}, "disconnect": {"op"},
                      "connect": {"op", "candidateId"},
                      "options": {"op", "candidateId", "role"},
                      "clear": {"op", "connectionId"},
                      "apply": {"op", "connectionId", "sequence", "values"}}
            if op not in fields or set(request) != fields[op]:
                raise ValueError("Unknown operation or unexpected/missing request fields")
            if op in ("apply", "clear") and self.controller is not None and request["connectionId"] != self.state["connectionId"]:
                return {"ok": False, "error": "Connection generation expired; discard this request",
                        "state": copy.deepcopy(self.state)}
            extra = {}
            if op == "disconnect":
                self._close()
            elif op == "scan":
                extra = self._scan()
            elif op == "options":
                role = request["role"]
                if role not in ("key", "scale"):
                    raise ValueError("Only key and scale options are readable")
                token = request["candidateId"]
                if not isinstance(token, str) or token not in self.candidates:
                    raise ValueError("Candidate expired; scan and explicitly select again")
                selected = self.candidates[token]
                if not selected["public"]["compatible"]:
                    raise ValueError(selected["public"]["reason"] or "Unsupported plugin")
                extra = {"options": selected["profile"].options(role)}
            elif op == "connect":
                self._close()
                token = request["candidateId"]
                if not isinstance(token, str) or token not in self.candidates:
                    raise ValueError("Candidate expired; scan and explicitly select again")
                selected = self.candidates[token]
                if not selected["public"]["compatible"]:
                    raise ValueError(selected["public"]["reason"] or "Unsupported plugin")
                self._guard(selected)
                self.selected = selected
                self.controller = client.Client(selected["identity"]["pid"], selected["profile"],
                    identity_reader=self.identity_reader, module_reader=self.module_reader,
                    transport=self.transport)
                # Seed scan identity before attach so the Client also guards the attach race.
                self.controller.identity = dict(selected["identity"])
                attached = self.controller.attach()
                if attached.get("pending"):
                    raise RuntimeError("Plugin changed during connect; scan again")
                p = selected["public"]
                self.state = empty_state()
                self.state.update(phase="awaiting", connectionId=secrets.token_urlsafe(24),
                    target={k: p[k] for k in ("pid", "processName", "pluginName", "profileId")},
                    capabilities=[r for r in ROLES if r in selected["profile"].roles])
                self._status()
            elif op == "status":
                self._status()
            elif op == "apply":
                if self.controller is None or request["connectionId"] != self.state["connectionId"]:
                    raise ValueError("Connection expired; explicitly reconnect")
                sequence = request["sequence"]
                if type(sequence) is not int or not self.sequence < sequence <= 2**53 - 1:
                    raise ValueError("sequence must be a strictly increasing positive JS safe integer")
                values = request["values"]
                if not isinstance(values, dict) or not values or any(r not in self.state["capabilities"] for r in values):
                    raise ValueError("values must contain only supported parameter roles")
                for role, value in values.items():
                    if role in DISCRETE_ROLES:
                        # Profile.map_values resolves the label; unknown labels fail closed there.
                        if not isinstance(value, str) or not 0 < len(value) <= 32:
                            raise ValueError(f"{role} must be a declared option label")
                    else:
                        client._normalized(value)
                if not self._status():
                    return {"ok": False, "error": "Agent cache snapshot unavailable; retry",
                            "state": copy.deepcopy(self.state)}
                if self.state["phase"] != "ready":
                    raise RuntimeError("No matched plugin instance; wait for ready before applying")
                self.sequence = sequence
                response = self.controller.apply(values)
                if response.get("stage") != "cached" or response.get("duplicate") is not False:
                    raise RuntimeError("Unexpected cache acknowledgement")
                self.revision = client._uint(response.get("cache_revision"), 64, "cache revision", minimum=1)
                self.state["delivery"] = {"stage": "cached", "sequence": sequence}
            elif op == "clear":
                if self.controller is None or request["connectionId"] != self.state["connectionId"]:
                    raise ValueError("Connection expired; explicitly reconnect")
                if not self._status():
                    return {"ok": False, "error": "Agent cache snapshot unavailable; retry",
                            "state": copy.deepcopy(self.state)}
                if self.state["phase"] != "ready":
                    raise RuntimeError("No matched plugin instance; wait for ready before clearing")
                response = self.controller.clear()
                if (response.get("ok") is not True or response.get("cleared") is not True
                        or response.get("dsp_reset") is not False):
                    raise RuntimeError("Unexpected clear acknowledgement")
                self.revision = None
                self.state["delivery"] = None
                extra = {"cleared": True, "dspReset": False}
            return {"ok": True, "state": copy.deepcopy(self.state), **extra}
        except (OSError, ValueError, RuntimeError, TypeError, KeyError) as exc:
            error = str(exc)
            if isinstance(request, dict) and request.get("op") in ("scan", "options") and self.controller is not None:
                self.state["error"] = error
                return {"ok": False, "error": error, "state": copy.deepcopy(self.state)}
            self._close(error)
            return {"ok": False, "error": error, "state": copy.deepcopy(self.state)}


def main():
    bridge = AppBridge()
    while True:
        line = sys.stdin.buffer.readline(MAX_LINE + 1)
        if not line:
            return 0
        if len(line) > MAX_LINE or not line.endswith(b"\n"):
            return 2
        try:
            request = json.loads(line)
            result = bridge.command(request)
        except (ValueError, UnicodeError) as exc:
            bridge._close(str(exc))
            result = {"ok": False, "error": str(exc), "state": copy.deepcopy(bridge.state)}
        encoded = json.dumps(result, ensure_ascii=True, allow_nan=False)
        if len(encoded) > MAX_LINE - 1:
            bridge._close("Response exceeds size limit")
            encoded = json.dumps({"ok": False, "error": bridge.state["error"], "state": bridge.state})
        print(encoded, flush=True)


if __name__ == "__main__":
    raise SystemExit(main())

"""Optional HTTP client for the versioned, loopback UvCraft bridge.

Only UV loop data is applied back to Blender. Geometry and object data stay in
their original datablocks. This module has no bpy import so it can be tested
without Blender.
"""

from __future__ import annotations

import json
import math
from pathlib import Path
from tempfile import TemporaryDirectory
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

from . import bridge


class RemoteError(bridge.BridgeError):
    pass


PROTOCOL_VERSION = 1
MAX_RESPONSE = 16 * 1024 * 1024


def _loopback_url(url):
    parsed = urlsplit(url)
    if parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "localhost", "::1"}:
        raise RemoteError("bridge URL must use HTTP on a loopback address")
    if parsed.username or parsed.password or parsed.path not in {"", "/"}:
        raise RemoteError("bridge URL must contain only the local host and port")
    return url.rstrip("/")


class Client:
    def __init__(self, url="http://127.0.0.1:8766", token="", opener=urlopen):
        self.url = _loopback_url(url)
        self.token = token.strip()
        self._open = opener

    def request(self, method, path, payload=None):
        data = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
        headers = {"Accept": "application/json"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        request = Request(self.url + path, data=data, headers=headers, method=method)
        try:
            with self._open(request, timeout=15) as response:
                raw = response.read(MAX_RESPONSE + 1)
        except HTTPError as exc:
            try:
                body = json.loads(exc.read(MAX_RESPONSE + 1))
                message = body["error"]["message"]
            except (ValueError, KeyError, TypeError):
                message = str(exc)
            raise RemoteError(message) from exc
        except (URLError, TimeoutError) as exc:
            raise RemoteError(f"cannot reach UvCraft bridge: {exc}") from exc
        if len(raw) > MAX_RESPONSE:
            raise RemoteError("bridge response is too large")
        try:
            return json.loads(raw)
        except ValueError as exc:
            raise RemoteError("bridge returned invalid JSON") from exc

    def connect(self):
        health = self.request("GET", "/v1/health")
        if health.get("protocol_version") != PROTOCOL_VERSION:
            raise RemoteError("incompatible UvCraft bridge protocol")
        self.request("GET", "/v1/status")  # Also verifies the session token.
        return health

    def _mesh_obj(self, mesh, preserve_existing_uvs=True):
        with TemporaryDirectory(prefix="uvcraft-blender-remote-") as directory:
            path = Path(directory) / "mesh.obj"
            bridge.write_obj(mesh, path, preserve_existing_uvs=preserve_existing_uvs)
            return path.read_text(encoding="utf-8")

    @staticmethod
    def _seams(mesh):
        return [list(map(int, edge.vertices)) for edge in getattr(mesh, "edges", [])
                if edge.use_seam]

    def send(self, mesh, object_id, addon_version="0.1.0", preserve_existing_uvs=True):
        return self.request("POST", "/v1/sessions", {
            "protocol_version": PROTOCOL_VERSION,
            "addon_version": addon_version,
            "object_id": str(object_id),
            "obj": self._mesh_obj(mesh, preserve_existing_uvs),
            "seams": self._seams(mesh),
        })["session_id"]

    def update(self, session_id, mesh, preserve_existing_uvs=True):
        return self.request("POST", f"/v1/sessions/{session_id}/mesh", {
            "obj": self._mesh_obj(mesh, preserve_existing_uvs),
            "seams": self._seams(mesh),
        })

    def status(self, session_id):
        return self.request("GET", f"/v1/sessions/{session_id}/status")

    def receive(self, session_id, mesh):
        return self.receive_data(session_id, mesh)[0]

    def receive_data(self, session_id, mesh):
        data = self.request("GET", f"/v1/sessions/{session_id}/uv")
        faces, _, _, _ = bridge.snapshot(mesh)
        expected = [[mesh.loops[loop].vertex_index for loop in face] for face in faces]
        topo = data.get("topology", {})
        if topo.get("vertices") != len(mesh.vertices) or topo.get("faces") != expected:
            raise RemoteError("mesh topology changed; UV import cancelled")
        uvs = data.get("uvs")
        if not isinstance(uvs, list) or len(uvs) != len(mesh.loops):
            raise RemoteError("bridge returned an invalid UV corner count")
        assignments = []
        corner = 0
        for face in faces:
            for loop in face:
                point = uvs[corner]
                if (not isinstance(point, list) or len(point) != 2
                        or not all(isinstance(value, (int, float)) and math.isfinite(value)
                                   for value in point)):
                    raise RemoteError("bridge returned invalid UV coordinates")
                assignments.append((loop, tuple(point)))
                corner += 1
        raw_seams = data.get("seams", [])
        if not isinstance(raw_seams, list):
            raise RemoteError("bridge returned invalid seams")
        seams = set()
        for pair in raw_seams:
            if (not isinstance(pair, list) or len(pair) != 2
                    or any(not isinstance(value, int) or value < 0 for value in pair)):
                raise RemoteError("bridge returned invalid seams")
            seams.add(tuple(sorted(pair)))
        edges = {tuple(sorted(edge.vertices)) for edge in getattr(mesh, "edges", [])}
        if seams - edges:
            raise RemoteError("bridge seam topology differs from the Blender mesh")
        return assignments, seams

    @staticmethod
    def apply_seams(mesh, seams):
        for edge in getattr(mesh, "edges", []):
            edge.use_seam = tuple(sorted(edge.vertices)) in seams

    def disconnect(self, session_id):
        return self.request("DELETE", f"/v1/sessions/{session_id}")

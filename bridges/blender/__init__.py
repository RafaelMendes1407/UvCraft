"""UvCraft bridge for Blender. Install this directory as a Blender add-on."""

bl_info = {
    "name": "UvCraft UV Bridge",
    "author": "UvCraft contributors",
    "version": (0, 1, 0),
    "blender": (3, 6, 0),
    "location": "View3D > Sidebar > UvCraft",
    "description": "Edit selected mesh UVs through the UvCraft Python module",
    "category": "UV",
}

import bpy
from bpy.props import BoolProperty, EnumProperty, PointerProperty, StringProperty

from . import bridge, remote


REMOTE_SESSIONS = {}
REMOTE_STATUS = {"connected": False, "last_sync": "Never", "topology": "Not checked"}


def _remote_client(settings):
    return remote.Client(settings.bridge_url, settings.bridge_token)


def _selected_meshes(context):
    meshes = {}
    for obj in context.selected_objects:
        if obj.type == "MESH":
            key = obj.data.as_pointer() if hasattr(obj.data, "as_pointer") else id(obj.data)
            meshes.setdefault(key, (obj.name, obj.data))
    return meshes


def _remote_operation(context, action):
    settings = context.scene.uvcraft_bridge_settings
    client = _remote_client(settings)
    client.connect()
    meshes = _selected_meshes(context)
    if not meshes:
        raise remote.RemoteError("select at least one mesh object")
    staged = []
    try:
        for key, (name, mesh) in meshes.items():
            if action == "send":
                session_id = client.send(mesh, name, ".".join(map(str, bl_info["version"])),
                                         settings.preserve_existing_uvs)
                staged.append((key, session_id))
            else:
                session_id = REMOTE_SESSIONS.get(key)
                if session_id is None:
                    raise remote.RemoteError(f"{name} has no UvCraft session; send it first")
                if action == "update":
                    client.update(session_id, mesh, settings.preserve_existing_uvs)
                elif action == "receive":
                    staged.append((mesh, *client.receive_data(session_id, mesh)))
    except Exception:
        if action == "send":
            for _, session_id in staged:
                try:
                    client.disconnect(session_id)
                except Exception:
                    pass
        raise
    if action == "send":
        previous = [REMOTE_SESSIONS[key] for key, _ in staged if key in REMOTE_SESSIONS]
        REMOTE_SESSIONS.update(staged)
        for session_id in previous:
            try:
                client.disconnect(session_id)
            except Exception:
                pass
    elif action == "receive":
        for mesh, assignments, seams in staged:
            bridge.apply(mesh, assignments)
            client.apply_seams(mesh, seams)
    REMOTE_STATUS["connected"] = True
    REMOTE_STATUS["topology"] = "Compatible"
    from datetime import datetime
    REMOTE_STATUS["last_sync"] = datetime.now().strftime("%H:%M:%S")
    return len(meshes)


def _refresh_remote_status(context):
    client = _remote_client(context.scene.uvcraft_bridge_settings)
    try:
        client.connect()
    except Exception:
        REMOTE_STATUS["connected"] = False
        raise
    meshes = _selected_meshes(context)
    if not meshes:
        raise remote.RemoteError("select at least one mesh object")
    for key, (name, mesh) in meshes.items():
        session_id = REMOTE_SESSIONS.get(key)
        if session_id is None:
            raise remote.RemoteError(f"{name} has no UvCraft session; send it first")
        status = client.status(session_id)
        faces, _, _, _ = bridge.snapshot(mesh)
        expected = [[mesh.loops[loop].vertex_index for loop in face] for face in faces]
        topology = status.get("topology", {})
        if topology.get("vertices") != len(mesh.vertices) or topology.get("faces") != expected:
            REMOTE_STATUS["topology"] = "Mismatch"
            raise remote.RemoteError(f"{name} topology differs from its UvCraft session")
    REMOTE_STATUS.update(connected=True, topology="Compatible")
    return len(meshes)


class UVCRAFT_PG_bridge(bpy.types.PropertyGroup):
    bridge_url: StringProperty(name="Bridge URL", default="http://127.0.0.1:8766")
    bridge_token: StringProperty(name="Session token", subtype="PASSWORD")
    preserve_existing_uvs: BoolProperty(name="Preserve Existing UVs", default=True)
    preset: EnumProperty(
        name="Commands",
        items=[
            ("PACK", "Pack", "Pack existing UV islands"),
            ("UNFOLD_PACK", "Unfold + Pack", "Unfold UV islands, then pack"),
            ("AUTO_SEAMS_UNFOLD_PACK", "Auto Seams + Unfold + Pack", "Cut seams, unfold and pack"),
            ("CUSTOM", "Custom JSON", "Execute one or more UvCraft commands from JSON"),
        ],
        default="PACK",
    )
    commands_json: StringProperty(
        name="Command JSON",
        description='Example: [{"cmd":"unfold"},{"cmd":"pack"}]',
        default='[{"cmd":"unfold"},{"cmd":"pack"}]',
    )
    selection_scope: EnumProperty(
        name="Apply to",
        items=[
            ("ALL", "All UVs", "Apply to all loops of each selected mesh"),
            ("FACES", "Selected faces", "Apply to selected face loops"),
            ("UVS", "Selected UVs", "Apply only to selected UV corners"),
        ],
        default="ALL",
    )


class UVCRAFT_OT_run(bpy.types.Operator):
    bl_idname = "uvcraft.run"
    bl_label = "Run UvCraft"
    bl_description = "Round-trip selected mesh UVs through UvCraft while preserving face and corner order"
    bl_options = {"REGISTER", "UNDO"}

    @classmethod
    def poll(cls, context):
        return context.mode in {"OBJECT", "EDIT_MESH"} and any(
            obj.type == "MESH" for obj in context.selected_objects
        )

    def execute(self, context):
        settings = context.scene.uvcraft_bridge_settings
        was_edit = context.mode == "EDIT_MESH"
        switched = False
        try:
            if was_edit:
                bpy.ops.object.mode_set(mode="OBJECT")
                switched = True
            objects = [obj for obj in context.selected_objects if obj.type == "MESH"]
            # Linked objects share one mesh datablock; edit that datablock once.
            meshes = {}
            for obj in objects:
                key = obj.data.as_pointer() if hasattr(obj.data, "as_pointer") else id(obj.data)
                meshes.setdefault(key, obj.data)
            staged = []
            for mesh in meshes.values():
                selected = {poly.index for poly in mesh.polygons if poly.select} if settings.selection_scope == "FACES" else None
                selected_loops = bridge.selected_uv_loops(mesh) if settings.selection_scope == "UVS" else None
                assignments = bridge.run(
                    mesh, settings.preset, settings.commands_json,
                    selected_faces=selected, selected_loops=selected_loops,
                )
                staged.append((mesh, assignments))
            for mesh, assignments in staged:
                bridge.apply(mesh, assignments)
            self.report({"INFO"}, f"UvCraft updated UVs on {len(staged)} mesh datablock(s)")
            return {"FINISHED"}
        except ImportError as exc:
            self.report({"ERROR"}, f"UvCraft Python module is unavailable: {exc}")
            return {"CANCELLED"}
        except Exception as exc:
            self.report({"ERROR"}, f"UvCraft bridge: {exc}")
            return {"CANCELLED"}
        finally:
            if switched:
                bpy.ops.object.mode_set(mode="EDIT")


class UVCRAFT_PT_bridge(bpy.types.Panel):
    bl_label = "UvCraft"
    bl_idname = "UVCRAFT_PT_bridge"
    bl_space_type = "VIEW_3D"
    bl_region_type = "UI"
    bl_category = "UvCraft"

    def draw(self, context):
        layout = self.layout
        settings = context.scene.uvcraft_bridge_settings
        layout.prop(settings, "preset")
        if settings.preset == "CUSTOM":
            layout.prop(settings, "commands_json")
        layout.prop(settings, "selection_scope")
        layout.operator("uvcraft.run", icon="UV")
        layout.separator()
        layout.label(text="Connection")
        layout.prop(settings, "bridge_url")
        layout.prop(settings, "bridge_token")
        layout.label(text="Connected" if REMOTE_STATUS["connected"] else "Disconnected")
        row = layout.row(align=True)
        row.operator("uvcraft.remote_connect", text="Connect")
        row.operator("uvcraft.remote_disconnect", text="Disconnect")
        layout.operator("uvcraft.remote_status", text="Refresh Status")
        layout.label(text="Mesh Transfer")
        row = layout.row(align=True)
        row.operator("uvcraft.remote_send", text="Send Selected")
        row.operator("uvcraft.remote_update", text="Update Mesh")
        layout.operator("uvcraft.remote_receive", text="Receive UVs")
        layout.label(text="Synchronization")
        layout.prop(settings, "preserve_existing_uvs")
        layout.label(text="Validate Topology: On")
        layout.label(text="Auto Sync: unavailable")
        layout.label(text=f"Objects: {len(REMOTE_SESSIONS)}")
        layout.label(text=f"Active: {context.active_object.name if context.active_object else 'None'}")
        layout.label(text=f"Last sync: {REMOTE_STATUS['last_sync']}")
        layout.label(text=f"Topology: {REMOTE_STATUS['topology']}")


class UVCRAFT_OT_remote_connect(bpy.types.Operator):
    bl_idname = "uvcraft.remote_connect"
    bl_label = "Connect to UvCraft"

    def execute(self, context):
        try:
            info = _remote_client(context.scene.uvcraft_bridge_settings).connect()
            REMOTE_STATUS["connected"] = True
            self.report({"INFO"}, f"UvCraft bridge {info['app_version']} connected")
            return {"FINISHED"}
        except Exception as exc:
            REMOTE_STATUS["connected"] = False
            self.report({"ERROR"}, str(exc))
            return {"CANCELLED"}


class UVCRAFT_OT_remote_disconnect(bpy.types.Operator):
    bl_idname = "uvcraft.remote_disconnect"
    bl_label = "Disconnect UvCraft"

    def execute(self, context):
        client = _remote_client(context.scene.uvcraft_bridge_settings)
        try:
            for session_id in list(REMOTE_SESSIONS.values()):
                client.disconnect(session_id)
        except Exception as exc:
            self.report({"ERROR"}, str(exc))
            return {"CANCELLED"}
        REMOTE_SESSIONS.clear()
        REMOTE_STATUS.update(connected=False, topology="Not checked")
        return {"FINISHED"}


class UVCRAFT_OT_remote_status(bpy.types.Operator):
    bl_idname = "uvcraft.remote_status"
    bl_label = "Get Synchronization Status"

    def execute(self, context):
        try:
            count = _refresh_remote_status(context)
            self.report({"INFO"}, f"UvCraft: {count} mesh datablock(s) compatible")
            return {"FINISHED"}
        except Exception as exc:
            self.report({"ERROR"}, str(exc))
            return {"CANCELLED"}


class _RemoteMeshOperator(bpy.types.Operator):
    bl_options = {"REGISTER", "UNDO"}
    action = ""

    @classmethod
    def poll(cls, context):
        return context.mode in {"OBJECT", "EDIT_MESH"} and bool(_selected_meshes(context))

    def execute(self, context):
        was_edit = context.mode == "EDIT_MESH"
        try:
            if was_edit:
                bpy.ops.object.mode_set(mode="OBJECT")
            count = _remote_operation(context, self.action)
            self.report({"INFO"}, f"UvCraft {self.action}: {count} mesh datablock(s)")
            return {"FINISHED"}
        except Exception as exc:
            REMOTE_STATUS["topology"] = str(exc)
            self.report({"ERROR"}, str(exc))
            return {"CANCELLED"}
        finally:
            if was_edit:
                bpy.ops.object.mode_set(mode="EDIT")


class UVCRAFT_OT_remote_send(_RemoteMeshOperator):
    bl_idname = "uvcraft.remote_send"
    bl_label = "Send Selected Objects"
    action = "send"


class UVCRAFT_OT_remote_update(_RemoteMeshOperator):
    bl_idname = "uvcraft.remote_update"
    bl_label = "Update Mesh"
    action = "update"


class UVCRAFT_OT_remote_receive(_RemoteMeshOperator):
    bl_idname = "uvcraft.remote_receive"
    bl_label = "Receive UVs"
    action = "receive"


CLASSES = (UVCRAFT_PG_bridge, UVCRAFT_OT_run, UVCRAFT_OT_remote_connect,
           UVCRAFT_OT_remote_disconnect, UVCRAFT_OT_remote_status, UVCRAFT_OT_remote_send,
           UVCRAFT_OT_remote_update, UVCRAFT_OT_remote_receive, UVCRAFT_PT_bridge)


def register():
    for cls in CLASSES:
        bpy.utils.register_class(cls)
    bpy.types.Scene.uvcraft_bridge_settings = PointerProperty(type=UVCRAFT_PG_bridge)


def unregister():
    del bpy.types.Scene.uvcraft_bridge_settings
    for cls in reversed(CLASSES):
        bpy.utils.unregister_class(cls)

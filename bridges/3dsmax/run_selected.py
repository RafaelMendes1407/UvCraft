"""Example script for the 3ds Max Python Script Editor."""

import pymxs
from pymxs import runtime as rt
import uvcraft_3dsmax

commands = [{"cmd": "auto_seams"}, {"cmd": "unfold"}, {"cmd": "pack"}]
channel = 1
selected_faces = None  # e.g. {1, 2, 3}, using 1-based Editable Poly face IDs

if len(rt.selection) != 1:
    raise uvcraft_3dsmax.BridgeError("select exactly one Editable Poly node")
node = rt.selection[0]
poly = node.baseObject
if not rt.isKindOf(poly, rt.Editable_Poly):
    raise uvcraft_3dsmax.BridgeError("selected node's base object must be Editable Poly")

prepared = uvcraft_3dsmax.prepare(poly, commands, rt, channel, selected_faces)
applied = False
with pymxs.undo(True, "UvCraft UV Bridge"):
    uvcraft_3dsmax.apply(poly, prepared, rt)
    applied = True
if not applied:
    raise uvcraft_3dsmax.BridgeError("3ds Max rolled back the UV edit")
rt.redrawViews()

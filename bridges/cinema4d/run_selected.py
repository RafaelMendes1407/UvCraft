"""Example script for Cinema 4D's Script Manager."""

import c4d
import uvcraft_cinema4d

commands = [{"cmd": "auto_seams"}, {"cmd": "unfold"}, {"cmd": "pack"}]
selected_only = False

doc = c4d.documents.GetActiveDocument()
obj = doc.GetActiveObject()
if not isinstance(obj, c4d.PolygonObject):
    raise uvcraft_cinema4d.BridgeError("select a PolygonObject")

selected = None
if selected_only:
    selection = obj.GetPolygonS()
    selected = {index for index in range(obj.GetPolygonCount()) if selection.IsSelected(index)}

prepared = uvcraft_cinema4d.prepare(obj, commands, c4d, selected)
doc.StartUndo()
try:
    uvcraft_cinema4d.apply(obj, prepared, c4d, doc)
finally:
    doc.EndUndo()
c4d.EventAdd()

//! The `uvcraft` Python module. Python values cross the boundary as JSON
//! (through Python's `json` module), so commands accept exactly what scripts
//! accept and results have the same shape as the CLI's JSON output.

use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};
use serde_json::Value;
use uvcraft_commands::{CommandError, Session as RsSession};

use crate::{ApiError, command_value};

create_exception!(
    uvcraft,
    UvCraftError,
    PyException,
    "A UvCraft command failed (no mesh loaded, invalid island, file error, ...)."
);

impl From<ApiError> for PyErr {
    fn from(e: ApiError) -> PyErr {
        let message = e.to_string();
        match e {
            ApiError::InvalidCommand(_) | ApiError::Script(_) => PyValueError::new_err(message),
            ApiError::Io { .. } => PyOSError::new_err(message),
            ApiError::Command(_) => UvCraftError::new_err(message),
        }
    }
}

fn to_json(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    let text: String = obj
        .py()
        .import("json")?
        .call_method1("dumps", (obj,))?
        .extract()?;
    serde_json::from_str(&text).map_err(|e| PyValueError::new_err(e.to_string()))
}

fn to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    let text = serde_json::to_string(value).expect("JSON values always serialize");
    py.import("json")?.call_method1("loads", (text,))
}

fn to_py_list<'py>(py: Python<'py>, values: Vec<Value>) -> PyResult<Bound<'py, PyAny>> {
    to_py(py, &Value::Array(values))
}

type Args = Vec<(String, Value)>;

fn arg(name: &str, value: impl serde::Serialize) -> (String, Value) {
    (
        name.into(),
        serde_json::to_value(value).expect("arguments always serialize"),
    )
}

/// A UvCraft editing session: one loaded mesh, its UVs, undo/redo and the
/// command log. Every method runs a command through the command layer.
#[pyclass(module = "uvcraft")]
#[derive(Default)]
pub struct Session {
    inner: RsSession,
}

impl Session {
    fn doc(&self) -> PyResult<&uvcraft_commands::Document> {
        self.inner
            .document()
            .ok_or_else(|| ApiError::from(CommandError::NoDocument).into())
    }

    /// Runs command `name` with `args` plus extra keyword options; returns the result dict.
    fn run<'py>(
        &mut self,
        py: Python<'py>,
        name: &str,
        mut args: Args,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if let Some(options) = options {
            match to_json(options.as_any())? {
                Value::Object(map) => args.extend(map),
                _ => unreachable!("a dict is a JSON object"),
            }
        }
        let result = crate::execute_value(&mut self.inner, command_value(name, args))?;
        to_py(py, &result)
    }
}

#[pymethods]
impl Session {
    #[new]
    fn new() -> Self {
        Self::default()
    }

    /// Runs one command, given as a dict (`{"cmd": "unfold"}`) or a JSON
    /// string. Returns `{"cmd", "status": "done"|"info"|"report", ...}`.
    fn execute<'py>(
        &mut self,
        py: Python<'py>,
        command: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let result = match command.cast::<PyString>() {
            Ok(text) => crate::execute_json(&mut self.inner, &text.extract::<String>()?)?,
            Err(_) => crate::execute_value(&mut self.inner, to_json(command)?)?,
        };
        to_py(py, &result)
    }

    /// Runs a script (JSON Lines or a JSON array); returns one result per command.
    fn run_script<'py>(&mut self, py: Python<'py>, text: &str) -> PyResult<Bound<'py, PyAny>> {
        to_py_list(py, crate::run_script(&mut self.inner, text)?)
    }

    /// Runs a script file, e.g. a macro written by `save_macro`.
    fn run_file<'py>(&mut self, py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyAny>> {
        let path = path.to_string_lossy().into_owned();
        to_py_list(py, crate::run_script_file(&mut self.inner, &path)?)
    }

    fn load<'py>(&mut self, py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "load", vec![arg("path", path)], None)
    }

    fn save<'py>(&mut self, py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "save", vec![arg("path", path)], None)
    }

    /// Export island cut contours to a dimensioned SVG. UV coordinates use `unit`.
    #[pyo3(signature = (path, unit, margin=0.0, islands=None))]
    fn export_flat_pattern<'py>(
        &mut self,
        py: Python<'py>,
        path: PathBuf,
        unit: &str,
        margin: f64,
        islands: Option<Vec<usize>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "export_flat_pattern",
            vec![
                arg("path", path),
                arg("unit", unit),
                arg("margin", margin),
                arg("islands", islands),
            ],
            None,
        )
    }

    fn undo<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "undo", vec![], None)
    }

    fn redo<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "redo", vec![], None)
    }

    /// Scale/rotate/translate islands: `transform([0], rotate_deg=90)`.
    #[pyo3(signature = (islands=None, **options))]
    fn transform<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "transform", vec![arg("islands", islands)], options)
    }

    /// Apply one weighted UV brush stroke (move, smooth or relax).
    #[allow(clippy::too_many_arguments)] // Python's public stroke parameters map directly to the command.
    #[pyo3(signature = (vertices, weights, mode, delta=(0.0, 0.0), strength=1.0, iterations=1))]
    fn brush_edit<'py>(
        &mut self,
        py: Python<'py>,
        vertices: Vec<usize>,
        weights: Vec<f64>,
        mode: &str,
        delta: (f64, f64),
        strength: f64,
        iterations: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "brush_edit",
            vec![
                arg("vertices", vertices),
                arg("weights", weights),
                arg("mode", mode),
                arg("delta", delta),
                arg("strength", strength),
                arg("iterations", iterations),
            ],
            None,
        )
    }

    fn cut<'py>(&mut self, py: Python<'py>, edges: Vec<usize>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "cut", vec![arg("edges", edges)], None)
    }

    fn weld<'py>(&mut self, py: Python<'py>, edges: Vec<usize>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "weld", vec![arg("edges", edges)], None)
    }

    /// Weld seams whose endpoint UVs are within `tolerance_uv`.
    #[pyo3(signature = (tolerance_uv, islands=None))]
    fn auto_weld<'py>(
        &mut self,
        py: Python<'py>,
        tolerance_uv: f64,
        islands: Option<Vec<usize>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "auto_weld",
            vec![arg("tolerance_uv", tolerance_uv), arg("islands", islands)],
            None,
        )
    }

    fn hard_edges_to_seams<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "hard_edges_to_seams", vec![], None)
    }

    fn seams_to_hard_edges<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "seams_to_hard_edges", vec![], None)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn auto_seams<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "auto_seams", vec![arg("islands", islands)], options)
    }

    #[pyo3(signature = (islands=None, smoothness=0.25, iterations=10))]
    fn box_planar_seams<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        smoothness: f64,
        iterations: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "box_planar_seams",
            vec![
                arg("islands", islands),
                arg("smoothness", smoothness),
                arg("iterations", iterations),
            ],
            None,
        )
    }

    #[pyo3(signature = (islands=None, max_cuts=10, target_distortion=1.1))]
    fn cut_reduce_stretch<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        max_cuts: usize,
        target_distortion: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "cut_reduce_stretch",
            vec![
                arg("islands", islands),
                arg("max_cuts", max_cuts),
                arg("target_distortion", target_distortion),
            ],
            None,
        )
    }

    /// `mode`: `"box"`, `"best"`, `{"planar": "x"}`, `{"cylindrical": "y"}`, `{"spherical": "y"}`.
    #[pyo3(signature = (mode, islands=None))]
    fn project<'py>(
        &mut self,
        py: Python<'py>,
        mode: &Bound<'py, PyAny>,
        islands: Option<Vec<usize>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = vec![arg("mode", to_json(mode)?), arg("islands", islands)];
        self.run(py, "project", args, None)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn unfold<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "unfold", vec![arg("islands", islands)], options)
    }

    /// `optimize(method="slim"|"arap", iterations=20)`.
    #[pyo3(signature = (islands=None, **options))]
    fn optimize<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "optimize", vec![arg("islands", islands)], options)
    }

    /// Optimize a weighted mix of angle and area distortion (`area_weight` 0..1).
    #[pyo3(signature = (area_weight, islands=None, iterations=None))]
    fn optimize_angle_area<'py>(
        &mut self,
        py: Python<'py>,
        area_weight: f64,
        islands: Option<Vec<usize>>,
        iterations: Option<usize>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "optimize_angle_area",
            vec![
                arg("area_weight", area_weight),
                arg("islands", islands),
                arg("iterations", iterations),
            ],
            None,
        )
    }

    /// Improve valid UVs from their current layout without reinitializing them.
    #[pyo3(signature = (islands=None, iterations=None))]
    fn refine<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        iterations: Option<usize>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "refine",
            vec![arg("islands", islands), arg("iterations", iterations)],
            None,
        )
    }

    /// `pack(padding_px=4, resolution=2048, method="raster"|"shelf", rescale=True, ...)`.
    #[pyo3(signature = (islands=None, **options))]
    fn pack<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "pack", vec![arg("islands", islands)], options)
    }

    #[pyo3(signature = (density, islands=None, resolution=None))]
    fn set_texel_density<'py>(
        &mut self,
        py: Python<'py>,
        density: f64,
        islands: Option<Vec<usize>>,
        resolution: Option<u32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = vec![
            arg("density", density),
            arg("islands", islands),
            arg("resolution", resolution),
        ];
        self.run(py, "set_texel_density", args, None)
    }

    #[pyo3(signature = (path, min_scale, max_scale, islands=None))]
    fn apply_density_map<'py>(
        &mut self,
        py: Python<'py>,
        path: &str,
        min_scale: f64,
        max_scale: f64,
        islands: Option<Vec<usize>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "apply_density_map",
            vec![
                arg("path", path),
                arg("min_scale", min_scale),
                arg("max_scale", max_scale),
                arg("islands", islands),
            ],
            None,
        )
    }

    #[pyo3(signature = (path, resolution=256, rays=32, max_distance=1.0))]
    fn bake_ambient_occlusion<'py>(
        &mut self,
        py: Python<'py>,
        path: &str,
        resolution: usize,
        rays: usize,
        max_distance: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "bake_ambient_occlusion",
            vec![
                arg("path", path),
                arg("resolution", resolution),
                arg("rays", rays),
                arg("max_distance", max_distance),
            ],
            None,
        )
    }

    #[pyo3(signature = (mesh_unit, uv_unit, islands=None))]
    fn set_real_scale<'py>(
        &mut self,
        py: Python<'py>,
        mesh_unit: &str,
        uv_unit: &str,
        islands: Option<Vec<usize>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "set_real_scale",
            vec![
                arg("mesh_unit", mesh_unit),
                arg("uv_unit", uv_unit),
                arg("islands", islands),
            ],
            None,
        )
    }

    #[pyo3(signature = (mesh_unit, uv_unit, islands=None))]
    fn set_exact_real_scale<'py>(
        &mut self,
        py: Python<'py>,
        mesh_unit: &str,
        uv_unit: &str,
        islands: Option<Vec<usize>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "set_exact_real_scale",
            vec![
                arg("mesh_unit", mesh_unit),
                arg("uv_unit", uv_unit),
                arg("islands", islands),
            ],
            None,
        )
    }

    #[pyo3(signature = (mesh_unit, output_unit, edges=None, islands=None))]
    fn measure_physical<'py>(
        &mut self,
        py: Python<'py>,
        mesh_unit: &str,
        output_unit: &str,
        edges: Option<Vec<usize>>,
        islands: Option<Vec<usize>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "measure_physical",
            vec![
                arg("mesh_unit", mesh_unit),
                arg("output_unit", output_unit),
                arg("edges", edges),
                arg("islands", islands),
            ],
            None,
        )
    }

    #[pyo3(signature = (width, height, unit, uv_unit, **options))]
    fn pack_physical<'py>(
        &mut self,
        py: Python<'py>,
        width: f64,
        height: f64,
        unit: &str,
        uv_unit: &str,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "pack_physical",
            vec![
                arg("width", width),
                arg("height", height),
                arg("unit", unit),
                arg("uv_unit", uv_unit),
            ],
            options,
        )
    }

    /// Equalize selected islands' texel density while preserving their total UV area.
    #[pyo3(signature = (islands=None, resolution=None))]
    fn equalize_texel_density<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        resolution: Option<u32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "equalize_texel_density",
            vec![arg("islands", islands), arg("resolution", resolution)],
            None,
        )
    }

    #[pyo3(signature = (vertices, pinned=true))]
    fn pin<'py>(
        &mut self,
        py: Python<'py>,
        vertices: Vec<usize>,
        pinned: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = vec![arg("vertices", vertices), arg("pinned", pinned)];
        self.run(py, "pin", args, None)
    }

    fn clear_pins<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "clear_pins", vec![], None)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn mosaic<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "mosaic", vec![arg("islands", islands)], options)
    }

    fn load_uvs<'py>(&mut self, py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "load_uvs", vec![arg("path", path)], None)
    }

    fn list_uv_channels<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "list_uv_channels", vec![], None)
    }

    fn switch_uv_channel<'py>(
        &mut self,
        py: Python<'py>,
        channel: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "switch_uv_channel", vec![arg("channel", channel)], None)
    }

    fn create_uv_channel<'py>(
        &mut self,
        py: Python<'py>,
        channel: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "create_uv_channel", vec![arg("channel", channel)], None)
    }

    fn swap_uv_channels<'py>(
        &mut self,
        py: Python<'py>,
        a: u32,
        b: u32,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "swap_uv_channels", vec![arg("a", a), arg("b", b)], None)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn mirror<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "mirror", vec![arg("islands", islands)], options)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn orient<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "orient", vec![arg("islands", islands)], options)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn align<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "align", vec![arg("islands", islands)], options)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn distribute<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "distribute", vec![arg("islands", islands)], options)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn snap<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "snap", vec![arg("islands", islands)], options)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn similar<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "similar", vec![arg("islands", islands)], options)
    }

    #[pyo3(signature = (islands=None, **options))]
    fn stack<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "stack", vec![arg("islands", islands)], options)
    }

    #[pyo3(signature = (islands=None))]
    fn unstack<'py>(
        &mut self,
        py: Python<'py>,
        islands: Option<Vec<usize>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "unstack", vec![arg("islands", islands)], None)
    }

    fn match_islands<'py>(
        &mut self,
        py: Python<'py>,
        source: usize,
        targets: Vec<usize>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "match",
            vec![arg("source", source), arg("targets", targets)],
            None,
        )
    }

    fn cut_unfold<'py>(
        &mut self,
        py: Python<'py>,
        edges: Vec<usize>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "cut_unfold", vec![arg("edges", edges)], None)
    }

    #[pyo3(signature = (edge, unfold=false))]
    fn cut_loop<'py>(
        &mut self,
        py: Python<'py>,
        edge: usize,
        unfold: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "cut_loop",
            vec![arg("edge", edge), arg("unfold", unfold)],
            None,
        )
    }

    #[pyo3(signature = (vertices, unfold=false))]
    fn cut_path<'py>(
        &mut self,
        py: Python<'py>,
        vertices: Vec<usize>,
        unfold: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "cut_path",
            vec![arg("vertices", vertices), arg("unfold", unfold)],
            None,
        )
    }

    #[pyo3(signature = (faces, unfold=false))]
    fn cut_border<'py>(
        &mut self,
        py: Python<'py>,
        faces: Vec<usize>,
        unfold: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "cut_border",
            vec![arg("faces", faces), arg("unfold", unfold)],
            None,
        )
    }

    /// Straighten an ordered path of mesh vertices or edges in one island.
    #[pyo3(signature = (island, **options))]
    fn straighten_path<'py>(
        &mut self,
        py: Python<'py>,
        island: usize,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "straighten_path", vec![arg("island", island)], options)
    }

    fn rectangularize<'py>(
        &mut self,
        py: Python<'py>,
        island: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "rectangularize", vec![arg("island", island)], None)
    }

    /// Align an ordered island border path horizontally (`axis="v"`) or vertically (`axis="u"`).
    #[pyo3(signature = (island, axis, **options))]
    fn align_border<'py>(
        &mut self,
        py: Python<'py>,
        island: usize,
        axis: &str,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "align_border",
            vec![arg("island", island), arg("axis", axis)],
            options,
        )
    }

    /// Query island indices by quality issue, e.g. `"overlapping"` or `"flipped"`.
    fn select_islands<'py>(
        &mut self,
        py: Python<'py>,
        problem: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "select_islands", vec![arg("problem", problem)], None)
    }

    /// Query islands by distortion or texel density within inclusive bounds.
    #[pyo3(signature = (metric, **options))]
    fn select_islands_by_metric<'py>(
        &mut self,
        py: Python<'py>,
        metric: &str,
        options: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "select_islands_by_metric",
            vec![arg("metric", metric)],
            options,
        )
    }

    /// Select OBJ faces by smoothing group (`None` means `s off`).
    #[pyo3(signature = (group=None))]
    fn select_faces_by_smoothing_group<'py>(
        &mut self,
        py: Python<'py>,
        group: Option<u32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "select_faces_by_smoothing_group",
            vec![arg("group", group)],
            None,
        )
    }

    /// Select OBJ or glTF faces by exact material name; `None` means no material.
    #[pyo3(signature = (material=None))]
    fn select_faces_by_material<'py>(
        &mut self,
        py: Python<'py>,
        material: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "select_faces_by_material",
            vec![arg("material", material)],
            None,
        )
    }

    fn select_faces_by_angle<'py>(
        &mut self,
        py: Python<'py>,
        seed_face: usize,
        max_angle_deg: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.run(
            py,
            "select_faces_by_angle",
            vec![
                arg("seed_face", seed_face),
                arg("max_angle_deg", max_angle_deg),
            ],
            None,
        )
    }

    /// Returns the quality report as a dict (optionally also written to `output`).
    #[pyo3(signature = (resolution=None, output=None, padding_px=None))]
    fn analyze<'py>(
        &mut self,
        py: Python<'py>,
        resolution: Option<u32>,
        output: Option<PathBuf>,
        padding_px: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = vec![
            arg("resolution", resolution),
            arg("output", output),
            arg("padding_px", padding_px),
        ];
        let result = crate::execute_value(&mut self.inner, command_value("analyze", args))?;
        to_py(py, &result["report"])
    }

    /// Writes the macro (edits currently applied since the last load) as a script.
    fn save_macro<'py>(&mut self, py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyAny>> {
        self.run(py, "save_macro", vec![arg("path", path)], None)
    }

    /// Every successful command so far, as dicts (replayable with `execute`).
    fn log<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        to_py_list(py, crate::commands_value(self.inner.log()))
    }

    fn log_jsonl(&self) -> String {
        self.inner.log_jsonl()
    }

    /// The edits behind the current UVs since the last load (undone ones dropped).
    fn macro_commands<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        to_py_list(py, crate::commands_value(self.inner.macro_commands()))
    }

    fn macro_jsonl(&self) -> String {
        self.inner.macro_jsonl()
    }

    /// The face indices of each island, in the numbering commands use.
    fn islands(&self) -> PyResult<Vec<Vec<usize>>> {
        Ok(crate::island_faces(&self.inner)?)
    }

    /// UV coordinates, one `(u, v)` per UV vertex.
    fn uvs(&self) -> PyResult<Vec<(f64, f64)>> {
        Ok(self.doc()?.uv().uvs.iter().map(|p| (p.x, p.y)).collect())
    }

    /// UV vertex index of each face corner (corners in face order).
    fn corner_uv(&self) -> PyResult<Vec<u32>> {
        Ok(self.doc()?.uv().corner_uv.clone())
    }

    /// Mesh vertex index of each face corner.
    fn corner_vertex(&self) -> PyResult<Vec<usize>> {
        let mesh = &self.doc()?.mesh;
        Ok((0..mesh.corner_count())
            .map(|c| mesh.corner_vertex(c))
            .collect())
    }

    /// Corner range `(start, end)` of each face.
    fn face_corners(&self) -> PyResult<Vec<(usize, usize)>> {
        let mesh = &self.doc()?.mesh;
        Ok((0..mesh.face_count())
            .map(|f| {
                let r = mesh.face_corners(f);
                (r.start, r.end)
            })
            .collect())
    }

    /// Pinned flag per mesh vertex.
    fn pins(&self) -> PyResult<Vec<bool>> {
        Ok(self.doc()?.pins().to_vec())
    }

    #[getter]
    fn has_document(&self) -> bool {
        self.inner.document().is_some()
    }

    #[getter]
    fn path(&self) -> PyResult<PathBuf> {
        Ok(self.doc()?.path.clone())
    }

    #[getter]
    fn vertex_count(&self) -> PyResult<usize> {
        Ok(self.doc()?.mesh.vertex_count())
    }

    #[getter]
    fn face_count(&self) -> PyResult<usize> {
        Ok(self.doc()?.mesh.face_count())
    }

    #[getter]
    fn edge_count(&self) -> PyResult<usize> {
        Ok(self.doc()?.mesh.edge_count())
    }

    #[getter]
    fn corner_count(&self) -> PyResult<usize> {
        Ok(self.doc()?.mesh.corner_count())
    }

    #[getter]
    fn uv_count(&self) -> PyResult<usize> {
        Ok(self.doc()?.uv().uvs.len())
    }

    #[getter]
    fn can_undo(&self) -> bool {
        self.inner.can_undo()
    }

    #[getter]
    fn can_redo(&self) -> bool {
        self.inner.can_redo()
    }

    fn __repr__(&self) -> String {
        match self.inner.document() {
            Some(d) => format!(
                "uvcraft.Session({:?}, {} faces, {} commands)",
                d.path.display().to_string(),
                d.mesh.face_count(),
                self.inner.log().len()
            ),
            None => "uvcraft.Session(no mesh)".into(),
        }
    }
}

#[pymodule]
fn uvcraft(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Session>()?;
    m.add("UvCraftError", m.py().get_type::<UvCraftError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

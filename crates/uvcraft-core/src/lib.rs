//! UvCraft data model. No UI and no algorithms beyond connectivity.

pub mod islands;
pub mod mesh;
pub mod progress;
pub mod uv;

pub use glam;
pub use islands::Islands;
pub use mesh::{Mesh, MeshError, NONE};
pub use progress::{CancelToken, Cancelled, Progress, ProgressEvent};
pub use uv::{EdgeKind, UvError, UvLayer};

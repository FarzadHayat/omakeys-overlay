pub mod binds;
pub mod boards;
pub mod config;
pub mod output;
pub mod overrides;
pub mod state;

pub use crate::board::labels::LabelStyle;
pub use binds::{ensure_hyprland_signature, fetch_binds, fetch_binds_with_retry, Bind};
pub use config::{load_config, normalize_scale, save_config, Anchor, AppConfig, VisibilityMode};
// AppConfig re-exported for binary.
pub use output::KeyOut;
pub use overrides::{load_overrides, Overrides};
pub use state::{BindLayer, BindView, Frame, FrameKind, KeyView, OverlayCore};

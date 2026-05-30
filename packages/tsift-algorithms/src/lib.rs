pub mod coupling;
pub mod dead_code;
pub mod health;
pub mod scc;

pub use coupling::{CouplingReport, ModuleCoupling, coupling_analysis};
pub use dead_code::{DeadCodeNode, DeadCodeResult, detect_dead_code};
pub use health::{HealthReport, HealthScore, composite_health_score};
pub use scc::{SccComponent, SccResult, tarjan_scc};

pub mod scc;
pub mod health;
pub mod dead_code;
pub mod coupling;

pub use scc::{tarjan_scc, SccResult, SccComponent};
pub use health::{composite_health_score, HealthScore, HealthReport};
pub use dead_code::{detect_dead_code, DeadCodeResult, DeadCodeNode};
pub use coupling::{coupling_analysis, CouplingReport, ModuleCoupling};

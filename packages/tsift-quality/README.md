# tsift-quality

Quality-gate surfaces for tsift.

This crate contains skill audit, performance gate, DCI benchmark, runtime churn,
and lint helpers used by tsift commands and tests. Shared cache primitives live
in `tsift-cache`; this crate re-exports `cycle_packet_cache` for compatibility.
It is part of the versioned tsift workspace and is published with the rest of
the split crates.

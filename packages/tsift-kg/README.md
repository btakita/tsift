# tsift-kg

Shared local Knowledge Graph extraction pipeline for tsift and agent-doc.

This crate chunks source, session, and memory text; calls a local extractor
provider; validates JSON entity/relation payloads; materializes provider-neutral
`GraphProjection` rows; and can upsert those rows into the SQLite graph store.

`verify_projection_multi_run_stability` compares two projections from repeated
runs over the same source and fails if source-derived node or edge ids drift or
duplicate. The SQLite upsert path is covered by a sequential-run test so KG
facts update in place instead of accumulating duplicate rows.

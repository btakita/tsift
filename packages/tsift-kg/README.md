# tsift-kg

Shared local Knowledge Graph extraction pipeline for tsift and agent-doc.

This crate chunks source, session, and memory text; calls a local extractor
provider; validates JSON entity/relation payloads; materializes provider-neutral
`GraphProjection` rows; and can upsert those rows into the SQLite graph store.

import { defineSchema, defineTable } from "convex/server";
import { v } from "convex/values";

const stringMap = v.record(v.string(), v.string());

const provenance = v.object({
  source: v.string(),
  source_ref: v.string(),
  content_hash: v.optional(v.string()),
});

const freshness = v.object({
  content_hash: v.optional(v.string()),
  observed_at_unix: v.optional(v.number()),
});

export default defineSchema({
  nodes: defineTable({
    externalId: v.string(),
    kind: v.string(),
    label: v.string(),
    properties: stringMap,
    provenance: v.array(provenance),
    freshness: v.optional(freshness),
  })
    .index("by_external_id", ["externalId"])
    .index("by_kind", ["kind"]),

  edges: defineTable({
    edgeKey: v.string(),
    fromExternalId: v.string(),
    toExternalId: v.string(),
    kind: v.string(),
    properties: stringMap,
    provenance: v.array(provenance),
    freshness: v.optional(freshness),
  })
    .index("by_edge_key", ["edgeKey"])
    .index("by_from_kind", ["fromExternalId", "kind"])
    .index("by_to_kind", ["toExternalId", "kind"]),
});

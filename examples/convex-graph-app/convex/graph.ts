import { v } from "convex/values";
import { mutation, query } from "./_generated/server";

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

const nodeRow = v.object({
  externalId: v.string(),
  kind: v.string(),
  label: v.string(),
  properties: stringMap,
  provenance: v.array(provenance),
  freshness: v.optional(freshness),
});

const edgeRow = v.object({
  edgeKey: v.string(),
  fromExternalId: v.string(),
  toExternalId: v.string(),
  kind: v.string(),
  properties: stringMap,
  provenance: v.array(provenance),
  freshness: v.optional(freshness),
});

const requiredIndexes = [
  { table: "nodes", name: "by_external_id", fields: ["externalId"] },
  { table: "nodes", name: "by_kind", fields: ["kind"] },
  { table: "edges", name: "by_edge_key", fields: ["edgeKey"] },
  { table: "edges", name: "by_from_kind", fields: ["fromExternalId", "kind"] },
  { table: "edges", name: "by_to_kind", fields: ["toExternalId", "kind"] },
];

export const snapshot = query({
  args: {},
  handler: async (ctx) => {
    return {
      nodes: await ctx.db.query("nodes").collect(),
      edges: await ctx.db.query("edges").collect(),
      indexes: requiredIndexes,
    };
  },
});

export const upsertNodes = mutation({
  args: { rows: v.array(nodeRow) },
  handler: async (ctx, { rows }) => {
    for (const row of rows) {
      const existing = await ctx.db
        .query("nodes")
        .withIndex("by_external_id", (q) => q.eq("externalId", row.externalId))
        .unique();
      if (existing) {
        await ctx.db.patch(existing._id, row);
      } else {
        await ctx.db.insert("nodes", row);
      }
    }
    return { status: "ok", count: rows.length };
  },
});

export const upsertEdges = mutation({
  args: { rows: v.array(edgeRow) },
  handler: async (ctx, { rows }) => {
    for (const row of rows) {
      const from = await ctx.db
        .query("nodes")
        .withIndex("by_external_id", (q) => q.eq("externalId", row.fromExternalId))
        .unique();
      const to = await ctx.db
        .query("nodes")
        .withIndex("by_external_id", (q) => q.eq("externalId", row.toExternalId))
        .unique();
      if (!from || !to) {
        throw new Error(`edge ${row.edgeKey} references missing node`);
      }
      const existing = await ctx.db
        .query("edges")
        .withIndex("by_edge_key", (q) => q.eq("edgeKey", row.edgeKey))
        .unique();
      if (existing) {
        await ctx.db.patch(existing._id, row);
      } else {
        await ctx.db.insert("edges", row);
      }
    }
    return { status: "ok", count: rows.length };
  },
});

export const deleteEdges = mutation({
  args: { keys: v.array(v.string()) },
  handler: async (ctx, { keys }) => {
    for (const edgeKey of keys) {
      const existing = await ctx.db
        .query("edges")
        .withIndex("by_edge_key", (q) => q.eq("edgeKey", edgeKey))
        .unique();
      if (existing) {
        await ctx.db.delete(existing._id);
      }
    }
    return { status: "ok", count: keys.length };
  },
});

export const deleteNodes = mutation({
  args: { keys: v.array(v.string()) },
  handler: async (ctx, { keys }) => {
    for (const externalId of keys) {
      const existing = await ctx.db
        .query("nodes")
        .withIndex("by_external_id", (q) => q.eq("externalId", externalId))
        .unique();
      if (existing) {
        await ctx.db.delete(existing._id);
      }
    }
    return { status: "ok", count: keys.length };
  },
});

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

// Default page size used by the paginated snapshot queries. Sized to stay well
// under the Convex isolate's per-request syscall budget while keeping HTTP
// round-trips low on multi-thousand-row tables. Override by passing `limit`.
const DEFAULT_SNAPSHOT_PAGE_SIZE = 500;
const MAX_SNAPSHOT_PAGE_SIZE = 2000;

function clampLimit(limit: number | undefined): number {
  if (limit === undefined || limit === null) {
    return DEFAULT_SNAPSHOT_PAGE_SIZE;
  }
  if (!Number.isFinite(limit) || limit <= 0) {
    return DEFAULT_SNAPSHOT_PAGE_SIZE;
  }
  return Math.min(Math.floor(limit), MAX_SNAPSHOT_PAGE_SIZE);
}

// Legacy single-shot snapshot — retained for back-compat with small tables and
// existing operator commands. Fails (Convex syscall budget) on tables >~5k
// rows; new callers MUST use snapshotMeta + snapshotNodesPage +
// snapshotEdgesPage instead. See #convexsnapshotscale for the migration.
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

// Cheap metadata read: indexes + page sizing only. Row counts are intentionally
// omitted because counting every row still spends Convex syscall budget at
// million-row scale; callers discover completion through page nextCursor=null.
export const snapshotMeta = query({
  args: { projectionMetaId: v.optional(v.string()) },
  handler: async (ctx, { projectionMetaId }) => {
    let projectionHash: string | null = null;
    if (projectionMetaId !== undefined) {
      const meta = await ctx.db
        .query("nodes")
        .withIndex("by_external_id", (q) => q.eq("externalId", projectionMetaId))
        .unique();
      const hash = meta?.properties?.content_hash;
      if (typeof hash === "string") {
        projectionHash = hash;
      }
    }
    return {
      indexes: requiredIndexes,
      projectionHash,
      pageSize: DEFAULT_SNAPSHOT_PAGE_SIZE,
    };
  },
});

// Cursor-based page of nodes ordered by `externalId` (via the `by_external_id`
// index). `cursor` is the exclusive lower bound (the last `externalId` from the
// previous page); pass `null`/omit to start from the beginning. The returned
// `nextCursor` is `null` when the page exhausts the table.
export const snapshotNodesPage = query({
  args: {
    cursor: v.optional(v.union(v.string(), v.null())),
    limit: v.optional(v.number()),
  },
  handler: async (ctx, { cursor, limit }) => {
    const pageSize = clampLimit(limit);
    const builder = ctx.db.query("nodes").withIndex("by_external_id", (q) =>
      cursor === undefined || cursor === null
        ? q
        : q.gt("externalId", cursor),
    );
    const rows = await builder.take(pageSize);
    const nextCursor =
      rows.length === pageSize ? rows[rows.length - 1].externalId : null;
    return { rows, nextCursor, pageSize };
  },
});

// Cursor-based page of edges ordered by `edgeKey` (via the `by_edge_key`
// index). Same cursor contract as snapshotNodesPage.
export const snapshotEdgesPage = query({
  args: {
    cursor: v.optional(v.union(v.string(), v.null())),
    limit: v.optional(v.number()),
  },
  handler: async (ctx, { cursor, limit }) => {
    const pageSize = clampLimit(limit);
    const builder = ctx.db.query("edges").withIndex("by_edge_key", (q) =>
      cursor === undefined || cursor === null
        ? q
        : q.gt("edgeKey", cursor),
    );
    const rows = await builder.take(pageSize);
    const nextCursor =
      rows.length === pageSize ? rows[rows.length - 1].edgeKey : null;
    return { rows, nextCursor, pageSize };
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

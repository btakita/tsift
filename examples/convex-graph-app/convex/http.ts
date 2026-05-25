import { httpRouter } from "convex/server";
import { httpAction } from "./_generated/server";
import { api } from "./_generated/api";

const http = httpRouter();

function jsonResponse(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

http.route({
  path: "/tsift/graph",
  method: "POST",
  handler: httpAction(async (ctx, request) => {
    const body = await request.json();
    switch (body.operation) {
      case "snapshot": {
        // Legacy single-shot. Fails on tables >~5k rows (Convex syscall
        // budget). New callers should use snapshot_meta + snapshot_nodes_page
        // + snapshot_edges_page. See #convexsnapshotscale.
        const rows = await ctx.runQuery(api.graph.snapshot, {});
        return jsonResponse({ status: "ok", rows });
      }
      case "snapshot_meta": {
        const meta = await ctx.runQuery(api.graph.snapshotMeta, {
          projectionMetaId: body.projectionMetaId,
        });
        return jsonResponse({ status: "ok", meta });
      }
      case "snapshot_nodes_page": {
        const page = await ctx.runQuery(api.graph.snapshotNodesPage, {
          cursor: body.cursor ?? null,
          limit: body.limit,
        });
        return jsonResponse({ status: "ok", page });
      }
      case "snapshot_edges_page": {
        const page = await ctx.runQuery(api.graph.snapshotEdgesPage, {
          cursor: body.cursor ?? null,
          limit: body.limit,
        });
        return jsonResponse({ status: "ok", page });
      }
      case "delete_edges": {
        const result = await ctx.runMutation(api.graph.deleteEdges, {
          keys: body.keys ?? [],
        });
        return jsonResponse(result);
      }
      case "upsert_nodes": {
        const result = await ctx.runMutation(api.graph.upsertNodes, {
          rows: body.nodeRows ?? [],
        });
        return jsonResponse(result);
      }
      case "upsert_edges": {
        const result = await ctx.runMutation(api.graph.upsertEdges, {
          rows: body.edgeRows ?? [],
        });
        return jsonResponse(result);
      }
      case "delete_nodes": {
        const result = await ctx.runMutation(api.graph.deleteNodes, {
          keys: body.keys ?? [],
        });
        return jsonResponse(result);
      }
      default:
        return jsonResponse(
          { status: "error", message: `unknown operation: ${body.operation}` },
          400,
        );
    }
  }),
});

export default http;

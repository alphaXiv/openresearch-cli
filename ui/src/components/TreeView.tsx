import {
  Background,
  BackgroundVariant,
  Handle,
  MarkerType,
  Position,
  ReactFlow,
  type Edge,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import { memo, useMemo } from "react";
import { statusColor, timeAgo, type Experiment, type Run } from "../api";

const NODE_W = 220;
const NODE_H = 96;
const GAP_X = 44;
const GAP_Y = 72;

type ExpNodeData = {
  exp: Experiment;
  latestRun: Run | null;
  runCount: number;
  isBaseline: boolean;
  selected: boolean;
};
type ExpFlowNode = Node<ExpNodeData, "exp">;

interface TreeNode {
  exp: Experiment;
  children: TreeNode[];
}

function buildForest(experiments: Experiment[]): TreeNode[] {
  const byId = new Map(experiments.map((e) => [e.id, { exp: e, children: [] as TreeNode[] }]));
  const roots: TreeNode[] = [];
  for (const e of experiments) {
    const node = byId.get(e.id)!;
    const parent = e.parentExperimentId ? byId.get(e.parentExperimentId) : undefined;
    if (parent) parent.children.push(node);
    else roots.push(node);
  }
  const byCreated = (a: TreeNode, b: TreeNode) => a.exp.createdAt - b.exp.createdAt;
  const sortRec = (n: TreeNode) => {
    n.children.sort(byCreated);
    n.children.forEach(sortRec);
  };
  roots.sort(byCreated);
  roots.forEach(sortRec);
  return roots;
}

function subtreeWidth(node: TreeNode): number {
  if (node.children.length === 0) return NODE_W;
  const cw =
    node.children.reduce((s, c) => s + subtreeWidth(c), 0) + GAP_X * (node.children.length - 1);
  return Math.max(NODE_W, cw);
}

const ExpNode = memo(function ExpNode({ data }: NodeProps<ExpFlowNode>) {
  const { exp, latestRun, runCount, isBaseline, selected } = data;
  const status = latestRun?.status;
  const live = status === "running" || status === "starting";
  return (
    <div className={`exp-node ${selected ? "selected" : ""} ${live ? "live" : ""}`}>
      <Handle type="target" position={Position.Top} />
      <div className="node-head">
        <span
          className="node-status"
          style={{ background: status ? statusColor(status) : "var(--border-strong)" }}
          title={status ?? "never run"}
        />
        <span className="node-slug">{exp.slug}</span>
        {isBaseline && <span className="baseline-chip">baseline</span>}
      </div>
      {(exp.title || exp.description) && (
        <div className="node-title">{exp.title || exp.description}</div>
      )}
      <div className="node-meta">
        <span>{runCount === 1 ? "1 run" : `${runCount} runs`}</span>
        {latestRun && <span>{latestRun.status}</span>}
        {latestRun && <span>{timeAgo(latestRun.createdAt)}</span>}
      </div>
      <Handle type="source" position={Position.Bottom} />
    </div>
  );
});

const nodeTypes = { exp: ExpNode };

const defaultEdgeOptions = {
  style: { stroke: "var(--border-strong)", strokeWidth: 1.5 },
  markerEnd: { type: MarkerType.ArrowClosed, color: "var(--muted)", width: 16, height: 16 },
};

export function TreeView({
  experiments,
  runs,
  selectedId,
  onSelect,
}: {
  experiments: Experiment[];
  runs: Run[];
  selectedId: string | null;
  onSelect: (id: string | null) => void;
}) {
  const { nodes, edges } = useMemo(() => {
    const latestByExp = new Map<string, Run>();
    const countByExp = new Map<string, number>();
    for (const run of runs) {
      countByExp.set(run.experimentId, (countByExp.get(run.experimentId) ?? 0) + 1);
      const cur = latestByExp.get(run.experimentId);
      if (!cur || run.createdAt > cur.createdAt) latestByExp.set(run.experimentId, run);
    }

    const nodes: ExpFlowNode[] = [];
    const edges: Edge[] = [];
    const roots = buildForest(experiments);

    function layout(node: TreeNode, cx: number, y: number) {
      nodes.push({
        id: node.exp.id,
        type: "exp",
        position: { x: cx - NODE_W / 2, y },
        data: {
          exp: node.exp,
          latestRun: latestByExp.get(node.exp.id) ?? null,
          runCount: countByExp.get(node.exp.id) ?? 0,
          isBaseline: !node.exp.parentExperimentId,
          selected: node.exp.id === selectedId,
        },
      });
      if (node.children.length === 0) return;
      const totalW =
        node.children.reduce((s, c) => s + subtreeWidth(c), 0) +
        GAP_X * (node.children.length - 1);
      let x = cx - totalW / 2;
      for (const child of node.children) {
        const cw = subtreeWidth(child);
        edges.push({
          id: `e-${node.exp.id}-${child.exp.id}`,
          source: node.exp.id,
          target: child.exp.id,
        });
        layout(child, x + cw / 2, y + NODE_H + GAP_Y);
        x += cw + GAP_X;
      }
    }

    let rx = 0;
    for (const root of roots) {
      const w = subtreeWidth(root);
      layout(root, rx + w / 2, 0);
      rx += w + GAP_X;
    }
    return { nodes, edges };
  }, [experiments, runs, selectedId]);

  if (experiments.length === 0) {
    return (
      <div className="empty-state">
        <p>No experiments yet — ask the agent to branch one from the baseline.</p>
      </div>
    );
  }

  return (
    <ReactFlow
      nodes={nodes}
      edges={edges}
      nodeTypes={nodeTypes}
      defaultEdgeOptions={defaultEdgeOptions}
      nodesDraggable={false}
      nodesConnectable={false}
      minZoom={0.15}
      fitView
      fitViewOptions={{ padding: 0.25, maxZoom: 1 }}
      onNodeClick={(_, node) => onSelect(node.id)}
      onPaneClick={() => onSelect(null)}
      colorMode="dark"
      style={{ background: "var(--bg-canvas)" }}
    >
      <Background variant={BackgroundVariant.Dots} color="#242b35" gap={28} size={1.6} />
    </ReactFlow>
  );
}

import {
  Background,
  BackgroundVariant,
  Handle,
  Position,
  ReactFlow,
  type Edge,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import { GitBranch, Terminal } from "lucide-react";
import { memo, useMemo } from "react";
import { timeAgo, type Experiment, type Run } from "../api";
import type { ExperimentView } from "./DetailDrawer";
import { StatusBadge } from "./StatusBadge";

const NODE_W = 240;
const NODE_H = 132;
const GAP_X = 44;
const GAP_Y = 72;
const MAX_SQUARES = 8;

type ExpNodeData = {
  exp: Experiment;
  latestRun: Run | null;
  runs: Run[]; // oldest → newest
  isBaseline: boolean;
  selected: boolean;
  onOpenView: (id: string, view: ExperimentView) => void;
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

function runSquareClass(status: string): string {
  if (status === "done") return "pass";
  if (status === "failed") return "fail";
  if (status === "running" || status === "starting") return "live";
  return "other";
}

const ExpNode = memo(function ExpNode({ data }: NodeProps<ExpFlowNode>) {
  const { exp, latestRun, runs, isBaseline, selected, onOpenView } = data;
  const status = latestRun?.status;
  const live = status === "running" || status === "starting";
  const kind = isBaseline ? "BASELINE" : live ? "RUNNING" : "EXPERIMENT";
  const squares = runs.slice(-MAX_SQUARES);
  return (
    <div className={`exp-node ${selected ? "selected" : ""} ${live ? "live" : ""}`}>
      <Handle type="target" position={Position.Top} />
      <div className="node-eyebrow">
        <span>{kind}</span>
        <StatusBadge status={status ?? "idle"} />
      </div>
      <div className="node-head">
        <span className="node-slug">{exp.slug}</span>
      </div>
      {(exp.title || exp.description) && (
        <div className="node-title">{exp.title || exp.description}</div>
      )}
      <div className="node-meta">
        <span>RUNS</span>
        {squares.length > 0 ? (
          <span className="run-squares">
            {squares.map((run) => (
              <span key={run.id} className={`run-sq ${runSquareClass(run.status)}`} title={run.status} />
            ))}
          </span>
        ) : (
          <span>no runs</span>
        )}
        <span style={{ flex: 1 }} />
        {latestRun && <span>{timeAgo(latestRun.createdAt)}</span>}
      </div>
      {/* Direct view shortcuts — changes always, terminal once there's a run. */}
      <div className="node-actions" onClick={(e) => e.stopPropagation()}>
        <button
          className="node-action"
          title="Open changes"
          onClick={() => onOpenView(exp.id, "changes")}
        >
          <GitBranch size={13} />
          Changes
        </button>
        {runs.length > 0 && (
          <button
            className="node-action"
            title="Open terminal"
            onClick={() => onOpenView(exp.id, "terminal")}
          >
            <Terminal size={13} />
            Terminal
          </button>
        )}
      </div>
      <Handle type="source" position={Position.Bottom} />
    </div>
  );
});

const nodeTypes = { exp: ExpNode };

const defaultEdgeOptions = {
  type: "default", // bezier
  style: { stroke: "var(--text)", strokeWidth: 1.5, opacity: 0.3 },
};

export function TreeView({
  experiments,
  runs,
  selectedId,
  onSelect,
  onOpenView,
}: {
  experiments: Experiment[];
  runs: Run[];
  selectedId: string | null;
  onSelect: (id: string | null) => void;
  /** Open an experiment view as a right-pane tab (card shortcut buttons). */
  onOpenView: (id: string, view: ExperimentView) => void;
}) {
  const { nodes, edges } = useMemo(() => {
    const runsByExp = new Map<string, Run[]>();
    for (const run of runs) {
      const list = runsByExp.get(run.experimentId);
      if (list) list.push(run);
      else runsByExp.set(run.experimentId, [run]);
    }
    for (const list of runsByExp.values()) list.sort((a, b) => a.createdAt - b.createdAt);

    const nodes: ExpFlowNode[] = [];
    const edges: Edge[] = [];
    const roots = buildForest(experiments);

    function layout(node: TreeNode, cx: number, y: number) {
      const expRuns = runsByExp.get(node.exp.id) ?? [];
      nodes.push({
        id: node.exp.id,
        type: "exp",
        position: { x: cx - NODE_W / 2, y },
        data: {
          exp: node.exp,
          latestRun: expRuns[expRuns.length - 1] ?? null,
          runs: expRuns,
          isBaseline: !node.exp.parentExperimentId,
          selected: node.exp.id === selectedId,
          onOpenView,
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
  }, [experiments, runs, selectedId, onOpenView]);

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
    >
      <Background variant={BackgroundVariant.Dots} color="var(--dots-strong)" gap={28} size={1.6} />
    </ReactFlow>
  );
}

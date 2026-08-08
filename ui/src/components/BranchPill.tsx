export function BranchPill({ branch }: { branch: string }) {
  return <span className="files-pill"><code>{branch}</code></span>;
}

import { ExternalLink } from "lucide-react";
import { githubBranchUrl } from "../api";

/** A branch name as a pill linking to its GitHub tree view (files-pill
 * styling). Unpushed branches 404 on GitHub, which is self-explanatory. */
export function BranchPill({
  owner,
  repo,
  branch,
}: {
  owner: string;
  repo: string;
  branch: string;
}) {
  return (
    <a
      className="files-pill"
      href={githubBranchUrl(owner, repo, branch)}
      target="_blank"
      rel="noopener noreferrer"
      title={`Open ${branch} on GitHub`}
    >
      <code>{branch}</code>
      <ExternalLink size={12} />
    </a>
  );
}

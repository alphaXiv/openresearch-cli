import { githubBranchUrl } from "../api";
import { GitHubMark } from "./BackendLogos";

/** A branch name as a pill linking to its GitHub tree view (files-pill
 * styling). */
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
      <GitHubMark size={12} />
    </a>
  );
}

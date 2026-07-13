import { useEffect, useRef, useState } from "react";
import {
  createProject,
  resolvePaper,
  searchPapers,
  type PaperHit,
  type Project,
  type ResolvedPaper,
} from "../api";

/** owner/repo out of anything a user pastes: a full GitHub URL (https or ssh),
 * with or without .git, or the bare `owner/repo` shorthand. */
function parseRepo(input: string): { owner: string; repo: string } | null {
  const s = input
    .trim()
    .replace(/^git@github\.com:/i, "")
    .replace(/^https?:\/\/(www\.)?github\.com\//i, "")
    .replace(/\.git$/i, "")
    .replace(/^\/+|\/+$/g, "");
  const [owner, repo] = s.split("/");
  if (!owner || !repo || /[\s:@]/.test(owner) || /[\s:@]/.test(repo)) return null;
  return { owner, repo };
}

/** Mirror of the server's slugify — previews the repo name a blank project gets. */
function slugify(text: string): string {
  return (
    text
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 48)
      .replace(/-+$/, "") || "experiment"
  );
}

/** Mirror of the server's parse_paper_id: bare/versioned arXiv ids and
 * arxiv.org / alphaxiv.org URLs. Null when the input reads as a title query. */
function parsePaperId(input: string): string | null {
  const s = input.trim().split(/[?#]/)[0];
  const last = s.split("/").filter(Boolean).pop() ?? "";
  const id = last.replace(/\.(pdf|md)$/i, "");
  return /^\d{4}\.\d{4,5}(v\d+)?$/.test(id) ? id : null;
}

/** Fast-search titles carry scrape cruft: "[1706.03762] Title - arXiv". */
function cleanTitle(title: string): string {
  return title.replace(/^\[[^\]]*\]\s*/, "").replace(/\s*[-–|]\s*arXiv\s*$/i, "");
}

type Mode = "existing" | "new" | "paper";
type RepoMode = "use" | "fork";

export function NewProjectForm({
  onCreated,
  onCancel,
}: {
  onCreated: (project: Project) => void;
  onCancel?: () => void;
}) {
  const [mode, setMode] = useState<Mode>("paper");
  const [repoMode, setRepoMode] = useState<RepoMode>("use");
  const [repoInput, setRepoInput] = useState("");
  const [name, setName] = useState("");
  const [nameTouched, setNameTouched] = useState(false);
  const [branch, setBranch] = useState("main");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // "From a paper" mode.
  const [paperQuery, setPaperQuery] = useState("");
  const [hits, setHits] = useState<PaperHit[]>([]);
  const [searching, setSearching] = useState(false);
  const [paper, setPaper] = useState<ResolvedPaper | null>(null);
  const [resolving, setResolving] = useState(false);
  const [paperNote, setPaperNote] = useState<string | null>(null);
  // Drops out-of-order search/resolve responses.
  const paperSeq = useRef(0);

  const parsed = parseRepo(repoInput);
  const valid = Boolean(
    name.trim() &&
      (mode === "new" ||
        (mode === "existing" && parsed !== null) ||
        (mode === "paper" && paper !== null && (repoInput.trim() === "" || parsed !== null))),
  );

  const onRepoChange = (value: string) => {
    setRepoInput(value);
    // Name follows the repo until the user edits it themselves.
    if (!nameTouched) setName(parseRepo(value)?.repo ?? "");
  };

  async function selectPaper(id: string) {
    const seq = ++paperSeq.current;
    setHits([]);
    setSearching(false);
    setResolving(true);
    setPaperNote(null);
    try {
      const p = await resolvePaper(id);
      if (seq !== paperSeq.current) return;
      setPaper(p);
      const repo = p.repoUrl ? parseRepo(p.repoUrl) : null;
      setRepoInput(repo ? `${repo.owner}/${repo.repo}` : "");
      // Paper repos are rarely writable — default to a private copy.
      setRepoMode("fork");
      if (!nameTouched) setName(repo?.repo ?? (p.title ?? "").trim().slice(0, 60));
    } catch (err) {
      if (seq !== paperSeq.current) return;
      setPaperNote(err instanceof Error ? err.message : String(err));
    } finally {
      if (seq === paperSeq.current) setResolving(false);
    }
  }

  function clearPaper() {
    paperSeq.current++;
    setPaper(null);
    setPaperQuery("");
    setHits([]);
    setPaperNote(null);
    setRepoInput("");
    if (!nameTouched) setName("");
  }

  // Debounced lookup: an id/URL resolves directly, anything else title-searches.
  useEffect(() => {
    if (mode !== "paper" || paper) return;
    const q = paperQuery.trim();
    const id = parsePaperId(q);
    if (!id && q.length < 3) {
      setHits([]);
      setSearching(false);
      return;
    }
    const seq = ++paperSeq.current;
    if (!id) setSearching(true);
    const t = setTimeout(() => {
      if (id) {
        void selectPaper(id);
        return;
      }
      searchPapers(q)
        .then((res) => {
          if (seq === paperSeq.current) setHits(res);
        })
        .catch((err) => {
          if (seq !== paperSeq.current) return;
          setHits([]);
          setPaperNote(err instanceof Error ? err.message : String(err));
        })
        .finally(() => {
          if (seq === paperSeq.current) setSearching(false);
        });
    }, 350);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, paper, paperQuery]);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!valid || pending) return;
    setPending(true);
    setError(null);
    try {
      const project = await createProject(
        mode === "new"
          ? { name: name.trim(), createRepo: true }
          : mode === "paper" && !parsed
            ? { name: name.trim(), createRepo: true, paperId: paper!.paperId }
            : {
                name: name.trim(),
                githubOwner: parsed!.owner,
                githubRepo: parsed!.repo,
                baselineBranch: branch.trim() || "main",
                forkRepo: repoMode === "fork",
                ...(mode === "paper" ? { paperId: paper!.paperId } : {}),
              },
      );
      onCreated(project);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(false);
    }
  }

  const creatingRepo = mode === "new" || (mode === "paper" && !parsed);
  const repoFields = (
    <>
      <div className="seg form-seg">
        <button
          type="button"
          className={repoMode === "use" ? "active" : ""}
          onClick={() => setRepoMode("use")}
        >
          Use this repo
        </button>
        <button
          type="button"
          className={repoMode === "fork" ? "active" : ""}
          onClick={() => setRepoMode("fork")}
        >
          Fork a copy
        </button>
      </div>
      <span className="repo-hint">
        {repoMode === "fork"
          ? `snapshots ${parsed ? `${parsed.owner}/${parsed.repo}` : "the repo"} into a private repo on your account`
          : "experiments push branches here — if you can't push to it, a fork is made automatically"}
      </span>
      <div className="row2">
        <label>
          Project name
          <input
            value={name}
            onChange={(e) => {
              setNameTouched(true);
              setName(e.target.value);
            }}
            placeholder="my-research"
          />
        </label>
        <label>
          Branch
          <input value={branch} onChange={(e) => setBranch(e.target.value)} placeholder="main" />
        </label>
      </div>
    </>
  );

  return (
    <form className="form" onSubmit={submit}>
      <div className="seg form-seg">
        <button
          type="button"
          className={mode === "paper" ? "active" : ""}
          onClick={() => setMode("paper")}
        >
          From a paper
        </button>
        <button
          type="button"
          className={mode === "existing" ? "active" : ""}
          onClick={() => setMode("existing")}
        >
          Existing repo
        </button>
        <button
          type="button"
          className={mode === "new" ? "active" : ""}
          onClick={() => setMode("new")}
        >
          New blank repo
        </button>
      </div>

      {mode === "existing" && (
        <>
          <label>
            GitHub repository
            <input
              value={repoInput}
              onChange={(e) => onRepoChange(e.target.value)}
              placeholder="https://github.com/karpathy/nanoGPT"
              autoFocus
              spellCheck={false}
            />
            <span className={`repo-hint mono ${parsed ? "ok" : ""}`}>
              {parsed
                ? `${parsed.owner} / ${parsed.repo}`
                : repoInput.trim()
                  ? "paste a GitHub URL or owner/repo"
                  : "URL or owner/repo — cloned with your git credentials"}
            </span>
          </label>
          {repoFields}
        </>
      )}

      {mode === "paper" &&
        (paper === null ? (
          <>
            <label>
              Paper
              <input
                value={paperQuery}
                onChange={(e) => setPaperQuery(e.target.value)}
                placeholder="arXiv id, URL, or title — e.g. 1706.03762"
                autoFocus
                spellCheck={false}
              />
              <span className={`repo-hint ${paperNote ? "" : "mono"}`}>
                {resolving
                  ? "looking up paper…"
                  : searching
                    ? "searching alphaXiv…"
                    : (paperNote ?? "searches alphaXiv by title — or paste an arXiv id / URL")}
              </span>
            </label>
            {hits.length > 0 && (
              <div className="paper-results">
                {hits.map((h) => (
                  <button key={h.paperId} type="button" onClick={() => void selectPaper(h.paperId)}>
                    <span className="title">{cleanTitle(h.title)}</span>
                    <span className="id">{h.paperId}</span>
                  </button>
                ))}
              </div>
            )}
          </>
        ) : (
          <>
            <div className="paper-pick">
              <div className="meta">
                <div className="title">{paper.title || paper.paperId}</div>
                <div className="id">arXiv {paper.paperId}</div>
              </div>
              <button type="button" className="btn ghost" onClick={clearPaper}>
                Change
              </button>
            </div>
            <label>
              GitHub repository{paper.repoUrl ? "" : " (optional)"}
              <input
                value={repoInput}
                onChange={(e) => onRepoChange(e.target.value)}
                placeholder="owner/repo — leave blank for a new private repo"
                spellCheck={false}
              />
              <span className={`repo-hint mono ${parsed ? "ok" : ""}`}>
                {parsed
                  ? `${parsed.owner} / ${parsed.repo}` +
                    (paper.repoUrl && parseRepo(paper.repoUrl)?.repo === parsed.repo
                      ? ` · linked on alphaXiv${paper.repoStars != null ? ` · ★ ${paper.repoStars}` : ""}`
                      : "")
                  : repoInput.trim()
                    ? "paste a GitHub URL or owner/repo"
                    : "no code linked to this paper — a blank private repo will be created"}
              </span>
            </label>
            {parsed ? (
              repoFields
            ) : (
              <label>
                Project name
                <input
                  value={name}
                  onChange={(e) => {
                    setNameTouched(true);
                    setName(e.target.value);
                  }}
                  placeholder="my-research"
                />
                <span className={`repo-hint mono ${name.trim() ? "ok" : ""}`}>
                  {name.trim()
                    ? `creates github.com/you/${slugify(name)} · private`
                    : "a blank private repo is created on your GitHub account"}
                </span>
              </label>
            )}
          </>
        ))}

      {mode === "new" && (
        <label>
          Project name
          <input
            value={name}
            onChange={(e) => {
              setNameTouched(true);
              setName(e.target.value);
            }}
            placeholder="my-research"
            autoFocus
          />
          <span className={`repo-hint mono ${name.trim() ? "ok" : ""}`}>
            {name.trim()
              ? `creates github.com/you/${slugify(name)} · private`
              : "a blank private repo is created on your GitHub account"}
          </span>
        </label>
      )}

      {error && <div className="error">{error}</div>}
      <div className="actions">
        {onCancel && (
          <button type="button" className="btn ghost" onClick={onCancel}>
            Cancel
          </button>
        )}
        <button type="submit" className="btn primary" disabled={!valid || pending}>
          {pending
            ? creatingRepo
              ? "Creating repo…"
              : repoMode === "fork"
                ? "Forking repo…"
                : "Cloning repo…"
            : "Create project"}
        </button>
      </div>
    </form>
  );
}

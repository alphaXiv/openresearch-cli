import { useEffect, useRef, useState } from "react";
import { ChevronRight, FolderOpen } from "lucide-react";
import {
  createProject,
  getProjectPathStatus,
  pickProjectFolder,
  resolvePaper,
  searchPapers,
  type PaperHit,
  type Project,
  type ProjectPathStatus,
  type ResolvedPaper,
} from "../api";

function slugify(text: string): string {
  return (
    text
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 48) || "research-project"
  );
}

function parsePaperId(input: string): string | null {
  const last = input.trim().split(/[?#]/)[0].split("/").filter(Boolean).pop() ?? "";
  const id = last.replace(/\.(pdf|md)$/i, "");
  return /^\d{4}\.\d{4,5}(v\d+)?$/.test(id) ? id : null;
}

type Mode = "folder" | "paper";
type ProjectDraft = {
  name: string;
  nameTouched: boolean;
  path: string;
  pathTouched: boolean;
};

export function NewProjectForm({
  onCreated,
  onCancel,
}: {
  onCreated: (project: Project) => void;
  onCancel?: () => void;
}) {
  const [mode, setMode] = useState<Mode>("folder");
  const [name, setName] = useState("");
  const [nameTouched, setNameTouched] = useState(false);
  const [path, setPath] = useState("");
  const [pathTouched, setPathTouched] = useState(false);
  const [pathStatus, setPathStatus] = useState<ProjectPathStatus | null>(null);
  const [pathError, setPathError] = useState<string | null>(null);
  const [checkingPath, setCheckingPath] = useState(false);
  const [pickingFolder, setPickingFolder] = useState(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [paperQuery, setPaperQuery] = useState("");
  const [paper, setPaper] = useState<ResolvedPaper | null>(null);
  const [hits, setHits] = useState<PaperHit[]>([]);
  const [searching, setSearching] = useState(false);
  const seq = useRef(0);
  const pathSeq = useRef(0);
  const folderPickSeq = useRef(0);
  const drafts = useRef<Record<Mode, ProjectDraft>>({
    folder: { name: "", nameTouched: false, path: "", pathTouched: false },
    paper: { name: "", nameTouched: false, path: "", pathTouched: false },
  });

  useEffect(() => {
    if (mode !== "paper" || !paper || pathTouched) return;
    const nextPath = `~/OpenResearch/${slugify(name || paper.title || paper.paperId)}`;
    if (nextPath === path) return;
    setPathStatus(null);
    setCheckingPath(true);
    setPath(nextPath);
  }, [mode, name, paper, path, pathTouched]);

  useEffect(() => {
    const request = ++pathSeq.current;
    setCheckingPath(true);
    setPathError(null);
    const timer = setTimeout(() => {
      void getProjectPathStatus(path.trim())
        .then((status) => {
          if (request === pathSeq.current) setPathStatus(status);
        })
        .catch((err) => {
          if (request !== pathSeq.current) return;
          setPathStatus(null);
          setPathError(err instanceof Error ? err.message : String(err));
        })
        .finally(() => {
          if (request === pathSeq.current) setCheckingPath(false);
        });
    }, path.trim() ? 200 : 0);
    return () => clearTimeout(timer);
  }, [mode, path]);

  useEffect(() => {
    const request = ++seq.current;
    if (mode !== "paper" || paper) {
      setSearching(false);
      return;
    }
    const query = paperQuery.trim();
    const id = parsePaperId(query);
    if (!id && query.length < 3) {
      setHits([]);
      setSearching(false);
      return;
    }
    setSearching(true);
    const timer = setTimeout(() => {
      if (id) {
        void resolvePaper(id)
          .then((resolved) => {
            if (request !== seq.current) return;
            setPaper(resolved);
            if (!nameTouched) setName(resolved.title?.trim().slice(0, 60) || resolved.paperId);
          })
          .catch((err) => request === seq.current && setError(err instanceof Error ? err.message : String(err)))
          .finally(() => request === seq.current && setSearching(false));
        return;
      }
      void searchPapers(query)
        .then((results) => request === seq.current && setHits(results))
        .catch((err) => request === seq.current && setError(err instanceof Error ? err.message : String(err)))
        .finally(() => request === seq.current && setSearching(false));
    }, 350);
    return () => clearTimeout(timer);
  }, [mode, paper, paperQuery, nameTouched]);

  async function choosePaper(paperId: string) {
    const request = ++seq.current;
    setSearching(true);
    setError(null);
    try {
      const resolved = await resolvePaper(paperId);
      if (request !== seq.current) return;
      setPaper(resolved);
      setHits([]);
      if (!nameTouched) setName(resolved.title?.trim().slice(0, 60) || resolved.paperId);
    } catch (err) {
      if (request === seq.current) setError(err instanceof Error ? err.message : String(err));
    } finally {
      if (request === seq.current) setSearching(false);
    }
  }

  function changePaper() {
    seq.current += 1;
    folderPickSeq.current += 1;
    setPaper(null);
    setPaperQuery("");
    setHits([]);
    setSearching(false);
    setPickingFolder(false);
    setPath("");
    setPathTouched(false);
    if (!nameTouched) setName("");
  }

  function chooseMode(next: Mode) {
    if (next === mode) return;
    seq.current += 1;
    folderPickSeq.current += 1;
    drafts.current[mode] = { name, nameTouched, path, pathTouched };
    const nextDraft = drafts.current[next];
    setMode(next);
    setError(null);
    setPathError(null);
    setPathStatus(null);
    setSearching(false);
    setPickingFolder(false);
    setName(nextDraft.name);
    setNameTouched(nextDraft.nameTouched);
    setPath(nextDraft.path);
    setPathTouched(nextDraft.pathTouched);
  }

  async function chooseLocalFolder() {
    if (pickingFolder) return;
    const request = ++folderPickSeq.current;
    setPickingFolder(true);
    setError(null);
    try {
      const selected = await pickProjectFolder();
      if (request !== folderPickSeq.current || !selected) return;
      setPathTouched(true);
      if (selected !== path) {
        setPathStatus(null);
        setCheckingPath(true);
        setPath(selected);
      }
      if (mode === "folder" && !nameTouched) {
        const folderName = selected.replace(/[\\/]+$/, "").split(/[\\/]/).pop();
        if (folderName) setName(folderName);
      }
    } catch (err) {
      if (request === folderPickSeq.current) {
        setError(err instanceof Error ? err.message : String(err));
      }
    } finally {
      if (request === folderPickSeq.current) setPickingFolder(false);
    }
  }

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (!canCreate) return;
    setPending(true);
    setError(null);
    try {
      const result = await createProject({
        name: name.trim(),
        path: path.trim(),
        createFolder: mode === "paper",
        initializeGit: true,
        ...(mode === "paper" && paper
          ? { paperId: paper.paperId, cloneUrl: paper.repoUrl ?? undefined }
          : {}),
      });
      onCreated(result.project);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(false);
    }
  }

  const gitMissing = pathStatus?.gitVersion === null;
  const missingLocalFolder =
    mode === "folder" &&
    Boolean(path.trim()) &&
    pathStatus !== null &&
    pathStatus.exists === false;
  const invalidProjectDestination =
    Boolean(path.trim()) && pathStatus?.exists === true && pathStatus.directory === false;
  const nonemptyPaperCloneFolder =
    mode === "paper" && Boolean(paper?.repoUrl) && pathStatus?.empty === false;
  const paperDestinationHasError = invalidProjectDestination || nonemptyPaperCloneFolder;
  const paperDestinationDescription = invalidProjectDestination
    ? "Choose a different destination. This path is a file, not a folder."
    : nonemptyPaperCloneFolder
      ? "Choose a different destination. The paper repository needs a new or empty folder."
      : paper?.repoUrl
        ? pathStatus?.exists === false
          ? "OpenResearch will create this folder and clone the paper's repository into it."
          : "OpenResearch will clone the paper's repository here and use it as your local workspace."
        : pathStatus?.exists === false
          ? "OpenResearch will create this folder and initialize it as the local project workspace."
          : "OpenResearch will use this folder as the local workspace and initialize Git if needed.";
  const canCreate =
    Boolean(name.trim() && path.trim()) &&
    !pending &&
    !pickingFolder &&
    !checkingPath &&
    pathStatus !== null &&
    !pathError &&
    !gitMissing &&
    !missingLocalFolder &&
    !invalidProjectDestination &&
    !nonemptyPaperCloneFolder &&
    (mode !== "paper" || paper !== null);

  return (
    <form className="form new-project-form" onSubmit={submit}>
      <div className="seg form-seg">
        <button
          type="button"
          className={mode === "folder" ? "active" : ""}
          aria-pressed={mode === "folder"}
          onClick={() => chooseMode("folder")}
        >
          From folder
        </button>
        <button
          type="button"
          className={mode === "paper" ? "active" : ""}
          aria-pressed={mode === "paper"}
          onClick={() => chooseMode("paper")}
        >
          From a paper
        </button>
      </div>

      {mode === "paper" && !paper && (
        <label>
          Paper
          <input
            value={paperQuery}
            onChange={(event) => setPaperQuery(event.target.value)}
            placeholder="arXiv id, URL, or title"
            autoFocus
          />
          <span className="repo-hint">{searching ? "Searching alphaXiv…" : "The public code repository is cloned without credentials."}</span>
          {hits.length > 0 && (
            <div className="paper-results">
              {hits.map((hit) => (
                <button key={hit.paperId} type="button" onClick={() => void choosePaper(hit.paperId)}>
                  <span className="title">{hit.title}</span>
                  <span className="id">{hit.paperId}</span>
                </button>
              ))}
            </div>
          )}
        </label>
      )}

      {paper && mode === "paper" && (
        <div className="paper-pick">
          <div className="meta">
            <div className="title">{paper.title || paper.paperId}</div>
            <div className="id">
              {paper.repoUrl ? "Public code repository found" : "No public code repository found"}
            </div>
          </div>
          <button type="button" className="btn sm" aria-label="Change selected paper" onClick={changePaper}>
            Change
          </button>
        </div>
      )}

      {(mode !== "paper" || paper) && (
        <>
          {mode === "paper" ? (
            <div className="project-location-field">
              <div className="project-location-label">
                {paper?.repoUrl ? "Clone destination" : "Project location"}
              </div>
              <div className="paper-destination">
                <code title={path}>{path}</code>
                <button
                  type="button"
                  className="btn sm"
                  aria-label={`${paper?.repoUrl ? "Change clone destination" : "Change project location"}; current location: ${path}`}
                  aria-describedby="paper-destination-description"
                  disabled={pickingFolder}
                  onClick={() => void chooseLocalFolder()}
                >
                  {pickingFolder ? "Choosing…" : "Change…"}
                </button>
              </div>
              <span
                id="paper-destination-description"
                className={`folder-picker-hint${paperDestinationHasError ? " error" : ""}`}
              >
                {paperDestinationDescription}
              </span>
            </div>
          ) : (
            <button
              type="button"
              className="folder-picker-control"
              aria-label={path ? `Change project folder; current folder: ${path}` : "Choose or create a project folder"}
              disabled={pickingFolder}
              title={path || undefined}
              onClick={() => void chooseLocalFolder()}
            >
              <FolderOpen className={path ? "folder-picker-icon" : "folder-picker-icon placeholder"} size={16} />
              <span className={path ? "mono" : "placeholder"}>
                {pickingFolder ? "Choosing…" : path || "Choose or create a folder"}
              </span>
              <ChevronRight className="folder-picker-chevron" size={15} />
            </button>
          )}
          {path && (
            <label>
              <span className="project-field-label">Project name</span>
              <input
                value={name}
                onChange={(event) => {
                  setNameTouched(true);
                  setName(event.target.value);
                }}
                placeholder="my-research"
                autoFocus
              />
            </label>
          )}
          {gitMissing && (
            <div className="project-path-notice error">
              Git is required for experiments but is not installed. Install Git, then restart OpenResearch.
            </div>
          )}
          {!gitMissing && path.trim() && checkingPath && (
            <span className="repo-hint mono">Checking folder…</span>
          )}
          {!gitMissing && mode === "folder" && path.trim() && !checkingPath && pathStatus?.exists === false && (
            <div className="project-path-notice error">Choose an existing folder.</div>
          )}
          {!gitMissing && mode === "folder" && path.trim() && !checkingPath && invalidProjectDestination && (
            <div className="project-path-notice error">The selected path is not a folder.</div>
          )}
          {!gitMissing &&
            mode === "folder" &&
            !checkingPath &&
            pathStatus?.directory &&
            pathStatus.initialized === false && (
            <div className="project-path-notice">
              This folder is not a Git repository. OpenResearch will initialize Git here.
            </div>
            )}
          {pathError && <div className="project-path-notice error">{pathError}</div>}
        </>
      )}

      {error && <div className="error">{error}</div>}
      <div className="actions new-project-actions">
        {onCancel && <button type="button" className="btn" onClick={onCancel}>Cancel</button>}
        <button className="btn primary" disabled={!canCreate}>
          {pending
            ? "Creating…"
            : mode === "paper"
              ? paper?.repoUrl
                ? "Clone paper project"
                : "Create paper project"
              : "Create local project"}
        </button>
      </div>
    </form>
  );
}

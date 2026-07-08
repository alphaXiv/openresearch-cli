import type { SkillInfo } from "../api";

/** Slash-skill dropdown above the composer. Open/filter/keyboard state lives
 * in ChatPanel (it's derived from the draft); this just renders the matches. */
export function SkillMenu({
  skills,
  activeIndex,
  onPick,
  onHover,
}: {
  skills: SkillInfo[];
  activeIndex: number;
  onPick: (skill: SkillInfo) => void;
  onHover: (index: number) => void;
}) {
  return (
    <div className="skill-menu">
      {skills.map((s, i) => (
        <button
          key={s.name}
          type="button"
          className={`skill-item ${i === activeIndex ? "active" : ""}`}
          // mousedown + preventDefault keeps the textarea focused.
          onMouseDown={(e) => {
            e.preventDefault();
            onPick(s);
          }}
          onMouseEnter={() => onHover(i)}
        >
          <span className="skill-name">
            /{s.name} <span className="skill-hint">{s.argHint}</span>
          </span>
          <span className="skill-desc">{s.description}</span>
        </button>
      ))}
    </div>
  );
}

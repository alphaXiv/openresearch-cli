import { X } from "lucide-react";

/** A closable tab for dynamic tab strips (open sessions, open experiments).
 *  The close "x" is a span, not a button — it can't nest inside the tab button. */
export function ClosableTab({
  active,
  label,
  icon,
  onSelect,
  onClose,
}: {
  active: boolean;
  label: string;
  /** Optional leading adornment, e.g. a busy dot or branch icon. */
  icon?: React.ReactNode;
  onSelect: () => void;
  onClose: () => void;
}) {
  return (
    <button
      className={`tab closable ${active ? "active" : ""}`}
      onClick={onSelect}
      title={label}
    >
      {icon}
      <span className="tab-label">{label}</span>
      <span
        role="button"
        className="tab-close"
        title="Close tab"
        onClick={(e) => {
          e.stopPropagation();
          onClose();
        }}
      >
        <X size={12} />
      </span>
    </button>
  );
}

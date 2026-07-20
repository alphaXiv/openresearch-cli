// OpenResearch wordmark: logo mark + name, sized by the parent's font-size.
// Single source for the brand lockup (home, chat empty state, onboarding).
export function Wordmark() {
  return (
    <span className="wordmark">
      <svg viewBox="0 0 100 100" aria-hidden="true">
        <rect width="100" height="100" rx="8" fill="#9a2036" />
        <path
          d="M15.375 16.782v63.843a4 4 0 0 0 4 4h63.843c3.564 0 5.348-4.309 2.829-6.828L22.203 13.953c-2.52-2.52-6.828-.735-6.828 2.829"
          fill="#fff"
        />
      </svg>
      OpenResearch
    </span>
  );
}

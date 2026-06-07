/** Render rows as a left-aligned, space-padded table with a header row. */
export function printTable(headers: string[], rows: string[][]): void {
  const widths = headers.map((h, i) =>
    Math.max(h.length, ...rows.map((r) => (r[i] ?? "").length)),
  );
  const line = (cells: string[]) =>
    cells.map((c, i) => (c ?? "").padEnd(widths[i] ?? 0)).join("  ").trimEnd();

  console.log(line(headers));
  console.log(line(widths.map((w) => "─".repeat(w))));
  for (const row of rows) console.log(line(row));
}

/** Format an unknown SQL cell value for display. */
export function cell(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

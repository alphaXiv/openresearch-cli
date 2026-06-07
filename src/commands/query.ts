import { queryProject } from "../client.ts";
import { requireCredentials } from "../config.ts";
import { cell, printTable } from "../table.ts";

/** Runs one read-only DuckDB SQL statement against a project's evidence schema. */
export async function query(projectId: string | undefined, sql: string | undefined): Promise<void> {
  if (!projectId || !sql) {
    console.error('Usage: orx query <projectId> "<sql>"');
    process.exit(1);
  }
  const creds = await requireCredentials();
  const result = await queryProject(creds, projectId, sql);

  // The evidence cache warms asynchronously; tell the user when it isn't ready
  // so empty/partial results aren't mistaken for "no data".
  if (result.syncStatus !== "ready") {
    console.error(`[sync: ${result.syncStatus}] results may be incomplete`);
    for (const e of result.syncErrors) console.error(`  ${e}`);
  }

  if (result.columns.length === 0) {
    console.log("(no columns returned)");
    return;
  }

  printTable(
    result.columns,
    result.rows.map((row) => row.map(cell)),
  );

  if (result.moreRowsAvailable) {
    console.error(
      `\nShowing ${result.rowCount} of ${result.totalRowCount} rows (add LIMIT/OFFSET to page).`,
    );
  }
}

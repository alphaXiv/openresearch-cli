const LANGUAGE_ALIASES: Record<string, string> = {
  cjs: "javascript", console: "bash", cts: "typescript", fish: "bash",
  html: "markup", js: "javascript", jsx: "javascript", md: "markdown",
  mjs: "javascript", mts: "typescript", py: "python", rb: "ruby",
  sh: "bash", shell: "bash", svg: "markup", terminal: "bash",
  ts: "typescript", tsx: "typescript", xml: "markup", yml: "yaml", zsh: "bash",
};

const FILE_NAME_LANGUAGES: Record<string, string> = {
  containerfile: "docker", dockerfile: "docker", justfile: "makefile", makefile: "makefile",
};

export function resolveSyntaxLanguage(language: string): string {
  const normalized = language.trim().toLowerCase();
  return LANGUAGE_ALIASES[normalized] ?? normalized;
}

export function detectSyntaxLanguageFromFilePath(filePath: string | null | undefined): string | null {
  if (!filePath) return null;
  const fileName = filePath.split("/").pop()?.toLowerCase() ?? "";
  const aliasedFileLanguage = FILE_NAME_LANGUAGES[fileName];
  if (aliasedFileLanguage) return resolveSyntaxLanguage(aliasedFileLanguage);
  const extensionIndex = fileName.lastIndexOf(".");
  if (extensionIndex === -1 || extensionIndex === fileName.length - 1) return null;
  return resolveSyntaxLanguage(fileName.slice(extensionIndex + 1));
}

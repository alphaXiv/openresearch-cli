//! Slash-skills for the `orx up` chat — canned prompt templates the user
//! invokes as `/name <args>` from the composer. The UI lists them via
//! `/api/skills`; expansion happens server-side in `ChatHost::send_message`
//! so the transcript keeps the short `/name` form the user typed while the
//! harness receives the full prompt. Works identically across all harnesses.

/// One built-in skill. `template` may contain `{args}`, replaced with the text
/// after the command.
pub struct Skill {
    pub name: &'static str,
    pub description: &'static str,
    /// Shown greyed-out in the picker after the name (e.g. "<topic>").
    pub arg_hint: &'static str,
    pub template: &'static str,
    /// Substituted for `{args}` when the user gives none.
    pub no_args: &'static str,
}

const LIT_REVIEW_TEMPLATE: &str = r#"Perform a multi-hop literature review using alphaXiv.

Topic: {args}

Use the `orx` CLI (already installed; public endpoints, no login needed):
- `orx lit "<query>"` — full-text search across papers; returns ids, titles, abstracts, and page-anchored snippets (`--json` for machine-readable output).
- `orx paper <id>` — a paper's structured overview report (~10 KB); `--full` for the raw extracted text when the report lacks a detail.

Method — iterate; do not stop after one search:
1. Hop 1: run `orx lit` with 2-3 distinct phrasings of the topic. Skim titles/abstracts/snippets and pick the 3-5 most relevant papers.
2. Read them: `orx paper <id>` for each pick.
3. Next hop: from those reports, extract cited papers, author names, benchmark/method names, and field terminology you did not start with. Turn these into new `orx lit` queries and search again.
4. Repeat until a hop surfaces nothing relevantly new (typically 2-4 hops). Track which papers you have already seen so you don't re-read them.

Then write the review:
- Organize by theme, not by paper.
- For each theme: the key papers (id + title), what they claim, where they agree and disagree.
- Note open problems / gaps you noticed.
- Cite every claim with its paper id (e.g. 2401.12345) so the user can pull it with `orx paper`.
- End with a short "start here" reading list of the 3-5 most load-bearing papers.
"#;

pub const CATALOG: &[Skill] = &[Skill {
    name: "lit-review",
    description: "Multi-hop literature review via alphaXiv search",
    arg_hint: "<topic>",
    template: LIT_REVIEW_TEMPLATE,
    no_args: "(none given — ask the user what topic to review before searching)",
}];

/// Expand a leading `/name [args]` into the skill's full prompt. `None` when
/// the text is not a known slash-skill (sent to the harness untouched).
pub fn expand(text: &str) -> Option<String> {
    let rest = text.strip_prefix('/')?;
    let (cmd, args) = match rest.split_once(char::is_whitespace) {
        Some((cmd, args)) => (cmd, args.trim()),
        None => (rest.trim_end(), ""),
    };
    let skill = CATALOG.iter().find(|s| s.name == cmd)?;
    let args = if args.is_empty() { skill.no_args } else { args };
    Some(skill.template.replace("{args}", args))
}

#[cfg(test)]
mod tests {
    use super::expand;

    #[test]
    fn expands_known_skill_with_args() {
        let out = expand("/lit-review sparse autoencoders").unwrap();
        assert!(out.contains("Topic: sparse autoencoders"));
        assert!(out.contains("orx lit"));
    }

    #[test]
    fn expands_bare_invocation_to_ask() {
        let out = expand("/lit-review").unwrap();
        assert!(out.contains("ask the user"));
    }

    #[test]
    fn passes_through_unknown_or_plain_text() {
        assert!(expand("/unknown thing").is_none());
        assert!(expand("hello /lit-review").is_none());
        assert!(expand("just text").is_none());
    }
}

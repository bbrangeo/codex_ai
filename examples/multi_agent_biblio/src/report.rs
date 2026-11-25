use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::hash::Hash;
use std::ops::Range;

use biblatex::{Bibliography, Chunk, Entry, EntryType, Spanned};
use markdown_builder::Markdown;
use serde::Serialize;

use super::sources::{Reference, normalize_doi};
use super::{Critique, ResearchPlan};

/// Référence et appréciation retenues pour le lot bibliographique.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Selected {
    #[serde(flatten)]
    pub(super) reference: Reference,
    pub(super) relevance: String,
    pub(super) topic: String,
    pub(super) score: u8,
}

pub(super) fn title_key(title: &str) -> Option<String> {
    let key: String = title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    (!key.is_empty()).then_some(key)
}

fn enrich(kept: &mut Reference, other: Reference) {
    fn fill(kept: &mut Option<String>, other: Option<String>) {
        if kept.as_deref().is_none_or(|value| value.trim().is_empty()) {
            *kept = other.filter(|value| !value.trim().is_empty());
        }
    }

    if kept.authors.is_empty() {
        kept.authors = other.authors;
    }
    kept.year = kept.year.or(other.year);
    kept.open_access = kept.open_access.or(other.open_access);
    kept.citations = kept.citations.max(other.citations);
    fill(&mut kept.venue, other.venue);
    fill(&mut kept.doi, other.doi);
    fill(&mut kept.abstract_text, other.abstract_text);
    fill(&mut kept.url, other.url);
}

fn dedup<K: Eq + Hash>(
    refs: Vec<Selected>,
    key: impl Fn(&Reference) -> Option<K>,
) -> Vec<Selected> {
    let mut indices = HashMap::<K, usize>::new();
    let mut kept = Vec::<Selected>::new();
    for selected in refs {
        if let Some(key) = key(&selected.reference) {
            let index = *indices.entry(key).or_insert(kept.len());
            if index < kept.len() {
                enrich(&mut kept[index].reference, selected.reference);
                continue;
            }
        }
        kept.push(selected);
    }
    kept
}

/// Classe et déduplique les références avant d'appliquer la taille maximale du lot.
/// Une année inconnue est exclue uniquement si une borne est demandée.
pub(super) fn shortlist(
    mut refs: Vec<Selected>,
    from: Option<u32>,
    to: Option<u32>,
    max: usize,
) -> Vec<Selected> {
    if from.is_some() || to.is_some() {
        refs.retain(|selected| {
            selected.reference.year.is_some_and(|year| {
                from.is_none_or(|from| year >= from) && to.is_none_or(|to| year <= to)
            })
        });
    }
    let rank = |selected: &Selected| Reverse((selected.score, selected.reference.citations));
    // Garder les titres alternatifs des doublons DOI pour reconnaître les notices sans DOI.
    let mut title_dois = HashMap::<String, Option<String>>::new();
    for selected in &mut refs {
        let reference = &mut selected.reference;
        reference.doi = reference.doi.as_deref().and_then(normalize_doi);
        if let (Some(title), Some(doi)) = (title_key(&reference.title), reference.doi.as_ref()) {
            title_dois
                .entry(title)
                .and_modify(|existing| {
                    if existing.as_ref() != Some(doi) {
                        *existing = None;
                    }
                })
                .or_insert_with(|| Some(doi.clone()));
        }
    }
    refs.sort_by_key(rank);
    let mut refs = dedup(refs, |reference| reference.doi.clone());
    refs.sort_by_key(rank);
    let mut refs = dedup(refs, |reference| {
        if let Some(doi) = &reference.doi {
            return Some(Ok(doi.clone()));
        }
        let title = title_key(&reference.title)?;
        // Un titre ambigu ne doit enrichir aucune des références portant un DOI.
        Some(match title_dois.get(&title) {
            Some(Some(doi)) => Ok(doi.clone()),
            _ => Err(title),
        })
    });
    refs.sort_by_key(rank);
    // Pour un titre associé à plusieurs DOI, garder la meilleure notice sans
    // lui attribuer le résumé ou les métadonnées d'un autre DOI.
    let mut titles = HashSet::new();
    refs.retain(|selected| {
        title_key(&selected.reference.title).is_none_or(|title| titles.insert(title))
    });
    refs.truncate(max);
    refs
}

fn citations(draft: &str) -> impl Iterator<Item = (Range<usize>, &str)> {
    draft.match_indices("[@").filter_map(|(start, _)| {
        let rest = &draft[start + 2..];
        let end = rest.find(['[', ']', '\n', '\r'])?;
        let id = &rest[..end];
        (rest.as_bytes()[end] == b']' && !id.is_empty() && !id.contains(char::is_whitespace))
            .then_some((start..start + end + 3, id))
    })
}

fn citation_index(id: &str) -> Option<usize> {
    let number = id.strip_prefix('r')?;
    if number.starts_with('0') || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number.parse::<usize>().ok()?.checked_sub(1)
}

/// Signale les identifiants inconnus du brouillon, puis les références du lot non citées.
pub(super) fn citation_issues(draft: &str, refs: &[Selected]) -> Vec<String> {
    let mut issues = Vec::new();
    let mut cited = vec![false; refs.len()];
    let mut unknown = HashSet::new();
    for (_, id) in citations(draft) {
        if let Some(seen) = citation_index(id).and_then(|index| cited.get_mut(index)) {
            *seen = true;
        } else if unknown.insert(id) {
            issues.push(format!("Citation inconnue : [@{id}]"));
        }
    }
    for (index, seen) in cited.into_iter().enumerate() {
        if !seen {
            issues.push(format!("Référence non citée : [@r{}]", index + 1));
        }
    }
    issues
}

/// Données nécessaires au rendu d'un rapport pour un lot dans son ordre de citation.
pub(super) struct Report<'a> {
    pub(super) subject: &'a str,
    pub(super) plan: &'a ResearchPlan,
    pub(super) draft: &'a str,
    pub(super) refs: &'a [Selected],
    pub(super) verified: &'a HashSet<String>,
    pub(super) critique: Option<&'a Critique>,
    pub(super) summary: &'a str,
}

fn markdown_text(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        match ch {
            '\n' | '\r' => escaped.push(' '),
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '&' | '~' | '|' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn surname(author: &str) -> &str {
    if let Some((family, _)) = author.split_once(',') {
        family.trim()
    } else {
        author.split_whitespace().last().unwrap_or_default()
    }
}

fn year_label(year: Option<u32>) -> String {
    year.map_or_else(|| "s. d.".to_owned(), |year| year.to_string())
}

fn render_draft(out: &mut String, draft: &str, refs: &[Selected]) {
    let mut end = 0;
    for (span, id) in citations(draft) {
        out.push_str(&draft[end..span.start]);
        end = span.end;
        if let Some(index) = citation_index(id).filter(|&index| index < refs.len()) {
            let reference = &refs[index].reference;
            let mut authors = reference.authors.iter().map(|author| surname(author));
            let first = authors.next().filter(|name| !name.is_empty());
            let authors = match (first, authors.next(), authors.next()) {
                (Some(first), Some(_), Some(_)) => format!("{first} et al."),
                (Some(first), Some(second), None) if !second.is_empty() => {
                    format!("{first} et {second}")
                }
                (Some(first), _, _) => first.to_owned(),
                (None, _, _) => "Auteur inconnu".to_owned(),
            };
            let name = markdown_text(&authors);
            let year = year_label(reference.year);
            let _ = write!(out, "[({name}, {year})](#r{})", index + 1);
        } else {
            let _ = write!(out, "**⚠️ Citation inconnue : @{}**", markdown_text(id));
        }
    }
    out.push_str(&draft[end..]);
}

fn doi_url(doi: &str) -> String {
    let mut url = String::from("https://doi.org/");
    for byte in doi.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            url.push(char::from(byte));
        } else {
            let _ = write!(url, "%{byte:02X}");
        }
    }
    url
}

fn markdown_list(out: &mut String, label: &str, items: &[String]) {
    let _ = writeln!(
        out,
        "- **{label} :** {}",
        if items.is_empty() {
            "—".to_owned()
        } else {
            items
                .iter()
                .map(|item| markdown_text(item))
                .collect::<Vec<_>>()
                .join(" ; ")
        }
    );
}

/// Rend le plan, le brouillon annoté, la bibliographie et la dernière évaluation.
pub(super) fn markdown(report: Report<'_>) -> String {
    let Report {
        subject,
        plan,
        draft,
        refs,
        verified,
        critique,
        summary,
    } = report;
    let mut doc = Markdown::new();
    doc.header1(markdown_text(subject))
        .header2("Plan de recherche");
    let mut out = String::new();
    let bound =
        |year: Option<u32>| year.map_or_else(|| "sans borne".to_owned(), |year| year.to_string());
    let _ = writeln!(
        out,
        "- **Années (début / fin) :** {} / {}",
        bound(plan.year_from),
        bound(plan.year_to)
    );
    markdown_list(&mut out, "Inclusion", &plan.inclusion);
    markdown_list(&mut out, "Exclusion", &plan.exclusion);
    for (index, question) in plan.questions.iter().enumerate() {
        doc.elements.push(std::mem::take(&mut out).into());
        doc.header3(format!(
            "Question {} — {}",
            index + 1,
            markdown_text(&question.question)
        ));
        markdown_list(&mut out, "Mots-clés FR", &question.keywords_fr);
        markdown_list(&mut out, "Mots-clés EN", &question.keywords_en);
        markdown_list(&mut out, "Synonymes", &question.synonyms);
    }

    doc.elements.push(std::mem::take(&mut out).into());
    doc.header2("Synthèse bibliographique");
    render_draft(&mut out, draft, refs);
    // Conserver le Markdown du rédacteur tel quel, sans le replier à 80 colonnes.
    out.push('\n');
    doc.elements.push(std::mem::take(&mut out).into());
    doc.header2("Références");
    for (index, selected) in refs.iter().enumerate() {
        let reference = &selected.reference;
        let id = index + 1;
        let title = markdown_text(&reference.title);
        let authors = if reference.authors.is_empty() {
            "Auteur inconnu".to_owned()
        } else {
            markdown_text(&reference.authors.join(", "))
        };
        let year = year_label(reference.year);
        let _ = write!(
            out,
            "- <a id=\"r{id}\"></a> **{title}** — {authors} ({year})"
        );
        if let Some(venue) = &reference.venue {
            let _ = write!(out, " — {}", markdown_text(venue));
        }
        if let Some(doi) = reference.doi.as_deref().and_then(normalize_doi) {
            let status = if verified.contains(&doi) {
                "✅"
            } else {
                "⚠️"
            };
            let _ = write!(
                out,
                " — {status} DOI : [{}]({})",
                markdown_text(&doi),
                doi_url(&doi)
            );
        } else {
            out.push_str(" — ⚠️ DOI absent");
        }
        out.push('\n');
    }

    let issues = citation_issues(draft, refs);
    if !issues.is_empty() {
        doc.elements.push(std::mem::take(&mut out).into());
        doc.header2("Contrôle des citations");
        for issue in issues {
            let _ = writeln!(out, "- {}", markdown_text(&issue));
        }
    }
    if let Some(critique) = critique {
        doc.elements.push(std::mem::take(&mut out).into());
        doc.header2("Dernière critique");
        let _ = writeln!(out, "**Note :** {}/10\n", critique.score);
        markdown_list(&mut out, "Points forts", &critique.strengths);
        for issue in &critique.issues {
            let _ = writeln!(
                out,
                "- **{:?} :** {}",
                issue.severity,
                markdown_text(&issue.detail)
            );
        }
        markdown_list(&mut out, "Suggestions", &critique.suggestions);
        markdown_list(&mut out, "Sujets manquants", &critique.missing_topics);
    }
    doc.elements.push(out.into());
    if !summary.trim().is_empty() {
        doc.header2("Note de synthèse");
        doc.elements.push(format!("{}\n", summary.trim()).into());
    }
    doc.render()
}

/// Exporte le même lot, avec les identifiants `r1`, `r2`, etc. du brouillon.
pub(super) fn bibtex(refs: &[Selected]) -> String {
    let mut bibliography = Bibliography::new();
    for (index, selected) in refs.iter().enumerate() {
        let reference = &selected.reference;
        let mut entry = Entry::new(format!("r{}", index + 1), EntryType::Misc);
        entry.set(
            "title",
            vec![Spanned::detached(Chunk::Verbatim(
                reference.title.replace(['\n', '\r', '\t'], " "),
            ))],
        );
        let mut names = Vec::new();
        for author in &reference.authors {
            if !names.is_empty() {
                names.push(Spanned::detached(Chunk::Normal(" and ".to_owned())));
            }
            for (index, word) in author.split_whitespace().enumerate() {
                if index > 0 {
                    names.push(Spanned::detached(Chunk::Normal(" ".to_owned())));
                }
                names.push(Spanned::detached(if word.eq_ignore_ascii_case("and") {
                    Chunk::Verbatim(word.to_owned())
                } else {
                    Chunk::Normal(word.to_owned())
                }));
            }
        }
        if !names.is_empty() {
            entry.set("author", names);
        }
        for (name, value) in [
            ("year", reference.year.map(|year| year.to_string())),
            ("howpublished", reference.venue.clone()),
            ("abstract", reference.abstract_text.clone()),
            ("doi", reference.doi.as_deref().and_then(normalize_doi)),
            ("url", reference.url.clone()),
        ] {
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                entry.set(
                    name,
                    vec![Spanned::detached(Chunk::Normal(
                        value.replace(['\n', '\r', '\t'], " "),
                    ))],
                );
            }
        }
        bibliography.insert(entry);
    }
    // biblatex échappe ces caractères comme des accents TeX. En texte littéral,
    // les commandes explicites évitent de modifier le caractère qui suit.
    bibliography
        .to_bibtex_string()
        .replace(r"\^", r"\textasciicircum{}")
        .replace(r"\~", r"\textasciitilde{}")
}

#[cfg(test)]
mod tests {
    use super::super::{Issue, Question, Severity};
    use super::*;

    fn selected(title: &str, doi: Option<&str>, score: u8) -> Selected {
        Selected {
            reference: Reference {
                title: title.to_owned(),
                authors: vec!["Camille Martin".to_owned()],
                year: Some(2022),
                venue: None,
                doi: doi.map(str::to_owned),
                citations: None,
                open_access: None,
                abstract_text: None,
                url: None,
                source_tool: "test".to_owned(),
            },
            relevance: "Pertinent".to_owned(),
            topic: "Sujet".to_owned(),
            score,
        }
    }

    #[test]
    fn deduplicates_doi_then_title_and_reranks_after_enrichment() {
        let preferred = selected("Énergie : solaire !", None, 90);
        let named = selected("Autre titre", Some("10.1234/solaire"), 80);
        let mut alias = selected(
            "énergie solaire",
            Some("https://doi.org/10.1234/SOLAIRE"),
            70,
        );
        alias.reference.abstract_text = Some("Résumé conservé".to_owned());
        alias.reference.citations = Some(40);
        let mut other = selected("Concurrent", None, 90);
        other.reference.citations = Some(20);
        let refs = shortlist(vec![other, alias, named, preferred], None, None, 1);
        let best = &refs[0];
        assert_eq!(best.reference.title, "Énergie : solaire !");
        assert_eq!(best.reference.doi.as_deref(), Some("10.1234/solaire"));
        assert_eq!(
            best.reference.abstract_text.as_deref(),
            Some("Résumé conservé")
        );
        assert_eq!(best.reference.citations, Some(40));
    }

    #[test]
    fn deduplicates_an_ambiguous_title_without_mixing_dois() {
        let first = selected("Même titre", Some("10.1234/a"), 90);
        let mut second = selected("Même titre !", Some("10.1234/b"), 80);
        second.reference.venue = Some("Revue B".to_owned());
        let ambiguous = selected("Même titre", None, 85);
        let refs = shortlist(vec![second, ambiguous, first], None, None, 10);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].reference.doi.as_deref(), Some("10.1234/a"));
        assert!(refs[0].reference.venue.is_none());
    }

    #[test]
    fn applies_inclusive_year_bounds_before_merging_and_limiting() {
        let inside = selected("Dans la période", Some("10.1234/a"), 70);
        let mut outside = selected("Hors période", Some("10.1234/a"), 100);
        outside.reference.year = Some(2021);
        let mut unknown = selected("Date inconnue", None, 95);
        unknown.reference.year = None;
        let refs = shortlist(vec![inside, outside, unknown], Some(2022), Some(2022), 1);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].reference.title, "Dans la période");
    }

    #[test]
    fn reports_unknown_ids_once_and_all_uncited_entries() {
        let refs = [selected("A", None, 90), selected("B", None, 80)];
        let issues = citation_issues("Été [@r1] [@r3] [@r3] [@fake] [@r01] [@r0]", &refs);
        assert_eq!(
            issues,
            [
                "Citation inconnue : [@r3]",
                "Citation inconnue : [@fake]",
                "Citation inconnue : [@r01]",
                "Citation inconnue : [@r0]",
                "Référence non citée : [@r2]",
            ]
        );
    }

    #[test]
    fn markdown_links_known_ids_only_and_includes_plan_and_critique() {
        let refs = [
            selected("A", Some("10.1234/a"), 90),
            selected("B", Some("10.1234/b"), 80),
        ];
        let plan = ResearchPlan {
            questions: vec![Question {
                question: "Pourquoi ?".to_owned(),
                keywords_fr: vec!["solaire".to_owned()],
                keywords_en: vec!["solar".to_owned()],
                synonyms: vec!["photovoltaïque".to_owned()],
            }],
            year_from: Some(2000),
            year_to: Some(2022),
            inclusion: vec!["articles".to_owned()],
            exclusion: vec!["brevets".to_owned()],
        };
        let critique = Critique {
            score: 7.5,
            strengths: vec!["Clarté".to_owned()],
            issues: vec![Issue {
                severity: Severity::Majeure,
                detail: "Nuancer".to_owned(),
            }],
            suggestions: vec!["Comparer".to_owned()],
            missing_topics: vec!["Coûts".to_owned()],
        };
        let output = markdown(Report {
            subject: "Solaire",
            plan: &plan,
            draft: "Selon [@r1], pas [@r9].",
            refs: &refs,
            verified: &HashSet::from(["10.1234/a".to_owned()]),
            critique: Some(&critique),
            summary: "Dernière note",
        });
        for expected in [
            "[(Martin, 2022)](#r1)",
            "<a id=\"r1\"></a>",
            "Citation inconnue : @r9",
            "Référence non citée",
            "✅ DOI : [10.1234/a](https://doi.org/10.1234/a)",
            "⚠️ DOI : [10.1234/b](https://doi.org/10.1234/b)",
            "2000 / 2022",
            "Pourquoi ?",
            "solar",
            "photovoltaïque",
            "articles",
            "brevets",
            "7.5",
            "Clarté",
            "Majeure",
            "Nuancer",
            "Comparer",
            "Coûts",
            "Dernière note",
        ] {
            assert!(output.contains(expected), "Texte manquant : {expected}");
        }
        assert!(!output.contains("(#r9)"));
    }

    #[test]
    fn bibtex_preserves_keys_and_escapes_text_without_fabricating_missing_fields() {
        let mut first = selected(r"CO2 {A&B_50% #1 $ \ ^ ~", Some("10.1234/a"), 90);
        first.reference.authors = vec![
            "Martin, Camille".to_owned(),
            "Research and Development".to_owned(),
        ];
        first.reference.venue = Some("Sciences & techniques".to_owned());
        let mut second = selected("Sans date", None, 80);
        second.reference.year = None;
        second.reference.authors.clear();
        let output = bibtex(&[first, second]);
        use biblatex::ChunksExt;
        let parsed = Bibliography::parse(&output).expect("BibTeX doit rester lisible");
        let title = parsed.get("r1").expect("clé r1").title().expect("titre r1");
        assert_eq!(title.format_verbatim(), r"CO2 {A&B_50% #1 $ \ ^ ~");
        assert!(output.contains("author = {Martin, Camille and Research {and} Development},"));
        assert!(output.contains(r"howpublished = {Sciences \& techniques},"));
        let (first, second) = output
            .split_once("@misc{r2,")
            .expect("la deuxième clé doit être r2");
        assert!(first.starts_with("@misc{r1,"));
        assert!(first.contains("doi = {10.1234/a},"));
        assert!(first.contains("year = {2022},"));
        assert!(!second.contains("year ="));
        assert!(!second.contains("author ="));
        assert!(!second.contains("doi ="));
    }
}

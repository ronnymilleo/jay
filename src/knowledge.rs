//! Knowledge base persistence: one TOML file per kind inside `.nest/kb/`.
//!
//! Each file holds a `[[entries]]` array of `KnowledgeEntry`. Plain text,
//! so `git diff`/`log`/`merge` work directly — same philosophy as tasks.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::model::{KnowledgeEntry, KnowledgeKind};
use crate::project::{KB_DIR, NEST_DIR};

fn kb_dir(root: &Path) -> PathBuf {
    root.join(NEST_DIR).join(KB_DIR)
}

fn kb_file(root: &Path, kind: KnowledgeKind) -> PathBuf {
    kb_dir(root).join(kind.filename())
}

/// Wrapper for TOML serialization (array of tables). The status file also
/// carries the explicit current-summary reference so that setting the
/// reference and creating an entry is one atomic file update.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct EntriesFile {
    #[serde(default)]
    current_summary: Option<CurrentSummary>,
    #[serde(default)]
    entries: Vec<KnowledgeEntry>,
}

/// Explicit designation of the authoritative project summary. Historical
/// status entries are kept; designating never rewrites or promotes by tag.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CurrentSummary {
    /// Id of the designated status entry.
    pub entry_id: i64,
    /// RFC 3339 (with offset) designation time.
    pub set_at: String,
    /// Who designated it ("human" or "agent:<name>").
    pub set_by: String,
    /// Optional Git commit the summary claims to describe.
    #[serde(default)]
    pub commit: Option<String>,
}

fn load_file(root: &Path, kind: KnowledgeKind) -> Result<EntriesFile> {
    let path = kb_file(root, kind);
    if !path.is_file() {
        return Ok(EntriesFile::default());
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(toml::from_str(&text)?)
}

fn save_file(root: &Path, kind: KnowledgeKind, file: &EntriesFile) -> Result<()> {
    let path = kb_file(root, kind);
    let mut value = toml::Value::try_from(file)?;
    if path.exists() {
        let raw: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
        preserve_unknown(&raw, &mut value, &["entries", "current_summary"]);
        if let (Some(old), Some(new)) =
            (raw.get("current_summary"), value.get_mut("current_summary"))
        {
            preserve_unknown(old, new, &["entry_id", "set_at", "set_by", "commit"]);
        }
        if let (Some(old), Some(new)) = (
            raw.get("entries").and_then(toml::Value::as_array),
            value.get_mut("entries").and_then(toml::Value::as_array_mut),
        ) {
            for entry in new {
                if let Some(original) = old.iter().find(|o| o.get("id") == entry.get("id")) {
                    preserve_unknown(original, entry, crate::diag::KNOWN_KB_ENTRY_FIELDS);
                }
            }
        }
    }
    crate::project::atomic_write(&path, &toml::to_string_pretty(&value)?)
}

// Preserve extension values as raw TOML, including nested extension tables.
// Known nullable fields are deliberately excluded so clearing them still works.
fn preserve_unknown(old: &toml::Value, new: &mut toml::Value, known: &[&str]) {
    if let (Some(old), Some(new)) = (old.as_table(), new.as_table_mut()) {
        for (key, value) in old {
            if !known.contains(&key.as_str()) {
                new.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Loads all entries of a given kind (empty vec if the file is absent).
pub fn load_entries(root: &Path, kind: KnowledgeKind) -> Result<Vec<KnowledgeEntry>> {
    Ok(load_file(root, kind)?.entries)
}

/// Saves all entries of a given kind atomically (creating the dir as
/// needed), preserving the current-summary reference.
pub fn save_entries(root: &Path, kind: KnowledgeKind, entries: &[KnowledgeEntry]) -> Result<()> {
    let _lock = crate::service::ProjectLock::acquire(root)?;
    save_entries_locked(root, kind, entries)
}

fn save_entries_locked(root: &Path, kind: KnowledgeKind, entries: &[KnowledgeEntry]) -> Result<()> {
    let mut file = load_file(root, kind)?;
    file.entries = entries.to_vec();
    save_file(root, kind, &file)
}

/// Next available id for a given kind = max(existing ids) + 1 (1 if none).
pub fn next_id(root: &Path, kind: KnowledgeKind) -> Result<i64> {
    let entries = load_entries(root, kind)?;
    Ok(entries.iter().map(|e| e.id).max().unwrap_or(0) + 1)
}

/// Appends an entry and persists. Returns the saved entry (with id set).
/// Supersession references are validated before anything is written.
pub fn add_entry(root: &Path, mut entry: KnowledgeEntry) -> Result<KnowledgeEntry> {
    let _lock = crate::service::ProjectLock::acquire(root)?;
    let kind = entry.kind;
    entry.id = next_id(root, kind)?;
    let mut entries = load_entries(root, kind)?;
    validate_supersedes(&entry, &entries)?;
    entries.push(entry.clone());
    save_entries_locked(root, kind, &entries)?;
    Ok(entry)
}

/// Updates an existing entry by id. The updater function modifies it in place.
pub fn update_entry<F>(
    root: &Path,
    kind: KnowledgeKind,
    id: i64,
    updater: F,
) -> Result<KnowledgeEntry>
where
    F: FnOnce(&mut KnowledgeEntry),
{
    let _lock = crate::service::ProjectLock::acquire(root)?;
    let mut entries = load_entries(root, kind)?;
    let idx = entries
        .iter()
        .position(|e| e.id == id)
        .ok_or_else(|| anyhow::anyhow!("knowledge entry {} ({}) not found", id, kind.as_str()))?;
    updater(&mut entries[idx]);
    {
        let others: Vec<KnowledgeEntry> = entries.iter().filter(|e| e.id != id).cloned().collect();
        validate_supersedes(&entries[idx], &others)?;
    }
    entries[idx].updated_at = crate::model::now_ts();
    let updated = entries[idx].clone();
    save_entries_locked(root, kind, &entries)?;
    Ok(updated)
}

/// Validates supersession references: no self-reference, targets must exist
/// in the same kind, and no cycles. Superseded text is never touched.
pub fn validate_supersedes(entry: &KnowledgeEntry, others: &[KnowledgeEntry]) -> Result<()> {
    if entry.supersedes.is_empty() {
        return Ok(());
    }
    let mut seen = std::collections::BTreeSet::new();
    for s in &entry.supersedes {
        if *s == entry.id {
            anyhow::bail!("entry {} cannot supersede itself", entry.id);
        }
        if !seen.insert(*s) {
            anyhow::bail!("duplicate supersedes reference #{s} on entry {}", entry.id);
        }
        if !others.iter().any(|o| o.id == *s) {
            anyhow::bail!(
                "entry {} supersedes #{s}, which does not exist among {} entries",
                entry.id,
                entry.kind.as_str()
            );
        }
    }
    // cycle detection over supersedes edges
    let all: Vec<&KnowledgeEntry> = others.iter().collect();
    let edges = |id: i64| -> Vec<i64> {
        if id == entry.id {
            entry.supersedes.clone()
        } else {
            all.iter()
                .find(|o| o.id == id)
                .map(|o| o.supersedes.clone())
                .unwrap_or_default()
        }
    };
    let mut visited = std::collections::BTreeSet::new();
    let mut path = Vec::new();
    if let Some(cycle) = find_supersession_cycle(entry.id, &edges, &mut visited, &mut path) {
        anyhow::bail!(
            "supersession cycle: {}",
            cycle
                .iter()
                .map(|i| format!("#{i}"))
                .collect::<Vec<_>>()
                .join(" -> ")
        );
    }
    Ok(())
}

/// Returns the first supersession cycle among `entries` (id -> ... -> id).
pub fn supersession_cycle(entries: &[KnowledgeEntry]) -> Option<Vec<i64>> {
    let mut visited = std::collections::BTreeSet::new();
    for e in entries {
        let mut path = Vec::new();
        let edges = |id: i64| -> Vec<i64> {
            entries
                .iter()
                .find(|o| o.id == id)
                .map(|o| o.supersedes.clone())
                .unwrap_or_default()
        };
        if let Some(c) = find_supersession_cycle(e.id, &edges, &mut visited, &mut path) {
            return Some(c);
        }
    }
    None
}

fn find_supersession_cycle(
    id: i64,
    edges: &impl Fn(i64) -> Vec<i64>,
    visited: &mut std::collections::BTreeSet<i64>,
    path: &mut Vec<i64>,
) -> Option<Vec<i64>> {
    if path.contains(&id) {
        let start = path.iter().position(|x| *x == id).unwrap();
        let mut cycle = path[start..].to_vec();
        cycle.push(id);
        return Some(cycle);
    }
    if !visited.insert(id) {
        return None;
    }
    path.push(id);
    for d in edges(id) {
        if let Some(c) = find_supersession_cycle(d, edges, visited, path) {
            return Some(c);
        }
    }
    path.pop();
    None
}

// ===== authoritative current summary =====

/// The designated current-summary reference (None when never designated).
pub fn current_summary_ref(root: &Path) -> Result<Option<CurrentSummary>> {
    Ok(load_file(root, KnowledgeKind::Status)?.current_summary)
}

/// What "current status" retrieval returns: the designated summary plus
/// provenance, or the highest-id fallback labeled as a historical update.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CurrentStatus {
    pub entry: KnowledgeEntry,
    /// True when an explicit current-summary reference designates this entry.
    pub designated: bool,
    /// Provenance of the designation (None for the fallback).
    pub designation: Option<CurrentSummary>,
    /// Set for the fallback: never presented as authoritative.
    pub note: Option<String>,
}

pub const FALLBACK_NOTE: &str =
    "latest historical update; no current summary designated (set one with `jay kb set-current <id>` or MCP set_current_summary)";

/// Designates an existing status entry as the authoritative current summary.
/// Requires meaningful (nonblank) content; records actor/time; accepts an
/// optional Git commit reference. Historical entries are never promoted
/// automatically because they carry a `current` tag.
pub fn set_current_summary(
    root: &Path,
    entry_id: i64,
    actor: &str,
    commit: Option<String>,
) -> Result<CurrentSummary> {
    let _lock = crate::service::ProjectLock::acquire(root)?;
    let entry = get_entry(root, KnowledgeKind::Status, entry_id)?
        .ok_or_else(|| anyhow::anyhow!("status entry #{entry_id} not found"))?;
    if entry.content.trim().is_empty() {
        anyhow::bail!(
            "status entry #{entry_id} has blank content; a current summary must be meaningful"
        );
    }
    let cs = CurrentSummary {
        entry_id,
        set_at: crate::model::now_ts(),
        set_by: actor.to_string(),
        commit,
    };
    let mut file = load_file(root, KnowledgeKind::Status)?;
    file.current_summary = Some(cs.clone());
    save_file(root, KnowledgeKind::Status, &file)?;
    Ok(cs)
}

/// Creates a status entry and (optionally) designates it as current in ONE
/// atomic file update.
pub fn add_status_entry(
    root: &Path,
    mut entry: KnowledgeEntry,
    set_current: bool,
    actor: &str,
) -> Result<(KnowledgeEntry, Option<CurrentSummary>)> {
    let _lock = crate::service::ProjectLock::acquire(root)?;
    entry.kind = KnowledgeKind::Status;
    if set_current && entry.content.trim().is_empty() {
        anyhow::bail!("a current summary must have meaningful content");
    }
    entry.id = next_id(root, KnowledgeKind::Status)?;
    let mut file = load_file(root, KnowledgeKind::Status)?;
    validate_supersedes(&entry, &file.entries)?;
    file.entries.push(entry.clone());
    let cs = if set_current {
        let cs = CurrentSummary {
            entry_id: entry.id,
            set_at: crate::model::now_ts(),
            set_by: actor.to_string(),
            commit: entry.commit.clone(),
        };
        file.current_summary = Some(cs.clone());
        Some(cs)
    } else {
        None
    };
    save_file(root, KnowledgeKind::Status, &file)?;
    Ok((entry, cs))
}

/// Current status retrieval: the designated summary plus provenance; when
/// none is designated, the highest-id historical entry with an explicit
/// label. A missing pointer target falls back the same way (doctor flags it).
pub fn current_status(root: &Path) -> Result<Option<CurrentStatus>> {
    let file = load_file(root, KnowledgeKind::Status)?;
    if let Some(cs) = &file.current_summary {
        if let Some(entry) = file.entries.iter().find(|e| e.id == cs.entry_id) {
            return Ok(Some(CurrentStatus {
                entry: entry.clone(),
                designated: true,
                designation: Some(cs.clone()),
                note: None,
            }));
        }
    }
    Ok(file
        .entries
        .iter()
        .max_by_key(|e| e.id)
        .map(|entry| CurrentStatus {
            entry: entry.clone(),
            designated: false,
            designation: None,
            note: Some(FALLBACK_NOTE.to_string()),
        }))
}

// ===== freshness facts =====

/// Facts about a summary's freshness. Presents evidence, never an assertion
/// that the prose is wrong.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Freshness {
    /// Timestamp the summary was designated (or last updated, for fallback).
    pub reference_time: Option<String>,
    /// Task ids whose updated_at postdates the reference time.
    pub tasks_updated_after: Vec<i64>,
    /// KB entries ("kind#id") whose updated_at postdates the reference time.
    pub kb_updated_after: Vec<String>,
    /// Local git HEAD, when available.
    pub head_commit: Option<String>,
    /// Whether the recorded commit matches local HEAD (None when not
    /// comparable: no commit recorded, no repo, or commit unavailable).
    pub commit_matches_head: Option<bool>,
    /// True when any freshness fact suggests reviewing the summary.
    pub possibly_stale: bool,
    /// Human-readable facts (never "the prose is wrong" claims).
    pub facts: Vec<String>,
}

fn to_local(ts: &str) -> Option<chrono::DateTime<chrono::Local>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.with_timezone(&chrono::Local));
    }
    // heuristic: legacy naive timestamps are assumed to be local time
    if let Ok(n) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S") {
        use chrono::TimeZone;
        return chrono::Local.from_local_datetime(&n).single();
    }
    None
}

/// Local git HEAD commit of the project folder (None when absent/unavailable).
pub fn head_commit(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Gathers freshness facts for a current-status view.
pub fn summary_freshness(root: &Path, status: &CurrentStatus) -> Result<Freshness> {
    let reference = status
        .designation
        .as_ref()
        .map(|cs| cs.set_at.clone())
        .unwrap_or_else(|| status.entry.updated_at.clone());
    let ref_time = to_local(&reference);
    let mut facts = Vec::new();

    let mut tasks_updated_after = Vec::new();
    if let Some(rt) = &ref_time {
        for t in crate::tasks::load_tasks(root)? {
            if let Some(ts) = to_local(&t.updated_at) {
                if &ts > rt {
                    tasks_updated_after.push(t.id);
                }
            }
        }
        if !tasks_updated_after.is_empty() {
            facts.push(format!(
                "{} task(s) updated after this summary: {}",
                tasks_updated_after.len(),
                tasks_updated_after
                    .iter()
                    .map(|i| format!("#{i}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    let mut kb_updated_after = Vec::new();
    if let Some(rt) = &ref_time {
        for kind in KnowledgeKind::ALL {
            for e in load_entries(root, kind)? {
                if e.id == status.entry.id && kind == KnowledgeKind::Status {
                    continue;
                }
                if let Some(ts) = to_local(&e.updated_at) {
                    if &ts > rt {
                        kb_updated_after.push(format!("{}#{}", kind.as_str(), e.id));
                    }
                }
            }
        }
        if !kb_updated_after.is_empty() {
            facts.push(format!(
                "{} KB entry/entries updated after this summary: {}",
                kb_updated_after.len(),
                kb_updated_after.join(", ")
            ));
        }
    }

    let head = head_commit(root);
    let recorded = status
        .designation
        .as_ref()
        .and_then(|cs| cs.commit.clone())
        .or_else(|| status.entry.commit.clone());
    let commit_matches_head = match (&recorded, &head) {
        (Some(rec), Some(h)) => {
            let matches = h.starts_with(rec.as_str()) || rec.starts_with(h.as_str());
            if !matches {
                facts.push(format!(
                    "summary records commit {rec} but local HEAD is {h}"
                ));
            }
            Some(matches)
        }
        _ => None,
    };
    if head.is_none() && recorded.is_some() {
        facts.push("summary records a commit but no local git repo/HEAD is available".into());
    }

    let possibly_stale = !tasks_updated_after.is_empty()
        || !kb_updated_after.is_empty()
        || commit_matches_head == Some(false);

    Ok(Freshness {
        reference_time: Some(reference),
        tasks_updated_after,
        kb_updated_after,
        head_commit: head,
        commit_matches_head,
        possibly_stale,
        facts,
    })
}

/// Gets a single entry by id and kind.
pub fn get_entry(root: &Path, kind: KnowledgeKind, id: i64) -> Result<Option<KnowledgeEntry>> {
    let entries = load_entries(root, kind)?;
    Ok(entries.into_iter().find(|e| e.id == id))
}

/// Filters entries by optional tag (case-insensitive substring match on any tag).
pub fn filter_entries(
    root: &Path,
    kind: Option<KnowledgeKind>,
    tag: Option<&str>,
) -> Result<Vec<KnowledgeEntry>> {
    let kinds = match kind {
        Some(k) => vec![k],
        None => KnowledgeKind::ALL.to_vec(),
    };
    let mut result = Vec::new();
    for k in kinds {
        let entries = load_entries(root, k)?;
        for e in entries {
            if let Some(t) = tag {
                let t_lower = t.to_lowercase();
                if !e
                    .tags
                    .iter()
                    .any(|tag| tag.to_lowercase().contains(&t_lower))
                {
                    continue;
                }
            }
            result.push(e);
        }
    }
    result.sort_by_key(|e| e.id);
    Ok(result)
}

/// Returns the latest status entry (highest id), or None if no status exists.
pub fn latest_status(root: &Path) -> Result<Option<KnowledgeEntry>> {
    let entries = load_entries(root, KnowledgeKind::Status)?;
    Ok(entries.into_iter().max_by_key(|e| e.id))
}

// ===== BM25 Search =====
//
// BM25 (Best Match 25) is the ranking algorithm behind Elasticsearch, Lucene,
// and most keyword search engines. It scores documents against a query using
// three signals:
//
//   1. IDF (Inverse Document Frequency) — rare terms across the corpus score
//      higher. "Odin" in 2 of 20 entries is more informative than "the" in all.
//      Formula: ln((N - n + 0.5) / (n + 0.5) + 1)
//      where N = total docs, n = docs containing the term.
//
//   2. TF (Term Frequency) with saturation — a term appearing 3x is better
//      than 1x, but 30x isn't 10x better than 3x. The k1 parameter controls
//      how quickly returns diminish. Typical k1 = 1.2–2.0.
//      Formula: (tf * (k1 + 1)) / (tf + k1)
//
//   3. Length normalization — short documents aren't penalized for having
//      fewer words. A 10-word entry mentioning "TOML" once should rank higher
//      than a 500-word entry mentioning it once. The b parameter controls how
//      much length matters. b=0 ignores length; b=1 fully normalizes. Typical b=0.75.
//      Formula: tf_norm = tf / (1 - b + b * (doc_len / avg_doc_len))
//
// Final score for a document = sum over query terms of: IDF(term) * TF_saturated(term, doc)
//
// We also boost tag matches (tags are high-signal metadata) and title matches
// (title terms are more important than body terms).

/// BM25 tuning parameters.
const K1: f64 = 1.5; // term frequency saturation
const B: f64 = 0.75; // length normalization strength
const TAG_BOOST: f64 = 3.0; // multiplier for tag matches
const TITLE_BOOST: f64 = 2.0; // multiplier for title matches

/// A search result with its BM25 relevance score.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub entry: KnowledgeEntry,
    pub score: f64,
}

/// Tokenizes text into lowercase alphanumeric words (min 2 chars).
/// Simple but effective for technical content — no stemming needed at this scale.
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|s| s.trim_matches('_').to_string())
        .filter(|s| s.len() >= 2)
        .collect()
}

/// Searches all knowledge entries using BM25 ranking.
///
/// `kind_filter`: if Some, only search that kind. If None, search all kinds.
/// Returns results sorted by score descending, filtered to score > 0.
pub fn search(
    root: &Path,
    query: &str,
    kind_filter: Option<KnowledgeKind>,
) -> Result<Vec<SearchResult>> {
    // Load all candidate entries
    let kinds = match kind_filter {
        Some(k) => vec![k],
        None => KnowledgeKind::ALL.to_vec(),
    };
    let mut entries = Vec::new();
    for k in &kinds {
        entries.extend(load_entries(root, *k)?);
    }

    if entries.is_empty() || query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let query_terms = tokenize(query);
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }

    let n_docs = entries.len() as f64;

    // Pre-compute tokenized fields for each entry
    let entry_tokens: Vec<(Vec<String>, Vec<String>, Vec<String>)> = entries
        .iter()
        .map(|e| {
            let title_tokens = tokenize(&e.title);
            let content_tokens = tokenize(&e.content);
            let tag_tokens: Vec<String> = e.tags.iter().flat_map(|t| tokenize(t)).collect();
            (title_tokens, content_tokens, tag_tokens)
        })
        .collect();

    // Compute average document length (all tokens combined)
    let total_len: usize = entry_tokens
        .iter()
        .map(|(t, c, g)| t.len() + c.len() + g.len())
        .sum();
    let avg_len = if n_docs > 0.0 {
        total_len as f64 / n_docs
    } else {
        1.0
    };

    // For each query term, compute IDF and score each document
    let mut scores = vec![0.0_f64; entries.len()];

    for term in &query_terms {
        // Count how many documents contain this term (in any field)
        let doc_freq = entry_tokens
            .iter()
            .filter(|(title, content, tags)| {
                title.contains(term) || content.contains(term) || tags.contains(term)
            })
            .count() as f64;

        // IDF: rare terms get higher scores
        // ln((N - df + 0.5) / (df + 0.5) + 1)
        let idf = ((n_docs - doc_freq + 0.5) / (doc_freq + 0.5) + 1.0).ln();

        for (i, (title_tokens, content_tokens, tag_tokens)) in entry_tokens.iter().enumerate() {
            let doc_len = (title_tokens.len() + content_tokens.len() + tag_tokens.len()) as f64;

            // Count term frequency in each field
            let tf_content = content_tokens.iter().filter(|t| *t == term).count() as f64;
            let tf_title = title_tokens.iter().filter(|t| *t == term).count() as f64;
            let tf_tags = tag_tokens.iter().filter(|t| *t == term).count() as f64;

            // Length-normalized denominator
            let norm = 1.0 - B + B * (doc_len / avg_len);

            // BM25 TF component with saturation: (tf * (k1 + 1)) / (tf + k1 * norm)
            // Applied separately per field so boosts work correctly
            let score_content = if tf_content > 0.0 {
                let tf_sat = (tf_content * (K1 + 1.0)) / (tf_content + K1 * norm);
                idf * tf_sat
            } else {
                0.0
            };

            let score_title = if tf_title > 0.0 {
                let tf_sat = (tf_title * (K1 + 1.0)) / (tf_title + K1 * norm);
                idf * tf_sat * TITLE_BOOST
            } else {
                0.0
            };

            let score_tags = if tf_tags > 0.0 {
                // Tags don't need length normalization — they're always short
                let tf_sat = (tf_tags * (K1 + 1.0)) / (tf_tags + K1);
                idf * tf_sat * TAG_BOOST
            } else {
                0.0
            };

            scores[i] += score_content + score_title + score_tags;
        }
    }

    // Collect results with positive scores, sorted by score descending
    let mut results: Vec<SearchResult> = entries
        .into_iter()
        .zip(scores)
        .filter(|(_, score)| *score > 0.0)
        .map(|(entry, score)| SearchResult { entry, score })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::init_project;
    use std::fs;
    use std::path::Path;

    fn proj() -> tempfile::TempDir {
        let base = tempfile::tempdir().unwrap();
        let p = base.path().join("p");
        fs::create_dir_all(&p).unwrap();
        init_project(&p, None).unwrap();
        base
    }

    #[test]
    fn empty_when_no_file() {
        let root = proj();
        assert!(load_entries(root.path(), KnowledgeKind::Decision)
            .unwrap()
            .is_empty());
        assert_eq!(next_id(root.path(), KnowledgeKind::Decision).unwrap(), 1);
    }

    #[test]
    fn add_and_load_roundtrip() {
        let root = proj();
        let r = root.path();
        let e = KnowledgeEntry::new(
            0,
            KnowledgeKind::Decision,
            "Use Odin".into(),
            "raylib".into(),
        );
        let saved = add_entry(r, e).unwrap();
        assert_eq!(saved.id, 1);

        let loaded = load_entries(r, KnowledgeKind::Decision).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "Use Odin");

        // second entry gets id 2
        let e2 = KnowledgeEntry::new(
            0,
            KnowledgeKind::Decision,
            "TOML over JSON".into(),
            "diffable".into(),
        );
        let saved2 = add_entry(r, e2).unwrap();
        assert_eq!(saved2.id, 2);
        assert_eq!(load_entries(r, KnowledgeKind::Decision).unwrap().len(), 2);
    }

    #[test]
    fn update_modifies_existing() {
        let root = proj();
        let r = root.path();
        let e = KnowledgeEntry::new(0, KnowledgeKind::Note, "original".into(), "v1".into());
        let saved = add_entry(r, e).unwrap();

        let updated = update_entry(r, KnowledgeKind::Note, saved.id, |entry| {
            entry.content = "v2".into();
            entry.tags = vec!["updated".into()];
        })
        .unwrap();
        assert_eq!(updated.content, "v2");
        assert_eq!(updated.tags, vec!["updated"]);

        let reloaded = get_entry(r, KnowledgeKind::Note, saved.id)
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.content, "v2");
    }

    #[test]
    fn update_missing_returns_error() {
        let root = proj();
        let result = update_entry(root.path(), KnowledgeKind::Note, 999, |_| {});
        assert!(result.is_err());
    }

    #[test]
    fn filter_by_kind_and_tag() {
        let root = proj();
        let r = root.path();

        let mut d1 = KnowledgeEntry::new(0, KnowledgeKind::Decision, "Odin".into(), "c".into());
        d1.tags = vec!["stack".into()];
        add_entry(r, d1).unwrap();

        let mut d2 = KnowledgeEntry::new(0, KnowledgeKind::Decision, "TOML".into(), "c".into());
        d2.tags = vec!["format".into()];
        add_entry(r, d2).unwrap();

        let mut n1 = KnowledgeEntry::new(0, KnowledgeKind::Note, "impl detail".into(), "c".into());
        n1.tags = vec!["stack".into()];
        add_entry(r, n1).unwrap();

        // all decisions
        let decs = filter_entries(r, Some(KnowledgeKind::Decision), None).unwrap();
        assert_eq!(decs.len(), 2);

        // all with tag "stack" across kinds
        let stacked = filter_entries(r, None, Some("stack")).unwrap();
        assert_eq!(stacked.len(), 2);

        // decisions with tag "format"
        let fmt_decs = filter_entries(r, Some(KnowledgeKind::Decision), Some("format")).unwrap();
        assert_eq!(fmt_decs.len(), 1);
        assert_eq!(fmt_decs[0].title, "TOML");
    }

    #[test]
    fn latest_status_returns_highest_id() {
        let root = proj();
        let r = root.path();

        assert!(latest_status(r).unwrap().is_none());

        add_entry(
            r,
            KnowledgeEntry::new(0, KnowledgeKind::Status, "v1".into(), "old".into()),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(0, KnowledgeKind::Status, "v2".into(), "current".into()),
        )
        .unwrap();

        let latest = latest_status(r).unwrap().unwrap();
        assert_eq!(latest.title, "v2");
        assert_eq!(latest.content, "current");
    }

    #[test]
    fn different_kinds_are_independent() {
        let root = proj();
        let r = root.path();

        add_entry(
            r,
            KnowledgeEntry::new(0, KnowledgeKind::Decision, "d".into(), "".into()),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(0, KnowledgeKind::Note, "n".into(), "".into()),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(0, KnowledgeKind::Status, "s".into(), "".into()),
        )
        .unwrap();

        assert_eq!(load_entries(r, KnowledgeKind::Decision).unwrap().len(), 1);
        assert_eq!(load_entries(r, KnowledgeKind::Note).unwrap().len(), 1);
        assert_eq!(load_entries(r, KnowledgeKind::Status).unwrap().len(), 1);

        // each kind has its own id sequence
        assert_eq!(next_id(r, KnowledgeKind::Decision).unwrap(), 2);
        assert_eq!(next_id(r, KnowledgeKind::Note).unwrap(), 2);
        assert_eq!(next_id(r, KnowledgeKind::Status).unwrap(), 2);
    }

    #[test]
    fn tokenize_splits_and_lowercases() {
        let tokens = tokenize("Hello World! TOML-based config.");
        assert_eq!(tokens, vec!["hello", "world", "toml", "based", "config"]);
    }

    #[test]
    fn tokenize_filters_short_words() {
        let tokens = tokenize("a I is an the of to TOML");
        // single-char words filtered out
        assert_eq!(tokens, vec!["is", "an", "the", "of", "to", "toml"]);
    }

    #[test]
    fn search_empty_returns_nothing() {
        let root = proj();
        let results = search(root.path(), "anything", None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_finds_matching_entries() {
        let root = proj();
        let r = root.path();

        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Decision,
                "Use Odin".into(),
                "raylib graphics".into(),
            ),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Decision,
                "Use TOML".into(),
                "config format".into(),
            ),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Note,
                "MCP server".into(),
                "stdio transport".into(),
            ),
        )
        .unwrap();

        let results = search(r, "odin", None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.title, "Use Odin");
        assert!(results[0].score > 0.0);
    }

    #[test]
    fn search_ranks_title_match_higher_than_content() {
        let root = proj();
        let r = root.path();

        // "TOML" in title
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Decision,
                "TOML format".into(),
                "good for configs".into(),
            ),
        )
        .unwrap();
        // "TOML" only in content
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Note,
                "Config notes".into(),
                "we use TOML for everything".into(),
            ),
        )
        .unwrap();

        let results = search(r, "toml", None).unwrap();
        assert_eq!(results.len(), 2);
        // Title match should rank first due to TITLE_BOOST
        assert_eq!(results[0].entry.title, "TOML format");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn search_tag_match_boosts_ranking() {
        let root = proj();
        let r = root.path();

        // "odin" in tag
        let mut e1 = KnowledgeEntry::new(
            0,
            KnowledgeKind::Note,
            "Graphics lib".into(),
            "using raylib for rendering".into(),
        );
        e1.tags = vec!["odin".into()];
        add_entry(r, e1).unwrap();

        // "odin" only in content
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Note,
                "Language choice".into(),
                "we picked odin over bevy".into(),
            ),
        )
        .unwrap();

        let results = search(r, "odin", None).unwrap();
        assert_eq!(results.len(), 2);
        // Tag match should rank first due to TAG_BOOST
        assert_eq!(results[0].entry.title, "Graphics lib");
    }

    #[test]
    fn search_rare_term_scores_higher() {
        let root = proj();
        let r = root.path();

        // "rust" appears in many entries (common term -> lower IDF)
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Note,
                "Rust basics".into(),
                "rust is great".into(),
            ),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Note,
                "Rust advanced".into(),
                "rust macros".into(),
            ),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Note,
                "Rust tools".into(),
                "rust cargo".into(),
            ),
        )
        .unwrap();

        // "odin" appears in only one entry (rare term -> higher IDF)
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Decision,
                "Pick Odin".into(),
                "odin and rust are both options".into(),
            ),
        )
        .unwrap();

        // Searching "odin rust" — the entry with BOTH should rank highest,
        // but the rare term "odin" contributes more per-occurrence than "rust"
        let results = search(r, "odin", None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.title, "Pick Odin");
    }

    #[test]
    fn search_filters_by_kind() {
        let root = proj();
        let r = root.path();

        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Decision,
                "TOML decision".into(),
                "use toml".into(),
            ),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Note,
                "TOML note".into(),
                "use toml".into(),
            ),
        )
        .unwrap();

        let all = search(r, "toml", None).unwrap();
        assert_eq!(all.len(), 2);

        let decisions_only = search(r, "toml", Some(KnowledgeKind::Decision)).unwrap();
        assert_eq!(decisions_only.len(), 1);
        assert_eq!(decisions_only[0].entry.kind, KnowledgeKind::Decision);
    }

    #[test]
    fn search_scores_descending() {
        let root = proj();
        let r = root.path();

        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Decision,
                "Odin choice".into(),
                "picked odin".into(),
            ),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Note,
                "Other stuff".into(),
                "nothing relevant here at all".into(),
            ),
        )
        .unwrap();
        add_entry(
            r,
            KnowledgeEntry::new(
                0,
                KnowledgeKind::Note,
                "Odin details".into(),
                "odin raylib integration details".into(),
            ),
        )
        .unwrap();

        let results = search(r, "odin", None).unwrap();
        // Should have 2 results (not the irrelevant one), sorted by score desc
        assert_eq!(results.len(), 2);
        assert!(results[0].score >= results[1].score);
    }

    // ===== stage 5: authoritative summary =====

    fn status(root: &Path, title: &str, content: &str) -> KnowledgeEntry {
        add_entry(
            root,
            KnowledgeEntry::new(0, KnowledgeKind::Status, title.into(), content.into()),
        )
        .unwrap()
    }

    #[test]
    fn fallback_is_labeled_and_never_promoted_by_tag() {
        let root = proj();
        let r = root.path();
        let mut old =
            KnowledgeEntry::new(0, KnowledgeKind::Status, "old".into(), "old state".into());
        old.tags = vec!["current".into()];
        add_entry(r, old).unwrap();
        status(r, "newer delivery", "delivered X");

        let cs = current_status(r).unwrap().unwrap();
        assert!(!cs.designated);
        assert_eq!(cs.entry.title, "newer delivery", "highest-id fallback");
        assert_eq!(cs.note.as_deref(), Some(FALLBACK_NOTE));

        // the legacy 'current' tag triggers a doctor warning, never promotion
        let f = crate::diag::inspect(r);
        assert!(f.iter().any(|x| x.code == "kb.legacy_current_tag"), "{f:?}");
        // and nothing was rewritten
        let entries = load_entries(r, KnowledgeKind::Status).unwrap();
        assert!(entries[0].tags.contains(&"current".to_string()));
    }

    #[test]
    fn designated_summary_wins_over_later_entries() {
        let root = proj();
        let r = root.path();
        let chosen = status(r, "authoritative", "the real current state");
        status(r, "later ordinary update", "some review note");

        let cs_ref = set_current_summary(r, chosen.id, "human", Some("abc123".into())).unwrap();
        assert_eq!(cs_ref.entry_id, chosen.id);
        assert_eq!(cs_ref.set_by, "human");
        assert_eq!(cs_ref.commit.as_deref(), Some("abc123"));

        let cs = current_status(r).unwrap().unwrap();
        assert!(cs.designated);
        assert_eq!(
            cs.entry.id, chosen.id,
            "chosen summary wins over later entry"
        );
        assert_eq!(cs.designation.unwrap().commit.as_deref(), Some("abc123"));

        // history stays available
        let all = load_entries(r, KnowledgeKind::Status).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn set_current_requires_existing_entry_with_content() {
        let root = proj();
        let r = root.path();
        assert!(set_current_summary(r, 99, "human", None).is_err());
        let blank = status(r, "blank", "   ");
        assert!(set_current_summary(r, blank.id, "human", None).is_err());
    }

    #[test]
    fn add_status_entry_can_designate_atomically() {
        let root = proj();
        let r = root.path();
        let (entry, cs) = {
            let mut e = KnowledgeEntry::new(0, KnowledgeKind::Status, "s".into(), "content".into());
            e.commit = Some("deadbeef".into());
            add_status_entry(r, e, true, "agent:x").unwrap()
        };
        let cs = cs.unwrap();
        assert_eq!(cs.entry_id, entry.id);
        assert_eq!(cs.commit.as_deref(), Some("deadbeef"));
        // one file holds both entries and the pointer
        let text = std::fs::read_to_string(r.join(".nest/kb/status.toml")).unwrap();
        assert!(text.contains("[current_summary]"));
        assert!(text.contains("[[entries]]"));
        assert!(current_status(r).unwrap().unwrap().designated);
        // blank content refused for designation
        let e = KnowledgeEntry::new(0, KnowledgeKind::Status, "b".into(), " ".into());
        assert!(add_status_entry(r, e, true, "human").is_err());
    }

    #[test]
    fn supersession_validated_and_text_preserved() {
        let root = proj();
        let r = root.path();
        let first = status(r, "first conclusion", "old conclusion text");
        // missing target rejected
        let mut bad = KnowledgeEntry::new(0, KnowledgeKind::Status, "x".into(), "c".into());
        bad.supersedes = vec![999];
        assert!(add_entry(r, bad).is_err());
        // valid supersession; superseded text preserved
        let mut second =
            KnowledgeEntry::new(0, KnowledgeKind::Status, "revised".into(), "new".into());
        second.supersedes = vec![first.id];
        let second = add_entry(r, second).unwrap();
        let first_after = get_entry(r, KnowledgeKind::Status, first.id)
            .unwrap()
            .unwrap();
        assert_eq!(first_after.content, "old conclusion text");
        // cycles rejected: first -> second while second -> first
        update_entry(r, KnowledgeKind::Status, first.id, |e| {
            e.supersedes = vec![second.id];
        })
        .unwrap_err();
        // self reference rejected
        update_entry(r, KnowledgeKind::Status, first.id, |e| {
            e.supersedes = vec![first.id];
        })
        .unwrap_err();
    }

    #[test]
    fn missing_current_pointer_falls_back_and_doctor_flags() {
        let root = proj();
        let r = root.path();
        status(r, "s1", "c1");
        set_current_summary(r, 1, "human", None).unwrap();
        // hand-corrupt the pointer
        let path = r.join(".nest/kb/status.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("entry_id = 1", "entry_id = 42")).unwrap();
        let cs = current_status(r).unwrap().unwrap();
        assert!(!cs.designated, "missing pointer target falls back");
        let f = crate::diag::inspect(r);
        assert!(
            f.iter().any(|x| x.code == "kb.current_summary_missing"),
            "{f:?}"
        );
    }

    #[test]
    fn freshness_flags_later_task_updates_and_commit_mismatch() {
        let root = proj();
        let r = root.path();
        let s = status(r, "summary", "state at time T");
        set_current_summary(r, s.id, "human", Some("ffffffff".into())).unwrap();
        // a task updated after the summary
        std::thread::sleep(std::time::Duration::from_secs(1));
        let t = crate::model::Task::new(1, "later".into(), String::new());
        crate::tasks::save_task(r, &t).unwrap();

        let cs = current_status(r).unwrap().unwrap();
        let fresh = summary_freshness(r, &cs).unwrap();
        assert!(fresh.possibly_stale);
        assert_eq!(fresh.tasks_updated_after, vec![1]);
        // no git repo in the temp dir: commit comparison unavailable, fact recorded
        assert!(fresh.head_commit.is_none());
        assert!(fresh.commit_matches_head.is_none());
        assert!(
            fresh.facts.iter().any(|x| x.contains("no local git repo")),
            "{:?}",
            fresh.facts
        );
    }

    #[test]
    fn siggen_scenario_history_and_revised_summary() {
        // old 'current'-tagged entry, newer delivery record, later review note
        let root = proj();
        let r = root.path();
        let mut old = KnowledgeEntry::new(
            0,
            KnowledgeKind::Status,
            "Current state (2026-08-29)".into(),
            "Functional generator; tests empty.".into(),
        );
        old.tags = vec!["current".into()];
        add_entry(r, old).unwrap();
        let delivery = status(r, "Delivery complete", "CLI-first scope delivered");
        let _review_note = status(r, "Review note", "working tree claim at review time");

        // the team designates the delivery record as authoritative
        set_current_summary(r, delivery.id, "agent:rev", Some("d2eec00".into())).unwrap();

        let cs = current_status(r).unwrap().unwrap();
        assert!(cs.designated);
        assert_eq!(cs.entry.title, "Delivery complete");
        // history stays available through search with supersession metadata
        let results = search(r, "generator", Some(KnowledgeKind::Status)).unwrap();
        assert!(!results.is_empty());
        let all = load_entries(r, KnowledgeKind::Status).unwrap();
        assert_eq!(all.len(), 3);
        assert!(
            all[0].tags.contains(&"current".to_string()),
            "legacy tag untouched"
        );
    }
}

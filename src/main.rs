use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, bail};
use clap::{ArgAction, Parser, ValueEnum};
use pdf_oxide::extractors::XmpExtractor;
use pdf_oxide::converters::ConversionOptions;
use pdf_oxide::geometry::Rect;
use pdf_oxide::layout::TextSpan;
use pdf_oxide::{Annotation, AnnotationSubtype, PdfDocument};
use serde_json::json;

#[derive(Parser, Debug)]
#[command(
    name = "cli-pdf-extract",
    version,
    about = "Extract a PDF page as Markdown for LLM ingestion"
)]
struct Cli {
    /// Path(s) to input PDF file(s)
    #[arg(required = true, num_args = 1..)]
    input: Vec<PathBuf>,

    /// Zero-based page index to extract (single-page mode)
    #[arg(short, long, conflicts_with_all = ["start_page", "end_page", "all"])]
    page: Option<usize>,

    /// Zero-based start page (range mode, must be used with --end-page)
    #[arg(long, requires = "end_page", conflicts_with_all = ["page", "all"])]
    start_page: Option<usize>,

    /// Zero-based end page, inclusive (range mode, must be used with --start-page)
    #[arg(long, requires = "start_page", conflicts_with_all = ["page", "all"])]
    end_page: Option<usize>,

    /// Extract the last N pages
    #[arg(long, conflicts_with_all = ["page", "start_page", "end_page", "all", "abstract_only"])]
    last_pages: Option<usize>,

    /// Optional output path; if omitted, writes markdown to stdout
    #[arg(short, long, conflicts_with = "output_dir")]
    output: Option<PathBuf>,

    /// Output directory for batch processing (writes one file per PDF)
    #[arg(long, conflicts_with = "output")]
    output_dir: Option<PathBuf>,

    /// Extract all pages (default when no page/range is provided)
    #[arg(long)]
    all: bool,

    /// Extract a named section (e.g., "conclusion")
    #[arg(
        long,
        conflicts_with_all = [
            "page",
            "start_page",
            "end_page",
            "last_pages",
            "all",
            "highlight",
            "abstract_only",
            "metadata"
        ]
    )]
    section: Option<String>,

    /// Extract document metadata (title, authors, year, page_count)
    #[arg(
        long,
        conflicts_with_all = [
            "page",
            "start_page",
            "end_page",
            "last_pages",
            "all",
            "highlight",
            "abstract_only"
        ]
    )]
    metadata: bool,

    /// Extract highlight annotations and their notes instead of full text
    #[arg(long, conflicts_with = "abstract_only")]
    highlight: bool,

    /// Extract only the abstract (optimized for research-paper triage)
    #[arg(
        long = "abstract",
        alias = "abstract-only",
        conflicts_with_all = ["page", "start_page", "end_page", "all", "highlight"]
    )]
    abstract_only: bool,

    /// Extraction mode for non-highlight text
    #[arg(long, value_enum, default_value_t = ExtractMode::Text)]
    mode: ExtractMode,

    /// Disable spacing normalization for extracted text
    #[arg(long = "no-normalize-spacing", action = ArgAction::SetFalse, default_value_t = true)]
    normalize_spacing: bool,

    /// Number of worker threads for batch processing
    #[arg(long, default_value_t = 1)]
    parallel: usize,

    /// Output format (json currently supported for --metadata mode)
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ExtractMode {
    Auto,
    Markdown,
    Text,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    if args.parallel == 0 {
        bail!("--parallel must be at least 1");
    }

    if args.format == OutputFormat::Json && !args.metadata {
        bail!("--format json is currently supported only with --metadata");
    }

    let consolidated_metadata_json =
        args.input.len() > 1 && args.output.is_some() && args.metadata && args.format == OutputFormat::Json;
    if args.input.len() > 1 && args.output.is_some() && !consolidated_metadata_json {
        bail!("--output supports only a single input; use --output-dir for batch processing");
    }

    if let Some(output_dir) = &args.output_dir {
        std::fs::create_dir_all(output_dir).with_context(|| {
            format!(
                "failed to create output directory: {}",
                output_dir.display()
            )
        })?;
    }

    let settings = RunSettings {
        page: args.page,
        start_page: args.start_page,
        end_page: args.end_page,
        last_pages: args.last_pages,
        all: args.all,
        metadata_only: args.metadata,
        section: args.section.clone(),
        highlight: args.highlight,
        abstract_only: args.abstract_only,
        mode: args.mode,
        normalize_spacing: args.normalize_spacing,
        format: args.format,
    };

    let results: Vec<(PathBuf, anyhow::Result<String>)> = if args.parallel > 1 && args.input.len() > 1 {
        process_inputs_parallel(args.input.clone(), settings.clone(), args.parallel)?
    } else {
        let mut out = Vec::with_capacity(args.input.len());
        for path in &args.input {
            out.push((path.clone(), process_input(path.clone(), &settings)));
        }
        out
    };

    if let Some(output_path) = args.output {
        let total = results.len();
        if settings.metadata_only && settings.format == OutputFormat::Json && total > 1 {
            let mut arr = Vec::new();
            let mut failures = Vec::new();
            for (input, text_result) in results {
                match text_result {
                    Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(v) => arr.push(v),
                        Err(err) => failures.push((
                            input,
                            format!("failed to parse metadata json for consolidation: {err}"),
                        )),
                    },
                    Err(err) => failures.push((input, err.to_string())),
                }
            }
            std::fs::write(
                &output_path,
                serde_json::to_string_pretty(&serde_json::Value::Array(arr))?,
            )
            .with_context(|| format!("failed to write output: {}", output_path.display()))?;

            if !failures.is_empty() {
                for (path, err) in &failures {
                    eprintln!("warning: {}: {}", path.display(), err);
                }
                bail!(
                    "wrote consolidated json with partial failures: {} of {} file(s) failed",
                    failures.len(),
                    total
                );
            }
            return Ok(());
        }

        let text = results[0]
            .1
            .as_ref()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        std::fs::write(&output_path, text)
            .with_context(|| format!("failed to write output: {}", output_path.display()))?;
        return Ok(());
    }

    let mut failures = Vec::new();

    if let Some(output_dir) = args.output_dir {
        let total = results.len();
        for (input, text_result) in results {
            match text_result {
                Ok(text) => {
                    let filename = output_filename_for(&input, &settings);
                    let output_path = output_dir.join(filename);
                    std::fs::write(&output_path, text).with_context(|| {
                        format!("failed to write output: {}", output_path.display())
                    })?;
                }
                Err(err) => failures.push((input, err.to_string())),
            }
        }
        if !failures.is_empty() {
            for (path, err) in &failures {
                eprintln!("warning: {}: {}", path.display(), err);
            }
            bail!(
                "processed with partial failures: {} of {} file(s) failed",
                failures.len(),
                total
            );
        }
        return Ok(());
    }

    let total = results.len();
    for (idx, (input, text_result)) in results.iter().enumerate() {
        if total > 1 {
            if idx > 0 {
                println!();
            }
            println!("=== FILE: {} ===", input.display());
        }
        match text_result {
            Ok(text) => print!("{text}"),
            Err(err) => {
                eprintln!("warning: {}: {}", input.display(), err);
                failures.push((input.clone(), err.to_string()));
            }
        }
    }

    if !failures.is_empty() {
        bail!(
            "processed with partial failures: {} of {} file(s) failed",
            failures.len(),
            total
        );
    }

    Ok(())
}

#[derive(Clone)]
struct RunSettings {
    page: Option<usize>,
    start_page: Option<usize>,
    end_page: Option<usize>,
    last_pages: Option<usize>,
    all: bool,
    metadata_only: bool,
    section: Option<String>,
    highlight: bool,
    abstract_only: bool,
    mode: ExtractMode,
    normalize_spacing: bool,
    format: OutputFormat,
}

fn process_input(input: PathBuf, settings: &RunSettings) -> anyhow::Result<String> {
    let mut doc = PdfDocument::open(&input)
        .with_context(|| format!("failed to open PDF: {}", input.display()))?;

    if settings.metadata_only {
        return extract_metadata(&mut doc, &input, settings.format);
    }

    if let Some(section) = &settings.section {
        return extract_section(&mut doc, section, settings.mode, settings.normalize_spacing);
    }

    if settings.abstract_only {
        return extract_abstract(&mut doc, settings.mode, settings.normalize_spacing);
    }

    if let Some(last) = settings.last_pages {
        if last == 0 {
            bail!("--last-pages must be at least 1");
        }
    }

    let pages: Vec<usize> = match (settings.start_page, settings.end_page, settings.last_pages) {
        (Some(start), Some(end), None) => {
            if start > end {
                bail!(
                    "invalid range: --start-page ({start}) cannot be greater than --end-page ({end})"
                );
            }
            (start..=end).collect()
        }
        (None, None, Some(last)) => {
            let page_count = doc.page_count()?;
            let start = page_count.saturating_sub(last);
            (start..page_count).collect()
        }
        (None, None, None) => {
            if let Some(page) = settings.page {
                vec![page]
            } else {
                let page_count = doc.page_count()?;
                (0..page_count).collect()
            }
        }
        _ => bail!("both --start-page and --end-page must be provided together"),
    };

    if settings.all
        && settings.page.is_none()
        && settings.start_page.is_none()
        && settings.end_page.is_none()
    {
        // --all is the explicit version of the default behavior.
    }

    let mut markdown = String::new();
    for page in pages {
        let page_text = if settings.highlight {
            extract_highlights(&mut doc, page)
                .with_context(|| format!("failed to extract highlights for page {page}"))?
        } else {
            extract_page_text(&mut doc, page, settings.mode, settings.normalize_spacing)
                .with_context(|| format!("failed to extract page {page}"))?
        };

        if page_text.trim().is_empty() {
            continue;
        }

        if !markdown.is_empty() {
            markdown.push_str("\n\n");
        }
        markdown.push_str(&format!("--- PAGE {page} ---\n\n"));
        markdown.push_str(&page_text);
    }

    Ok(markdown)
}

fn output_filename_for(input: &std::path::Path, settings: &RunSettings) -> String {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    if settings.highlight {
        return format!("{stem}_highlights.txt");
    }
    if settings.abstract_only {
        return format!("{stem}_abstract.txt");
    }
    if settings.metadata_only {
        return match settings.format {
            OutputFormat::Text => format!("{stem}_metadata.txt"),
            OutputFormat::Json => format!("{stem}_metadata.json"),
        };
    }
    if let Some(section) = &settings.section {
        let name = section
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        return format!("{stem}_{name}.txt");
    }
    format!("{stem}.md")
}

fn extract_metadata(
    doc: &mut PdfDocument,
    input: &std::path::Path,
    format: OutputFormat,
) -> anyhow::Result<String> {
    let page_count = doc.page_count()?;
    let xmp = XmpExtractor::extract(doc).ok().flatten();

    let mut title = xmp.as_ref().and_then(|m| m.dc_title.clone());
    let mut authors = xmp
        .as_ref()
        .map(|m| normalize_authors(&m.dc_creator))
        .unwrap_or_default();
    let mut year = xmp
        .as_ref()
        .and_then(|m| {
            m.xmp_create_date
                .as_deref()
                .or(m.xmp_modify_date.as_deref())
                .and_then(extract_year)
        });

    // Fallback title: first non-empty heading/text line from page 0.
    if title.is_none() {
        if let Ok(first_page) = doc.extract_text(0) {
            title = first_page
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && l.len() > 8)
                .map(str::to_string);
        }
    }

    if let Ok(first_page) = doc.extract_text(0) {
        if authors.is_empty() {
            authors = extract_authors_from_first_page(&first_page);
        }
        // Fallback year from first page text.
        if year.is_none() {
            year = extract_year(&first_page);
        }
    }

    if authors.is_empty() {
        authors.push("unknown".to_string());
    }

    let filename = input
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown.pdf");

    let title = title.unwrap_or_else(|| "unknown".to_string());
    let year = year.unwrap_or_else(|| "unknown".to_string());

    match format {
        OutputFormat::Text => Ok(format!(
            "file: {filename}\ntitle: {title}\nauthors: {}\nyear: {year}\npage_count: {page_count}\n",
            authors.join(", ")
        )),
        OutputFormat::Json => Ok(
            json!({
                "file": filename,
                "title": title,
                "authors": authors,
                "year": year,
                "page_count": page_count
            })
            .to_string(),
        ),
    }
}

fn extract_year(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    for i in 0..=bytes.len() - 4 {
        let window = &bytes[i..i + 4];
        if window.iter().all(|b| b.is_ascii_digit()) {
            let year = std::str::from_utf8(window).ok()?;
            if (1900..=2100).contains(&year.parse::<u32>().ok()?) {
                return Some(year.to_string());
            }
        }
    }
    None
}

fn extract_section(
    doc: &mut PdfDocument,
    section: &str,
    mode: ExtractMode,
    normalize_spacing: bool,
) -> anyhow::Result<String> {
    let targets = section_targets(section);
    if let Some(found) = scan_section(doc, &targets, mode, normalize_spacing, false)? {
        return Ok(found);
    }

    // Relaxed fallback for conclusion-like sections: search for lines containing
    // the keyword even when heading formatting is noisy.
    if targets.iter().any(|t| t.contains("conclusion")) {
        if let Some(found) = scan_section(doc, &targets, mode, normalize_spacing, true)? {
            return Ok(found);
        }
    }

    if let Some(found) = scan_section_full_text(doc, &targets, mode, normalize_spacing)? {
        return Ok(found);
    }

    bail!("could not locate section: {section} (try --section discussion)");
}

fn scan_section(
    doc: &mut PdfDocument,
    targets: &[String],
    mode: ExtractMode,
    normalize_spacing: bool,
    relaxed: bool,
) -> anyhow::Result<Option<String>> {
    let page_count = doc.page_count()?;
    let section_mode = if mode == ExtractMode::Text {
        ExtractMode::Markdown
    } else {
        mode
    };
    let mut collecting = false;
    let mut collected = String::new();

    for page in 0..page_count {
        let page_text = extract_page_text(doc, page, section_mode, normalize_spacing)?;
        for line in page_text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if collecting {
                    collected.push('\n');
                }
                continue;
            }

            let marker = normalize_marker(trimmed);
            if !collecting {
                let match_found = if relaxed {
                    is_heading_like(trimmed) && contains_section_keyword(trimmed, &marker, targets)
                } else {
                    is_section_heading_match(&marker, targets)
                };
                if match_found {
                    collecting = true;
                    continue;
                }
            } else if is_heading_like(trimmed) && !is_section_heading_match(&marker, targets) {
                if marker.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                if !collected.trim().is_empty() {
                    if collected.trim().len() > 120 {
                        return Ok(Some(collected.trim().to_string()));
                    }
                    collecting = false;
                    collected.clear();
                }
            }

            if collecting {
                collected.push_str(trimmed);
                collected.push('\n');
            }
        }
    }

    if collected.trim().is_empty() || collected.trim().len() <= 120 {
        return Ok(None);
    }
    Ok(Some(collected.trim().to_string()))
}

fn section_targets(section: &str) -> Vec<String> {
    let target = normalize_marker(section);
    if target == "conclusion" || target == "conclusions" {
        return vec![
            "conclusion".to_string(),
            "conclusions".to_string(),
            "conclusionandfuturework".to_string(),
            "conclusionsandfuturework".to_string(),
            "discussionandconclusion".to_string(),
            "summaryandconclusion".to_string(),
            "concludingremarks".to_string(),
            "finalremarks".to_string(),
            "closingremarks".to_string(),
            "discussion".to_string(),
            "limitations".to_string(),
            "limitationsandtradeoffs".to_string(),
            "tradeoffs".to_string(),
        ];
    }
    vec![target]
}

fn is_section_heading_match(marker: &str, targets: &[String]) -> bool {
    if targets.iter().any(|t| marker == t) {
        return true;
    }
    for target in targets {
        if marker.ends_with(target) {
            let prefix = &marker[..marker.len().saturating_sub(target.len())];
            if prefix.is_empty()
                || prefix.chars().all(|c| c.is_ascii_digit())
                || prefix == "section"
                || prefix == "appendix"
            {
                return true;
            }
        }
    }
    false
}

fn is_heading_like(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return true;
    }
    if trimmed.len() > 120 {
        return false;
    }
    let words = trimmed.split_whitespace().count();
    if words > 14 {
        return false;
    }
    let norm = normalize_marker(trimmed);
    if norm.is_empty() {
        return false;
    }
    let has_digits_prefix = norm
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .count()
        > 0;
    let all_caps_words = trimmed
        .split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_ascii_alphabetic()))
        .all(|w| w.chars().all(|c| !c.is_ascii_lowercase()));
    has_digits_prefix || all_caps_words
}

fn contains_section_keyword(line: &str, marker: &str, targets: &[String]) -> bool {
    let line_words = line.split_whitespace().count();
    if line_words > 18 {
        return false;
    }
    targets.iter().any(|target| {
        marker.contains(target)
            || line
                .to_ascii_lowercase()
                .split_whitespace()
                .any(|w| w.contains(target))
    })
}

fn scan_section_full_text(
    doc: &mut PdfDocument,
    targets: &[String],
    mode: ExtractMode,
    normalize_spacing: bool,
) -> anyhow::Result<Option<String>> {
    let page_count = doc.page_count()?;
    let section_mode = if mode == ExtractMode::Text {
        ExtractMode::Markdown
    } else {
        mode
    };

    let mut lines = Vec::new();
    for page in 0..page_count {
        let page_text = extract_page_text(doc, page, section_mode, normalize_spacing)?;
        lines.extend(page_text.lines().map(str::to_string));
    }

    let mut start = None;
    for (idx, line) in lines.iter().enumerate() {
        let marker = normalize_marker(line);
        if is_section_heading_match(&marker, targets)
            || (is_heading_like(line) && contains_section_keyword(line, &marker, targets))
        {
            start = Some(idx + 1);
            break;
        }
    }
    let Some(start_idx) = start else {
        return Ok(None);
    };

    let mut out = String::new();
    for line in lines.iter().skip(start_idx) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            continue;
        }

        let marker = normalize_marker(trimmed);
        if is_heading_like(trimmed) && !is_section_heading_match(&marker, targets) {
            if marker.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            break;
        }
        out.push_str(trimmed);
        out.push('\n');
    }

    if out.trim().len() <= 120 {
        return Ok(None);
    }
    Ok(Some(out.trim().to_string()))
}

fn normalize_authors(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for entry in raw {
        let cleaned = entry.replace(['\n', '\r'], " ");
        for part in cleaned
            .split(&[',', ';'][..])
            .flat_map(|p| p.split(" and "))
            .map(str::trim)
        {
            let name = sanitize_author_name(part);
            if !name.is_empty() && looks_like_person_name(&name) {
                out.push(name);
            }
        }
    }
    out.dedup();
    out
}

fn extract_authors_from_first_page(page_text: &str) -> Vec<String> {
    let mut lines: Vec<&str> = page_text.lines().map(str::trim).collect();
    lines.retain(|l| !l.is_empty());

    let abstract_idx = lines
        .iter()
        .position(|l| normalize_marker(l).starts_with("abstract"))
        .unwrap_or(lines.len().min(30));
    let search_space = &lines[..abstract_idx.min(lines.len())];

    // Prefer lines with commas/and likely listing multiple names.
    for line in search_space.iter().rev() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("university")
            || lower.contains("department")
            || lower.contains("institute")
            || lower.contains("arxiv")
            || lower.contains('@')
        {
            continue;
        }
        if !(line.contains(',') || lower.contains(" and ")) {
            continue;
        }

        let candidates = line
            .split(&[',', ';'][..])
            .flat_map(|p| p.split(" and "))
            .map(str::trim)
            .map(sanitize_author_name)
            .filter(|s| !s.is_empty() && looks_like_person_name(s))
            .collect::<Vec<_>>();
        if !candidates.is_empty() {
            return dedup_vec(candidates);
        }
    }

    Vec::new()
}

fn sanitize_author_name(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphabetic() || *c == ' ' || *c == '-' || *c == '\'')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_like_person_name(s: &str) -> bool {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 2 || parts.len() > 5 {
        return false;
    }
    parts
        .iter()
        .all(|p| p.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
}

fn dedup_vec(v: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for item in v {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

fn process_inputs_parallel(
    inputs: Vec<PathBuf>,
    settings: RunSettings,
    workers: usize,
) -> anyhow::Result<Vec<(PathBuf, anyhow::Result<String>)>> {
    let queue = Arc::new(Mutex::new(
        inputs
            .into_iter()
            .enumerate()
            .collect::<std::collections::VecDeque<(usize, PathBuf)>>(),
    ));
    let queue_len = queue.lock().expect("queue mutex poisoned").len();
    let mut slots = Vec::with_capacity(queue_len);
    slots.resize_with(queue_len, || None);
    let results = Arc::new(Mutex::new(slots));
    let mut handles = Vec::new();

    for _ in 0..workers {
        let queue = Arc::clone(&queue);
        let results = Arc::clone(&results);
        let settings = settings.clone();
        handles.push(thread::spawn(move || {
            loop {
                let next = {
                    let mut q = queue.lock().expect("queue mutex poisoned");
                    q.pop_front()
                };
                let Some((idx, path)) = next else {
                    break;
                };
                let result = process_input(path.clone(), &settings);
                let mut out = results.lock().expect("results mutex poisoned");
                out[idx] = Some((path, result));
            }
        }));
    }

    for handle in handles {
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("worker thread panicked"))?;
    }

    let mut ordered = Vec::new();
    let mut locked = results.lock().expect("results mutex poisoned");
    for slot in locked.drain(..) {
        let Some(res) = slot else {
            bail!("internal error: missing batch result");
        };
        ordered.push(res);
    }
    Ok(ordered)
}

fn extract_abstract(
    doc: &mut PdfDocument,
    mode: ExtractMode,
    normalize_spacing: bool,
) -> anyhow::Result<String> {
    let page_count = doc.page_count()?;
    let scan_pages = page_count.min(3);
    let mut combined = String::new();

    for page in 0..scan_pages {
        let page_text = extract_page_text(doc, page, mode, normalize_spacing)?;
        if page_text.trim().is_empty() {
            continue;
        }
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        combined.push_str(&page_text);
    }

    if combined.trim().is_empty() {
        return Ok(String::new());
    }

    if let Some(abstract_block) = find_abstract_block(&combined) {
        return Ok(abstract_block);
    }

    bail!("could not locate abstract in first {scan_pages} page(s)")
}

fn find_abstract_block(text: &str) -> Option<String> {
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for line in text.lines() {
        lines.push((offset, line));
        offset += line.len() + 1;
    }

    let mut start = None;
    for (line_off, line) in &lines {
        let normalized = normalize_marker(line);
        if !normalized.starts_with("abstract") {
            continue;
        }

        let lower = line.to_lowercase();
        if let Some(pos) = lower.find("abstract") {
            // Handles "Abstract—..." inline content.
            start = Some(*line_off + pos + "abstract".len());
        } else {
            // Handles split heading styles like "A BSTRACT".
            start = Some(*line_off + line.len() + 1);
        }
        break;
    }
    let start = start?;

    let end_markers = [
        "indexterms",
        "keywords",
        "contents",
        "tableofcontents",
        "1introduction",
        "iintroduction",
        "1purposeofthisdocument",
    ];
    let mut end_abs = text.len();
    for (line_off, line) in &lines {
        if *line_off <= start {
            continue;
        }
        let normalized = normalize_marker(line);
        if end_markers.iter().any(|m| normalized.starts_with(m)) {
            end_abs = *line_off;
            break;
        }
    }

    let mut abstract_text = text[start..end_abs].trim().to_string();

    // Fallback guard: if no clear end marker is found, cap abstract length.
    if end_abs == text.len() {
        let soft_cap = abstract_text
            .char_indices()
            .nth(2800)
            .map(|(i, _)| i)
            .unwrap_or(abstract_text.len());
        abstract_text.truncate(soft_cap);

        // Prefer stopping at a paragraph break near the cap.
        if let Some(par_break) = abstract_text.rfind("\n\n") {
            if par_break > 500 {
                abstract_text.truncate(par_break);
            }
        }
    }

    // Secondary guard against accidental full-document extraction.
    if abstract_text.len() > 3200 {
        abstract_text = abstract_text.chars().take(3200).collect::<String>();
        if let Some(last_period) = abstract_text.rfind('.') {
            if last_period > 400 {
                abstract_text.truncate(last_period + 1);
            }
        }
    }
    if abstract_text.is_empty() {
        None
    } else {
        Some(abstract_text)
    }
}

fn normalize_marker(line: &str) -> String {
    line.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn extract_highlights(doc: &mut PdfDocument, page: usize) -> anyhow::Result<String> {
    let annotations = doc.get_annotations(page)?;
    if annotations.is_empty() {
        return Ok(String::new());
    }

    let spans = doc.extract_spans(page)?;
    let mut output = String::new();

    for annot in annotations {
        if annot.subtype_enum != AnnotationSubtype::Highlight {
            continue;
        }

        let rects = highlight_rects(&annot);
        if rects.is_empty() {
            continue;
        }

        let highlighted_text = collect_highlighted_text(&spans, &rects);
        let note = annot.contents.unwrap_or_default();

        if highlighted_text.trim().is_empty() && note.trim().is_empty() {
            continue;
        }

        if !output.is_empty() {
            output.push_str("\n\n");
        }

        output.push_str("HIGHLIGHT: ");
        output.push_str(highlighted_text.trim());

        if !note.trim().is_empty() {
            output.push('\n');
            output.push_str("NOTE: ");
            output.push_str(note.trim());
        }
    }

    Ok(output)
}

fn extract_markdown_without_images(doc: &mut PdfDocument, page: usize) -> anyhow::Result<String> {
    let options = ConversionOptions {
        include_images: false,
        ..Default::default()
    };
    let markdown = doc.to_markdown(page, &options)?;
    Ok(normalize_markdown(&rewrite_image_lines(&markdown), page))
}

fn extract_page_text(
    doc: &mut PdfDocument,
    page: usize,
    mode: ExtractMode,
    normalize_spacing: bool,
) -> anyhow::Result<String> {
    let apply_post = |text: String| {
        if normalize_spacing {
            normalize_spacing_text(&text)
        } else {
            text
        }
    };

    match mode {
        ExtractMode::Markdown => Ok(apply_post(extract_markdown_without_images(doc, page)?)),
        ExtractMode::Text => Ok(apply_post(doc.extract_text(page)?)),
        ExtractMode::Auto => {
            let md_text = apply_post(extract_markdown_without_images(doc, page)?);
            let md_score = text_quality_score(&md_text);
            if md_score >= 0.82 {
                return Ok(md_text);
            }

            let text_text = apply_post(doc.extract_text(page)?);
            let text_score = text_quality_score(&text_text);
            if text_score >= md_score {
                Ok(text_text)
            } else {
                Ok(md_text)
            }
        }
    }
}

fn rewrite_image_lines(markdown: &str) -> String {
    markdown
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with("![") {
                return Some(line.to_string());
            }

            let (Some(alt_start), Some(alt_end)) = (trimmed.find('['), trimmed.find(']')) else {
                return None;
            };
            let alt = &trimmed[(alt_start + 1)..alt_end];
            if alt.trim().is_empty() {
                return None;
            }

            Some(format!("Figure: {}", alt.trim()))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_spacing_text(input: &str) -> String {
    let normalized_unicode = input.replace('Æ', "fl").replace('æ', "ae");
    let mut out = String::with_capacity(normalized_unicode.len() + 64);
    let chars: Vec<char> = normalized_unicode.chars().collect();
    for (idx, ch) in chars.iter().enumerate() {
        if idx > 0 {
            let p = chars[idx - 1];
            let next = chars.get(idx + 1).copied();
            let need_space = p.is_ascii_lowercase()
                && ch.is_ascii_uppercase()
                && next.map(|n| n.is_ascii_lowercase()).unwrap_or(false);
            if need_space {
                out.push(' ');
            }
        }
        out.push(*ch);
    }

    out.split('\n')
        .map(normalize_glued_words_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_glued_words_line(line: &str) -> String {
    line.split_whitespace()
        .map(split_long_glued_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_long_glued_token(token: &str) -> String {
    if token.len() < 22 || token.contains('@') || token.contains("://") {
        return token.to_string();
    }

    let lower = token.to_ascii_lowercase();
    if !lower.chars().all(|c| c.is_ascii_alphabetic()) {
        return token.to_string();
    }

    let boundaries = [
        "the",
        "and",
        "for",
        "with",
        "that",
        "this",
        "from",
        "into",
        "while",
        "where",
        "when",
        "then",
        "most",
        "only",
        "used",
        "using",
        "user",
        "model",
        "models",
        "repair",
        "report",
        "solution",
        "uncertainty",
        "regions",
    ];
    let mut split_at = Vec::new();
    for marker in boundaries {
        let mut start = 0usize;
        while let Some(found) = lower[start..].find(marker) {
            let idx = start + found;
            if idx >= 5 && idx <= lower.len().saturating_sub(5) {
                split_at.push(idx);
            }
            start = idx + marker.len();
        }
    }
    split_at.sort_unstable();
    split_at.dedup();
    if split_at.is_empty() {
        return token.to_string();
    }

    let mut rebuilt = String::new();
    let mut last = 0usize;
    for idx in split_at {
        if idx.saturating_sub(last) < 4 {
            continue;
        }
        rebuilt.push_str(&token[last..idx]);
        rebuilt.push(' ');
        last = idx;
    }
    rebuilt.push_str(&token[last..]);
    rebuilt
}

fn text_quality_score(text: &str) -> f64 {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() {
        return 0.0;
    }

    let alpha_tokens: Vec<&str> = tokens
        .iter()
        .copied()
        .filter(|t| t.chars().all(|c| c.is_ascii_alphabetic()))
        .collect();
    let long_alpha = alpha_tokens.iter().filter(|t| t.len() > 20).count();
    let suspicious_joined = alpha_tokens
        .iter()
        .filter(|t| t.len() > 16 && looks_glued_word(t))
        .count();
    let marker_noise = text.matches("****").count() + text.matches("∗∗").count();

    let long_ratio = long_alpha as f64 / tokens.len() as f64;
    let glued_ratio = suspicious_joined as f64 / tokens.len() as f64;

    let heading_bonus = if text.lines().any(|l| l.trim_start().starts_with("# ")) {
        0.07
    } else {
        0.0
    };

    let mut score = 1.0;
    score -= (long_ratio * 2.2).min(0.5);
    score -= (glued_ratio * 2.5).min(0.5);
    score -= (marker_noise as f64 * 0.03).min(0.3);
    (score + heading_bonus).clamp(0.0, 1.0)
}

fn looks_glued_word(token: &str) -> bool {
    let t = token.to_ascii_lowercase();
    let markers = [
        "the", "and", "for", "with", "that", "this", "from", "into", "while", "where", "when",
        "then", "most", "only",
    ];
    let mut hits = 0usize;
    for marker in markers {
        if let Some(idx) = t.find(marker) {
            if idx > 1 && idx + marker.len() < t.len().saturating_sub(1) {
                hits += 1;
            }
        }
    }
    hits >= 2
}

fn normalize_markdown(markdown: &str, page: usize) -> String {
    let mut out = Vec::new();
    let lines: Vec<&str> = markdown.lines().collect();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // Drop empty heading markers like "##" / "###" which are PDF layout artifacts.
        if is_empty_heading_marker(trimmed) {
            i += 1;
            continue;
        }

        if let Some((level, text)) = parse_heading(trimmed) {
            if text.chars().all(|c| c.is_ascii_digit()) {
                i += 1;
                continue;
            }
            let mut parts = vec![text.to_string()];
            let mut j = i + 1;

            while j < lines.len() {
                let next = lines[j].trim();
                if next.is_empty() {
                    j += 1;
                    continue;
                }
                if let Some((next_level, next_text)) = parse_heading(next) {
                    if next_level == level && is_heading_fragment(next_text) {
                        parts.push(next_text.to_string());
                        j += 1;
                        continue;
                    }
                }
                break;
            }

            if parts.len() > 1 && parts.iter().all(|p| is_heading_fragment(p)) {
                out.push(format!("{} {}", "#".repeat(level), parts.join(" ")));
                i = j;
                continue;
            }
        }

        if let Some(section) = normalize_broken_section_heading(line) {
            out.push(section);
        } else {
            out.push(line.to_string());
        }
        i += 1;
    }

    let mut normalized = out.join("\n");
    if page == 0 {
        normalized = combine_title_fragments(&normalized);
    }
    normalized
}

fn is_empty_heading_marker(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    line.chars().all(|c| c == '#')
}

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    if level == 0 {
        return None;
    }
    let rest = line[level..].trim();
    if rest.is_empty() {
        return None;
    }
    Some((level, rest))
}

fn is_heading_fragment(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.len() > 30 {
        return false;
    }
    // Heuristic: fragments are short, typically 1-5 words.
    let words = t.split_whitespace().count();
    words > 0 && words <= 5
}

fn normalize_broken_section_heading(line: &str) -> Option<String> {
    let unstarred = line.replace('*', "");
    let compact = unstarred.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return None;
    }

    let mut parts = compact.splitn(2, ' ');
    let first = parts.next()?;
    let rest = parts.next()?.trim();
    if !first.chars().all(|c| c.is_ascii_digit()) || rest.is_empty() {
        return None;
    }

    let uppercase_like = rest
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_whitespace() || c == '-' || c == '&');
    if !uppercase_like {
        return None;
    }

    Some(format!("## {} {}", first, rest))
}

fn combine_title_fragments(markdown: &str) -> String {
    let mut lines: Vec<String> = markdown.lines().map(ToString::to_string).collect();
    let mut heading_indices = Vec::new();
    let mut heading_parts = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some((level, text)) = parse_heading(trimmed) {
            if level != 1 {
                continue;
            }
            let clean = text.trim();
            if clean.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if is_heading_fragment(clean) || clean.split_whitespace().count() <= 8 {
                heading_indices.push(idx);
                heading_parts.push(clean.to_string());
            }
        }
    }

    if heading_indices.len() < 2 {
        return markdown.to_string();
    }

    let merged = heading_parts.join(" ");
    let first = heading_indices[0];
    lines[first] = format!("# {merged}");
    for idx in heading_indices.iter().skip(1).rev() {
        lines.remove(*idx);
    }
    lines.join("\n")
}

fn highlight_rects(annot: &Annotation) -> Vec<Rect> {
    if let Some(quads) = &annot.quad_points {
        let mut rects = Vec::with_capacity(quads.len());
        for quad in quads {
            let xs = [quad[0], quad[2], quad[4], quad[6]];
            let ys = [quad[1], quad[3], quad[5], quad[7]];
            let (min_x, max_x) = min_max(&xs);
            let (min_y, max_y) = min_max(&ys);
            rects.push(Rect::new(
                min_x as f32,
                min_y as f32,
                (max_x - min_x) as f32,
                (max_y - min_y) as f32,
            ));
        }
        return rects;
    }

    if let Some(rect) = annot.rect {
        let xs = [rect[0], rect[2]];
        let ys = [rect[1], rect[3]];
        let (min_x, max_x) = min_max(&xs);
        let (min_y, max_y) = min_max(&ys);
        return vec![Rect::new(
            min_x as f32,
            min_y as f32,
            (max_x - min_x) as f32,
            (max_y - min_y) as f32,
        )];
    }

    Vec::new()
}

fn min_max(values: &[f64]) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for value in values {
        if *value < min {
            min = *value;
        }
        if *value > max {
            max = *value;
        }
    }
    (min, max)
}

fn collect_highlighted_text(spans: &[TextSpan], rects: &[Rect]) -> String {
    let mut out = String::new();
    for span in spans {
        if rects.iter().any(|rect| rect.intersects(&span.bbox)) {
            push_with_spacing(&mut out, &span.text);
        }
    }
    out
}

fn push_with_spacing(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }

    let need_space = out
        .chars()
        .last()
        .map(|c| !c.is_whitespace())
        .unwrap_or(false)
        && text
            .chars()
            .next()
            .map(|c| !c.is_whitespace())
            .unwrap_or(false);

    if need_space {
        out.push(' ');
    }
    out.push_str(text);
}

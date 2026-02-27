use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{ArgAction, Parser, ValueEnum};
use pdf_oxide::converters::ConversionOptions;
use pdf_oxide::geometry::Rect;
use pdf_oxide::layout::TextSpan;
use pdf_oxide::{Annotation, AnnotationSubtype, PdfDocument};

#[derive(Parser, Debug)]
#[command(
    name = "cli-pdf-extract",
    version,
    about = "Extract a PDF page as Markdown for LLM ingestion"
)]
struct Cli {
    /// Path to the input PDF file
    input: PathBuf,

    /// Zero-based page index to extract (single-page mode)
    #[arg(short, long, conflicts_with_all = ["start_page", "end_page", "all"])]
    page: Option<usize>,

    /// Zero-based start page (range mode, must be used with --end-page)
    #[arg(long, requires = "end_page", conflicts_with_all = ["page", "all"])]
    start_page: Option<usize>,

    /// Zero-based end page, inclusive (range mode, must be used with --start-page)
    #[arg(long, requires = "start_page", conflicts_with_all = ["page", "all"])]
    end_page: Option<usize>,

    /// Optional output path; if omitted, writes markdown to stdout
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Extract all pages (default when no page/range is provided)
    #[arg(long)]
    all: bool,

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
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ExtractMode {
    Auto,
    Markdown,
    Text,
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    let mut doc = PdfDocument::open(&args.input)
        .with_context(|| format!("failed to open PDF: {}", args.input.display()))?;

    if args.abstract_only {
        let abstract_text = extract_abstract(&mut doc, args.mode, args.normalize_spacing)?;
        if let Some(output_path) = args.output {
            std::fs::write(&output_path, abstract_text)
                .with_context(|| format!("failed to write output: {}", output_path.display()))?;
        } else {
            print!("{abstract_text}");
        }
        return Ok(());
    }

    let pages: Vec<usize> = match (args.start_page, args.end_page) {
        (Some(start), Some(end)) => {
            if start > end {
                bail!(
                    "invalid range: --start-page ({start}) cannot be greater than --end-page ({end})"
                );
            }
            (start..=end).collect()
        }
        (None, None) => {
            if let Some(page) = args.page {
                vec![page]
            } else {
                let page_count = doc.page_count()?;
                (0..page_count).collect()
            }
        }
        _ => bail!("both --start-page and --end-page must be provided together"),
    };

    if args.all && args.page.is_none() && args.start_page.is_none() && args.end_page.is_none() {
        // --all is the explicit version of the default behavior.
    }

    let mut markdown = String::new();
    for page in pages {
        let page_text = if args.highlight {
            extract_highlights(&mut doc, page)
                .with_context(|| format!("failed to extract highlights for page {page}"))?
        } else {
            extract_page_text(&mut doc, page, args.mode, args.normalize_spacing)
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

    if let Some(output_path) = args.output {
        std::fs::write(&output_path, markdown)
            .with_context(|| format!("failed to write output: {}", output_path.display()))?;
    } else {
        print!("{markdown}");
    }

    Ok(())
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

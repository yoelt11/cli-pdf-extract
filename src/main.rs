use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Parser;
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
    #[arg(long)]
    highlight: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    let mut doc = PdfDocument::open(&args.input)
        .with_context(|| format!("failed to open PDF: {}", args.input.display()))?;

    let pages: Vec<usize> = match (args.start_page, args.end_page) {
        (Some(start), Some(end)) => {
            if start > end {
                bail!("invalid range: --start-page ({start}) cannot be greater than --end-page ({end})");
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
            doc.to_markdown(page, &Default::default())
                .with_context(|| format!("failed to extract page {page} as markdown"))?
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
        && text.chars().next().map(|c| !c.is_whitespace()).unwrap_or(false);

    if need_space {
        out.push(' ');
    }
    out.push_str(text);
}

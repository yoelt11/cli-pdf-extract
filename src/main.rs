use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Parser;
use pdf_oxide::PdfDocument;

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
    #[arg(short, long, conflicts_with_all = ["start_page", "end_page"])]
    page: Option<usize>,

    /// Zero-based start page (range mode, must be used with --end-page)
    #[arg(long, requires = "end_page", conflicts_with = "page")]
    start_page: Option<usize>,

    /// Zero-based end page, inclusive (range mode, must be used with --start-page)
    #[arg(long, requires = "start_page", conflicts_with = "page")]
    end_page: Option<usize>,

    /// Optional output path; if omitted, writes markdown to stdout
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    let mut doc = PdfDocument::open(&args.input)
        .with_context(|| format!("failed to open PDF: {}", args.input.display()))?;

    let markdown = match (args.start_page, args.end_page) {
        (Some(start), Some(end)) => {
            if start > end {
                bail!("invalid range: --start-page ({start}) cannot be greater than --end-page ({end})");
            }

            let mut joined = String::new();
            for page in start..=end {
                let page_markdown = doc
                    .to_markdown(page, &Default::default())
                    .with_context(|| format!("failed to extract page {page} as markdown"))?;

                if !joined.is_empty() {
                    joined.push_str("\n\n");
                }
                joined.push_str(&format!("--- PAGE {page} ---\n\n"));
                joined.push_str(&page_markdown);
            }
            joined
        }
        (None, None) => {
            let page = args.page.unwrap_or(0);
            doc.to_markdown(page, &Default::default())
                .with_context(|| format!("failed to extract page {page} as markdown"))?
        }
        _ => bail!("both --start-page and --end-page must be provided together"),
    };

    if let Some(output_path) = args.output {
        std::fs::write(&output_path, markdown)
            .with_context(|| format!("failed to write output: {}", output_path.display()))?;
    } else {
        print!("{markdown}");
    }

    Ok(())
}

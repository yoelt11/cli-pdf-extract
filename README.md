# cli-pdf-extract

A fast Rust CLI wrapper around `pdf_oxide` that lets LLMs peek into PDFs without paying the cost of loading and synthesizing whole documents. It extracts page text as Markdown for quick ingestion and can also pull only highlights (plus notes), which is ideal when you want the essence without the full context and need higher LLM throughput.

## Features

- Extract pages as Markdown (single page, range, or all pages by default)
- Extract only highlight annotations and their notes (`--highlight`)
- Write to stdout (for piping) or a file (`--output`)
- Designed for low-latency LLM workflows (fast “peek” into large PDFs)

## Prerequisites

- Rust and Cargo installed (`rustup` recommended)

## Installation

### Option 1: Build and run locally

```bash
cargo run -- <PDF_PATH> --page 0
```

### Option 2: Install as a local CLI binary

From the repository root:

```bash
cargo install --path .
```

Then run:

```bash
cli-pdf-extract <PDF_PATH> --page 0
```

## Usage

### Show help

```bash
cli-pdf-extract --help
```

### Single page extraction

```bash
cli-pdf-extract examples/main.pdf --page 0 --output extract.md
```

### Page range extraction (inclusive)

```bash
cli-pdf-extract examples/main.pdf --start-page 0 --end-page 5 --output extract.md
```

### Extract highlights only (fastest LLM pass)

```bash
cli-pdf-extract examples/main.pdf --highlight
```

### Pipe directly to another command / LLM tool

```bash
cli-pdf-extract examples/main.pdf --start-page 0 --end-page 5 | cat
```

## Notes

- Pages are zero-indexed.
- `--start-page` and `--end-page` must be provided together.
- `--page` cannot be combined with range flags.
- Pro-tip: add standardized tags to annotation notes (e.g., `<problem-simulations>`, `<paper-idea>`) to enable downstream clustering, trend discovery, and routing.

## License

MIT. See [LICENSE](LICENSE).

## Author

Edgar Torres (edgar.torres@ki.uni-stuttgart.de)

# cli-pdf-extract

`cli-pdf-extract` is a fast Rust CLI for LLM-friendly PDF inspection. It wraps `pdf_oxide` and focuses on practical research workflows: quick triage, targeted extraction, and lightweight downstream parsing.

## Why This Tool

LLMs are often slow when they must open and synthesize full PDFs. This CLI gives a faster path:

- Extract only what you need (`--abstract`, `--highlight`, page/range)
- Avoid heavy image payloads by default
- Prefer plain text output for fast agent throughput (default `--mode text`)

## Modalities

- `full text/pages`: extract one page, a range, or all pages (default if no page flags)
- `abstract`: extract only the abstract block for paper triage
- `highlight`: extract only PDF highlights and their notes

## Extraction Modes

For non-highlight extraction, choose with `--mode`:

- `text` (default): plain text, usually fastest for agents
- `markdown`: preserves heading/list structure when available
- `auto`: tries markdown first, then falls back to text if quality looks poor

Spacing normalization is enabled by default to reduce merged-word artifacts. Disable with `--no-normalize-spacing`.

## Install

### Local build/run

```bash
cargo run -- <PDF_PATH> --page 0
```

### Install binary locally

```bash
cargo install --path .
```

Then:

```bash
cli-pdf-extract <PDF_PATH> --page 0
```

## Usage

### Help

```bash
cli-pdf-extract --help
```

### Single page

```bash
cli-pdf-extract examples/main.pdf --page 0
```

### Page range (inclusive)

```bash
cli-pdf-extract examples/main.pdf --start-page 0 --end-page 5
```

### All pages (default behavior)

```bash
cli-pdf-extract examples/main.pdf
```

### Abstract-only

```bash
cli-pdf-extract examples/main.pdf --abstract
```

### Highlights + notes only

```bash
cli-pdf-extract examples/main.pdf --highlight
```

### Force markdown mode

```bash
cli-pdf-extract examples/main.pdf --mode markdown
```

### Auto fallback mode

```bash
cli-pdf-extract examples/main.pdf --mode auto
```

### Write to file

```bash
cli-pdf-extract examples/main.pdf --abstract --output abstract.txt
```

## Recommended Presets

### Paper triage (fast)

```bash
cli-pdf-extract paper.pdf --abstract
```

### Quick skim with structure

```bash
cli-pdf-extract paper.pdf --mode markdown --start-page 0 --end-page 2
```

### Robust default for noisy PDFs

```bash
cli-pdf-extract paper.pdf --mode auto --start-page 0 --end-page 5
```

### Annotation mining workflow

```bash
cli-pdf-extract paper.pdf --highlight
```

## Notes

- Page indices are zero-based.
- `--start-page` and `--end-page` must be used together.
- `--abstract` cannot be combined with page/range/all or `--highlight`.
- Pro-tip: add standardized tags to annotation notes (e.g., `<paper-idea>`, `<method>`, `<limitation>`) for downstream clustering and trend discovery.

## License

MIT. See `LICENSE`.

## Author

Edgar Torres (edgar.torres@ki.uni-stuttgart.de)

# cli-pdf-extract

A small Rust CLI to extract PDF pages as Markdown for LLM workflows.

## Features

- Extract a single page to Markdown
- Extract a page range (`--start-page` to `--end-page`)
- Write to stdout (for piping) or a file (`--output`)

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

### Pipe directly to another command / LLM tool

```bash
cli-pdf-extract examples/main.pdf --start-page 0 --end-page 5 | cat
```

## Notes

- Pages are zero-indexed.
- `--start-page` and `--end-page` must be provided together.
- `--page` cannot be combined with range flags.

## License

MIT. See [LICENSE](LICENSE).

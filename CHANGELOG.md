# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2025-01-05

### Added
- Performance benchmark suite (`examples/perf_bench.rs`) for testing large file operations
- Streaming file processing for reduced memory usage
- Heap-based sorting for directory listings with limits

### Changed
- **Performance**: Refactored file traversal to streaming processing
  - `find`: Stream files instead of collecting all upfront
  - `grep`: Line-by-line reading for context-free mode
  - `ls`: Heap-based sorting to handle large directories efficiently
  - `read`: Byte slice operations instead of line arrays
  - `truncate`: Stream-based processing for head/tail operations
- **Repository**: Migrated from `white1or1black/pixie-pi` to `pixie-agent/pixie-pi`

### Fixed
- Grep context mode emitting phantom lines after trailing newlines
- Truncate functions reporting incorrect line counts
- Read tool creating unnecessary string copies via `join()`
- Edge cases with newline handling (\r\n and \r endings)

### Performance
- Significant memory reduction for large file operations
- Faster directory listing when using limits
- Improved grep performance for context-free searches
- Optimized truncate operations for both head and tail modes

## [0.1.0] - Initial Release

### Features
- AI coding agent with read/write/edit/bash/grep/find/ls tools
- Interactive REPL and command-line modes
- Claude Code stream-json compatibility
- Anthropic Messages API support (no SDK dependency)
- LLM-summarizing session compaction with model tiering
- Prompt caching support
- Adaptive and budget-based thinking modes
- Session persistence (JSONL)
- Skill discovery (Claude Code compatible)
- Fuzzy edit matching
- Gitignore-aware search tools

### Tools
- `read` - Read files (text or image) with offset/limit paging
- `write` - Create or overwrite files
- `edit` - Exact and fuzzy text replacement with unified diff
- `bash` - Run shell commands with timeout and process-tree kill
- `grep` - Content search (regex/literal, context lines, respects gitignore)
- `find` - File glob search (respects gitignore)
- `ls` - Directory listing

### Configuration
- Environment variable support for API keys and endpoints
- CLI flags for model selection, thinking levels, and tool configuration
- Session continuation and resume
- Permission mode support (bypass-only)

# TODO

- **CLI integration tests.** No `tests/` coverage for the binary's stdout / stderr / exit-code contracts — only library APIs are tested. A small `assert_cmd`-based suite covering `guid` / `path` / `find` / `alias` / `usage` miss paths (exit 1, `did you mean:` on stderr) and hit paths (TSV / `--json` shapes) would lock the UX. Noted while normalising `find`'s miss UX (CLAUDE.md item 11).

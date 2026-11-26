# Instructions for coding agents

molcajete is a clientless remote desktop gateway (RDP, VNC, SSH) in the
manner of Apache Guacamole: a Rust workspace and a React web UI. The
README and `docs/` describe what it does; this file says how to work in
the repository.

Everything that is committed or posted to GitHub is written in English:
code, comments, docs, commit messages, issues and pull requests.

## Layout

| Path | What |
|-|-|
| `crates/protocol` | `molcajete-protocol`: the Guacamole instruction parser and binary records |
| `crates/client` | `molcajete-client`: the browser client, built to wasm |
| `crates/gateway` | `molcajete-gateway`: tunnel, JSON API, authentication, database |
| `crates/daemon` | `molcajeted`: the daemon that speaks RDP, VNC and SSH |
| `crates/telemetry` | logs, metrics and profiles shared by the binaries |
| `web/` | React UI with the Astryx design system (bun) |
| `tests/browser` | renderer, input, tunnel and playback tests in headless Chromium (npm) |
| `fuzz/` | fuzz targets, outside the workspace (nightly, `cargo fuzz`) |
| `deploy/`, `docker/` | deployment files, and the test targets for local runs |
| `docs/` | administration, deployment, observability, architecture |

## Checks

Run the checks for what you touched before every commit. CI runs the
Rust ones on each pull request; the others run only by hand.

```sh
# Rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p molcajete-client --target wasm32-unknown-unknown -- -D warnings
cargo test --workspace

# web/
(cd web && bun run check)        # tsc, ESLint, Prettier, unit tests; `bun run format` fixes formatting

# Browser tests, when the client, the protocol or the tunnel changed
scripts/build-web.sh
(cd tests/browser && npm install && npm test)
```

The README's Tests section lists the end-to-end tests, the benchmarks
and the fuzz targets, and what each needs.

For UI work in `web/`, also follow `web/.claude/CLAUDE.md`, the Astryx
guidance. The astryx CLI generates that file; do not edit it by hand.

A change in behavior, configuration, metrics or the API comes with the
matching change in `README.md`, `docs/` and `gateway.example.toml`.

## Workflow

Every change belongs to one GitHub issue, and one pull request closes
one issue.

1. Find the issue, or open one (see Issues).
2. Branch from `main` as `feature/GH-<n>-<short-slug>`, for example
   `feature/GH-112-video-frame-cadence`. Do not commit to `main`.
3. Commit with the issue's key (see Commit messages).
4. Open the pull request from the template (see Pull requests).

## Commit messages

`.gitmessage` is the commit template and the authority on the format.
Read it before the first commit of a session and follow it exactly.
`git config commit.template .gitmessage` shows it in the editor.

```
GH-112: Send video frames on a schedule instead of after the last one.

<What happened, under which conditions, and why.>

<What the code does now.>
```

- Subject: `<KEY>: <Summary>.` The summary starts with a capital letter,
  is in the imperative mood ("Add", "Fix", not "Added", "Fixes"), ends
  with a period and stays within about 72 characters.
- KEY is `GH-<n>`, never `#<n>`. Only a change with no issue uses a
  component instead: `gateway`, `client`, `daemon`, `protocol`,
  `telemetry`, `web`, `deploy`, `docker`, `docs`.
- All commits of an issue carry the same KEY, so its history reads as a
  series: `GH-104: Leave ...`, `GH-104: Document ...`.
- Body: wrapped at 72 characters. The problem first, then the behavior
  after the change. Several independent changes go in `- ` bullets with
  continuation lines indented by two spaces.
- Merge commits: `<KEY>: Merge <summary in lowercase>.`
- No Conventional Commits prefixes (`feat:`, `fix:`) and no trailers
  such as `Co-Authored-By` or `Signed-off-by`.
- Separate commits for separate steps of an issue (the change, its
  tests, its documentation) rather than one commit with everything.

## Issues

The forms in `.github/ISSUE_TEMPLATE/` define what an issue contains.
Pick the one that fits, and search open and closed issues first.

| Form | Label | Sections (`*` required) |
|-|-|-|
| `bug_report.yml` | `bug` | Component\*, Remote protocol\*, Affects version\*, Environment\*, Problem\*, Steps to reproduce, Logs |
| `improvement.yml` | `enhancement` | Component\*, Problem\*, Ideas, How to measure |
| `new_feature.yml` | `enhancement` | Component\*, Problem\*, Goal\*, Design |

`gh issue create` and the API do not apply the forms, so reproduce one
by hand:

- Read the form's file and write each field as a `### <label>` heading
  followed by its content, in the form's order, with the labels spelled
  as in the form. Leave out optional sections that would be empty.
- Take the Component and Remote protocol values from the form's options.
- Add the form's label with `--label`.
- The title is a plain sentence that states the problem or the goal,
  with no prefix: "Video mode sends every other frame of a server faster
  than 30 fps".
- Give facts: the commit that was measured, resolutions, metric names
  and numbers, what was expected. Remove passwords and hostnames from
  logs.

```sh
gh issue create --label enhancement \
  --title "Some RDP frames go out as one image per 64×64 tile" \
  --body-file <file>
```

Security problems are not reported as issues.

## Pull requests

`.github/pull_request_template.md` defines the body. Read it and keep
its structure; `gh pr create --body` does not fill it in for you.

- Title: the subject of the commit, `GH-<n>: <Summary>.`
- First line: `Closes #<n>.` Here `#<n>` is right, since GitHub links
  it. Drop the line only for a small change with no issue, such as a
  typo fix.
- `## Summary`: what was wrong or missing and why, then what the change
  does now. `- ` bullets for several independent changes.
- `## Deployment notes`: configuration, migrations, environment
  variables or proxy settings that change. Delete the section when
  there are none.
- `## Testing`: the commands that were run with their results, what was
  checked by hand (protocol, remote host, browser), and what was not
  verified.
- Remove the template's HTML comments from the body.
- Report only checks that were actually run, with their real results.

## What this changes

<!-- One paragraph. If it fixes a number, say which number and how you verified it. -->

## Checks

- [ ] `cd rust-server && cargo test --release` passes
- [ ] `./scripts/build_app.sh` produces an app (CI does this too)
- [ ] `./scripts/privacy_check.sh` passes
- [ ] Any new fixture is **synthetic** — no real transcripts, paths, project names or usage
      figures from anyone's machine. There is a test in the parser suite that exists precisely
      because a real path was committed once.
- [ ] If this touches a parser or the aggregation, the numbers were checked against the real
      thing, not only against the unit tests

## Screenshots / before-after numbers

<!-- Charts and the HUD are visual; a picture or two numbers is worth more than prose. -->

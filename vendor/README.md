# vendor/

Third-party assets that ship **inside** the app, so the dashboard never reaches
out to a CDN. Loading a chart library from someone else's server means every
dashboard open leaks the user's IP to that server and hands a third party
script access to a page that can read the whole local token store — and it
breaks the dashboard when the machine is offline or the CDN is blocked.

| File | Version | From | License |
|---|---|---|---|
| `chart.umd.min.js` | chart.js 4.4.3 | `npm pack chart.js@4.4.3` → `dist/chart.umd.js` | MIT (banner kept in the file) |

Serving: the Rust server answers `GET /vendor/chart.umd.min.js` from this file
(`read_asset()` in `rust-server/src/main.rs` resolves `~/.tokendance/vendor/` →
working directory → app bundle, same order as the pages), and
`scripts/build_app.sh` copies the directory into `Contents/Resources/vendor/`.

To bump a version, download the new file, update the row above and the version
in the table, and re-run `./scripts/build_app.sh`.

# Security

## Reporting

Open a [private security advisory](https://github.com/philcui/tokendance/security/advisories/new)
rather than a public issue. Please include the version (menu bar → 关于) and what you did.

## What is in scope

- The local server that binds `127.0.0.1:8737`, and anything that could make it reachable from
  another machine. It is meant to be unreachable from the network; if you find a way to talk to it
  from off-box, that is a real bug.
- The parsers and the discovery scanner. They read files under `$HOME` and must never read
  anything outside the paths they are given, follow a symlink out of a scan root, or open a
  system privacy store (contacts, messages, mail, Health, Calendar). There is a test for the
  last one; a way around it is a real bug.
- The updater, which replaces the app bundle with administrator privileges. Anything that could
  make it install something other than a release from the configured endpoint is the most
  serious class of bug here.

## What is out of scope

- The official telemetry / update / leaderboard service. That half is not in this repository; it
  is a separate private service. Report problems with it to the same address and they will be
  handled, but the code will not be published.
- Anything that requires an attacker who already runs code as your user. Everything here runs as
  you, reads your files and writes your database.

## What the app sends

Four endpoints, listed in full in the README under *Network behaviour*. No usage figures, file
names, project paths or session content, and the ping log keeps time, method, path and status
only — not IP addresses.

# Security policy

## Supported versions

Only the latest release receives security fixes. Upgrade with
`npm install -g spoolway@latest` before reporting, and check whether the issue
still reproduces.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub:
[open a private advisory](https://github.com/marvingygas/spoolway/security/advisories/new).
Do not open a public issue, discussion or pull request for a security problem.

A useful report says which version you ran (`spoolway --version`), what you
did, and what an attacker gains. A minimal reproduction helps most.

You can expect a first answer within a week. Once a fix is released, the
advisory is published with credit to you, unless you ask to stay anonymous.

## Scope

spoolway runs coding agents on your machine with the permissions you give them.
Issues in spoolway itself are in scope: the binary, the npm packages, the
release workflow, and the files `spoolway init` and `spoolway sync` write.

Behaviour of the agents spoolway launches, such as Claude Code, belongs to
those projects. Report it to them directly.

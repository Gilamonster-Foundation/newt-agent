# Setup — backends, discovery, and credentials

`newt setup` finds an inference endpoint and writes it down. This page covers
the cases the README's five-line quick start deliberately leaves out.

## Install

```bash
git clone https://github.com/Gilamonster-Foundation/newt-agent
cd newt-agent
just install          # release binaries → ~/bin/newt, ~/bin/newt-mcp-server
```

## Discovery — a bare host

A bare hostname is probed **anonymously** across the configured discovery ports
(8000 and 8080 by default):

```bash
newt setup inference.example.net
```

Newt connects, asks each port what it is, and offers the models it detects.

## An authenticated endpoint

Give the exact HTTPS URL and a **reference to** the secret — never the secret
itself:

```bash
newt setup https://inference.example.net:8000 --token-env INFERENCE_TOKEN
newt setup https://inference.example.net:8080 --token-file ~/.config/newt/token
```

`--token-env` reads the named environment variable at call time; `--token-file`
reads a path. Neither writes the token into config. A token pasted inline would
land in a git-tracked file and in every backup of it — so there is no flag that
accepts one.

## Where it lands

Each detected endpoint becomes its own drop-in:

```
~/.newt/backends/<name>.toml     one file per endpoint
~/.newt/config.toml              records only the selected default_backend
```

The main config stays lean on purpose. Endpoints are composed from drop-ins, so
adding, removing, or overriding one is a file operation rather than an edit to a
shared file — and a machine-specific endpoint never has to be committed.

Inspect the result with `newt config`. `newt --help` is the authority on the
full flag surface; this page is not.

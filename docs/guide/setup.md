# Setup — backends, discovery, and credentials

`newt setup` finds an inference endpoint and writes it down. This page covers
the cases the README's five-line quick start deliberately leaves out.

## Install

```bash
git clone https://github.com/Gilamonster-Foundation/newt-agent
cd newt-agent
just install          # release binaries → ~/bin/newt, ~/bin/newt-mcp-server
```

## First run (unboxing)

A fresh box — no `~/.newt` config file and no backend from any other source
(drop-in, `--backend-*` flag) — opens the interactive wizard **immediately**
when you launch `newt` at a terminal:

```
Where does your model run?
  1) Ollama on this machine   (http://127.0.0.1:11434)
  2) Another machine          (hostname or URL — newt probes for Ollama / llama.cpp / vLLM)
  3) OpenAI                   (https://api.openai.com — API key)
  4) Anthropic                (https://api.anthropic.com — API key)
  5) Ollama Cloud             (https://ollama.com — API key)
```

Esc or Ctrl-C at any prompt skips setup: newt writes localhost defaults and
starts anyway (`newt setup` re-opens the wizard later, and `/setup` does the
same from inside a session). CI, image builds, and piped invocations never see
a prompt — they take the silent probe-and-write path immediately, exactly as
before.

Choice 2 expands the host through `[discovery]`, probes every candidate port
concurrently, tells the engines apart (Ollama, llama.cpp, vLLM — fingerprinted,
not guessed from the port), and lists what each endpoint serves. The model that
is **already loaded** on the endpoint is the Enter default, so accepting the
defaults gets you the model that answers immediately.

To rehearse the unboxing flow without touching your real config, point the
config root somewhere disposable — never move directories aside:

```bash
NEWT_CONFIG_DIR=$(mktemp -d) newt
```

## Credentials — encrypted at rest

Prefer an exported environment variable: the wizard records the *reference*
(`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OLLAMA_API_KEY`) and stores nothing.

A key **pasted** into the wizard is typed with echo off and lands on disk as
[age](https://age-encryption.org) ciphertext — never plaintext:

```
~/.newt/backends/<name>.token.age   the encrypted token (0600)
~/.newt/secrets/identity.txt        the machine key (0600, dir 0700)
```

- **Enter at the passphrase prompt** encrypts to the machine-local identity
  above; decryption is transparent. Be honest about what that buys: the key
  sits beside the lock, so it protects backups, synced dotfiles, and casual
  greps — an attacker who captures *both* files together can decrypt. It is an
  upgrade over plaintext, not a vault.
- **A passphrase** switches to age's scrypt mode; the key never touches disk.
  Interactive sessions ask once at startup; headless runs read
  `NEWT_TOKEN_PASSPHRASE` (which is excluded from every child-process
  environment newt spawns). A locked token warns and resolves to nothing —
  it never hangs a run.

The files are standard age: `age -d -i ~/.newt/secrets/identity.txt
~/.newt/backends/<name>.token.age` decrypts with the stock CLI. Legacy
plaintext `.token` files keep working; `newt doctor` shows each backend's
credential state and nudges plaintext files toward re-running setup.

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

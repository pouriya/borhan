# borhan

[![ci](https://github.com/pouriya/borhan/actions/workflows/ci.yml/badge.svg)](https://github.com/pouriya/borhan/actions/workflows/ci.yml) [![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Memory for AI agents, with a search engine instead of grep.**

Your agent forgets everything when a conversation ends. borhan is where it keeps what matters, and how it finds it again months later. borhan relies on the agent for two jobs, and teaches it to do both well:

- **Your agent writes the searches.** borhan does not guess what you meant; it teaches the agent what a good search looks like. Searches are written in borhan's own query language, where each idea is spelled several ways:

  ```
  (password passwords رمز گذرواژه) +(reset بازنشانی)
  ```

  That asks for two ideas, passwords and resetting, each in English and Persian, and requires the second. It finds both `Password reset links expire after 15 minutes.` and `لینک بازنشانی رمز عبور بعد از ۱۵ دقیقه منقضی می‌شود.`
- **Your agent decides what to store.** borhan does not record transcripts. Two ready-made skills, `/borhan-remember` and `/borhan-survey`, teach the agent what is worth keeping: decisions with their reasons, your corrections, dead ends, what each part of a codebase does and why. They also teach it how to write a note so it can be found later: one idea per paragraph, the searchable words first, key terms in every language the memory holds.

## Why not Markdown files and grep?

Most agents today remember things in Markdown files and grep them. That works for a few dozen notes. borhan is built for the scale where it doesn't: thousands of memories, each holding thousands of sessions, each session holding its own conversation of messages.

| | Markdown files + grep | borhan |
|---|---|---|
| **Organizing** | Folders and file names | Named memories, each with a description of what it holds and what it doesn't, split into sessions and messages, recording who said what |
| **Matching** | Exact text: `retry` misses `retries`, and a Persian word misses itself written with `ي` instead of `ی` or without its half-space | Words: `retry` finds `retries`, Persian spellings are normalized, and the exact spelling still ranks first |
| **Several ideas at once** | Pipe one grep into another, and every idea must land on the same line | Ideas are matched within the same paragraph, each spelled as many ways as you like |
| **Ranking** | None: every matching line counts the same | Best first: by how many of the ideas match, how close together, how recent |
| **Words that aren't there** | An alternative that never occurs is silently ignored | Each word that matched nothing is reported, and `lexicon` checks words before searching |
| **What comes back** | Lines | Paragraphs, each with a cursor for reading the paragraphs around it or the whole message |

Here is the kind of query borhan teaches your agent to write:

```
(error failure fault خطا مشکل) +(payment checkout پرداخت) (retry retries "try again")
```

That is three ideas: something went wrong, it involved payments, and a retry. Each is spelled several ways, in two languages, and payments are required. A paragraph that mentions all three ranks first; one that mentions a payment error without a retry still comes back, just lower. The closest grep:

```bash
grep -rniE 'error|failure|fault|خطا|مشکل' notes/ | grep -iE 'payment|checkout|پرداخت' | grep -iE 'retry|retries|try again'
```

That only finds lines with all three on them, in no particular order, and drops the paragraph that mentions two.

**It is not a vector database either.** Embedding-based memory returns text that is *similar* to the question, which is exactly the wrong thing for a decision, a name or an identifier, and it needs a model to run. borhan returns text that contains what was asked for, says why it matched, and is a single binary with nothing to download.

It works over **MCP**, from the **command line** and over **HTTP**, and it understands English and Persian, in one memory or in separate ones.

## Install

On Linux or macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/pouriya/borhan/master/install.sh | sh
```

On Windows, in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/pouriya/borhan/master/install.ps1 | iex"
```

With Docker, which serves borhan on port 1995 and keeps your memories in the `borhan` volume:

```bash
docker run -d --name borhan -p 127.0.0.1:1995:1995 -e BORHAN_TOKEN="$(openssl rand -hex 24)" -v borhan:/var/lib/borhan ghcr.io/pouriya/borhan:latest
```

## Getting started

First create the store, which lives in `~/.borhan`. With Docker, the container has already done this:

```bash
borhan init storage
```

### Server mode

There are two reasons to run borhan as a server, and only two:

- **MCP**, so agents like Claude Code, Cursor or Codex can use it as a tool.
- **The REST API**, so your own code can read and write the memory over HTTP.

Neither one yours? Skip this section — borhan works on its own from the command line, which is how most agents use it. Carry on at [Without a server](#without-a-server).

**Just you, on one machine?** Keep it on loopback. Nothing outside your machine can reach the port, which makes this both the easiest and the safest way to run it:

```bash
borhan init server --listen 127.0.0.1:1995 --token "$(openssl rand -hex 24)"
borhan serve
```

**Several people, several machines?** Put the server on a host everyone can reach and keep it running there. With Docker — the same image as above, published on every interface instead of loopback:

```bash
docker run -d --name borhan -p 1995:1995 \
  -e BORHAN_TOKEN="$(openssl rand -hex 24)" \
  -v borhan:/var/lib/borhan \
  ghcr.io/pouriya/borhan:latest
```

Or with systemd, which keeps it running across reboots:

<details>
<summary><b>systemd unit</b> — copy, paste, done</summary>

First the store and the config, as the user the service will run as:

```bash
borhan init storage
borhan init server --listen 0.0.0.0:1995 --token "$(openssl rand -hex 24)"
```

Then the unit. The heredoc fills in your user, your home and wherever the installer put the binary, so this goes in as-is:

```bash
sudo tee /etc/systemd/system/borhan.service >/dev/null <<EOF
[Unit]
Description=borhan memory store
After=network.target

[Service]
Type=exec
User=$(whoami)
ExecStartPre=$(command -v borhan) --home $HOME/.borhan init storage
ExecStart=$(command -v borhan) --home $HOME/.borhan --info serve
Restart=on-failure
RestartSec=5

NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=read-only
ReadWritePaths=$HOME/.borhan

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl enable --now borhan
```

`ExecStartPre` rebuilds each memory's index on every start, so an upgraded borhan never serves an index built by the older one. `--info` is there because the default level logs only failures, which on a service means a journal that never says what it bound to.

Check it with `systemctl status borhan` and `journalctl -u borhan -f`. From a clone of this repository, `sudo make systemd-install` does all of the above for you.

</details>

Anyone who can reach the port and knows the token has the whole memory, so put nginx or Caddy in front with a certificate if this is leaving a private network.

**Then point each machine at it.** One file on each, `~/.borhan/server.toml`, and nothing else — no `init`, no storage of its own:

```toml
remote_address = "https://borhan.example.com"
token = "the-same-token-the-server-uses"
```

Every `borhan` command on that machine now goes to the server. If it can't be reached, the command stops and tells you. It won't fall back to a local memory, because that one is empty and your note would end up where nobody is looking for it.

If the server's certificate is self-signed, or from a company CA this machine hasn't been told about, add `remote_skip_tls_verify = true`. That skips the certificate check entirely — who signed it, and whether it was issued for the host you typed — so anyone able to reroute the traffic can pose as the server and read your token. Fine on a VPN you trust. Not over the open internet.

**To connect an agent over MCP**, the endpoint is the server's address plus `/mcp`, with the token as `Authorization: Bearer <token>`. The token is in `~/.borhan/server.toml`. The easiest way is to let the agent set it up:

```
Add an MCP server named borhan to your configuration. It is an HTTP server at http://127.0.0.1:1995/mcp and needs the header "Authorization: Bearer <token>".
```

### Without a server

Skip all of that and you lose nothing. Any agent that can run shell commands uses the `borhan` command directly, and `borhan --help` is written to teach it everything it needs. Just tell it:

```
Run `borhan --help` and use borhan as your long-term memory.
```

### Load the skills

```bash
borhan skills remember --install
borhan skills survey --install
```

This writes them to `~/.agents/skills`, and to `~/.claude`, `~/.codex` and `~/.hermes` when those exist. Then say `/borhan-remember` at the end of a conversation, and `/borhan-survey` when you start on a codebase. With Docker, print a skill and save it yourself, for example `mkdir -p ~/.agents/skills/borhan-remember && docker exec borhan borhan skills remember > ~/.agents/skills/borhan-remember/SKILL.md`.

### Use it by hand

Your agent normally does all of this, but using borhan by hand once is the quickest way to see what your agent sees. With Docker, put `docker exec borhan` in front of each command.

Create a memory. A memory is a broad subject, not a single feature or file: typical stores have one for the `company`, one for `projects`, and one for each person. Its description tells your agent what belongs in it and what doesn't, so be specific about both. Tag it with the languages it will hold:

```bash
borhan memory create company --languages en,fa --description "How our company works: who owns which service, why each policy exists, past incidents and what they taught us, and what was tried before. Not code, and not anyone's personal preferences."
```

Add a few notes. Each note belongs to a session, such as a conversation, and search returns its paragraphs:

```bash
borhan memory add company --session security-review --role user --author pouriya "Password reset links expire after 15 minutes, because longer-lived links kept showing up in forwarded emails.

لینک بازنشانی رمز عبور بعد از ۱۵ دقیقه منقضی می‌شود."

borhan memory add company --session security-review --role assistant --author claude "We rate limit reset requests to 5 per hour per account, so the reset form cannot be used to spam someone's inbox."
```

Check which words the memory knows before searching for them. `password` is stored as `Password`, so it has no exact match but one folded match, and `expiry` appears nowhere:

```bash
borhan memory lexicon company password reset expiry منقضی
```

```
word      surface       lemma         context
password  password (0)  password (1)  0
reset     reset (2)     reset (2)     0
expiry    expiry (0)    expiri (0)    0
منقضی     منقضی (1)     منقض (1)      0
```

Search. `cover` says how many of the query's ideas each paragraph matched, and it is the number to trust. The third result mentions resets but not passwords, so it comes back last, with `1/2`:

```bash
borhan memory search company '(password passwords رمز) +(reset بازنشانی)'
```

```
score  cover  cursor                      session          message  size      matched
1.000  2/2    01M2QEH33Q6HMWPFQDPJV7RH2C  security-review  -        10 words  [(password OR passwords OR رمز),(reset OR بازنشانی)]
"لینک بازنشانی رمز عبور بعد از ۱۵ دقیقه منقضی می‌شود."

0.584  2/2    01M2QEH33QWPQ9KYPTCM9J818K  security-review  -        16 words  [(password OR passwords OR رمز),(reset OR بازنشانی)]
"Password reset links expire after 15 minutes, because longer-lived links kept showing up in forwarded emails."

0.059  1/2    01M2QEH35DAJQ1YGKNJWBY60P9  security-review  -        22 words  [(reset OR بازنشانی)]
"We rate limit reset requests to 5 per hour per account, so the reset form cannot be used to spam someone's inbox."
```

Search also takes a few options: `--fuzzy` tolerates a one-letter typo, `--role user` keeps only what the user said, `--after` and `--before` limit results to a time range (in unix milliseconds), and `--limit` sets how many results come back.

Read a result in context by passing its cursor. `--after 2` also shows the two paragraphs that follow it:

```bash
borhan memory cursor company 01M2QEH33QWPQ9KYPTCM9J818K --after 2
```

```
hit  unit                        role       session          seq
→    01M2QEH33QWPQ9KYPTCM9J818K  user       security-review  0.0
Password reset links expire after 15 minutes, because longer-lived links kept showing up in forwarded emails.

     01M2QEH33Q6HMWPFQDPJV7RH2C  user       security-review  0.1
لینک بازنشانی رمز عبور بعد از ۱۵ دقیقه منقضی می‌شود.

     01M2QEH35DAJQ1YGKNJWBY60P9  assistant  security-review  1.0
We rate limit reset requests to 5 per hour per account, so the reset form cannot be used to spam someone's inbox.
```

Every command accepts `--json`, and `borhan <command> --help` explains each one in full.

## Query language

A query is a list of ideas. Words inside one pair of parentheses are one idea, spelled several ways; ideas side by side are separate ideas. Results are paragraphs, ranked first by how many of the ideas they match, and each idea counts once no matter how many of its spellings a paragraph contains.

| Query | Meaning |
|---|---|
| `webhook` | One word. Also finds `webhooks`. |
| `(retry retries backoff)` | One idea, spelled three ways. |
| `(error خطا) (payment پرداخت)` | Two ideas, each in English and Persian. |
| `(error خطا) +(payment پرداخت)` | The second idea is required. |
| `(login signin ورود) -oauth` | Drop paragraphs that mention `oauth`. |
| `retry NOT idempotency` | The same as `retry -idempotency`. |
| `(retry AND backoff) webhook` | Both words are needed for the first idea. |
| `"rate limit"` | A phrase: these words, together, in this order. |
| `"retry failed"~2` | A phrase with up to two other words in between. |
| `"exponential back"*` | A phrase whose last word is a prefix: `backoff`, `backend`. |
| `(postgres^2 database db)` | A match on `postgres` counts twice as much. |
| `surface:JWT_SECRET` | This exact spelling, capital letters included. |

Regular expressions, ranges, a lone `*` and one-word prefixes like `back*` are not supported; borhan refuses them with a message saying what to write instead.

The syntax is [tantivy's query language](https://docs.rs/tantivy/0.26.1/tantivy/query/struct.QueryParser.html), so its documentation covers the details. What borhan adds is the ranking: one idea per pair of parentheses, scored by its best spelling, and results ordered by how many ideas they cover.

## Security

borhan has no user accounts. Whoever holds the token can read and change every memory on that server, so treat the token like a password.

- **Always set a token** on a server anyone else can reach. Without one, anything that can connect to the port has full access. The token is stored in `server.toml`, which only your user can read.
- **Keep it on `127.0.0.1` unless you are sharing it.** That is the default, and only your own machine can connect. With Docker, keep the `127.0.0.1:` in `-p 127.0.0.1:1995:1995`; without it the port is open on every network interface.
- **Refuse what you don't need.** `borhan init server --refuse delete --refuse replace` gives a server that can store but never destroy anything. Refused operations disappear from the agent's tool list, and the generated `server.toml` explains every option.
- **Web pages can't use the MCP endpoint.** It rejects requests sent from a browser page, so a site you visit cannot reach your memories through it.
- **The server speaks plain HTTP.** It terminates no TLS of its own, so to share it across a network put it behind a reverse proxy that does. Clients reach that proxy over `https://` through `remote_address`; without one, the token travels unencrypted.
- **The guide at `/` needs no token.** It only documents the API; it contains no memories.

Found a vulnerability? Please report it privately to pouriya.jahanbakhsh@gmail.com rather than opening a public issue.

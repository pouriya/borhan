---
name: borhan-survey
description: "Read a codebase and file what each part of it does into a borhan memory, one message per feature, so that a later search answers questions about this project. Use when the user asks you to survey, map, document or remember a project, invokes /borhan-survey, or when you are about to start work on a codebase nobody has described yet. Confirms which memory with options rather than guessing, names the session after the project, and reconciles with any survey already on file instead of replacing it. Pairs with borhan-remember: survey first when a project is not in the memory yet, then remember at the end of each working session on it."
license: MIT
---

# borhan-survey

A repository says what it does; it does not say what it is *for*. Git holds every
change and no explanation of any of them. This skill spends one pass over a
project writing down what each part does and why, in a borhan memory, so that in
four months a search for two words finds the answer instead of a file listing.

**Arguments to this skill are memory names separated by spaces** —
`/borhan-survey project_borhan`. They say *where*, not *what*, and they do not
remove the confirmation in step 3.

Work through the steps in order. Steps 1 and 2 are cheap and can fail the whole
thing; a survey is hours of reading, and discovering afterwards that nothing can
be stored wastes all of it.

## Where this sits

`borhan-survey` and `borhan-remember` are one workflow. They write into the same
memory and must never write into the same session:

| skill | one session per | what is in it | grows by |
|---|---|---|---|
| `borhan-survey` | project | what the code does and why | replacing what went stale |
| `borhan-remember` | conversation | what happened and what was decided | appending, never replacing |

Survey a project the first time you touch it, or any time it is not in the memory
yet. Then run remember at the end of each working session on it. Then survey
again when enough has changed that the feature descriptions have stopped being
true — it reconciles rather than starting over.

**Never write conversation content into a project session, and never write
feature descriptions into a conversation session.** Every search result prints
the session it came from, so `acme-api` and `2026-09-02-auth-rewrite` tell a
reader at a glance whether they are looking at how the code works or at what was
decided one afternoon. Mixing the two throws that signal away, and it is how one
feature ends up described in nine different conversations with nothing saying
which description is current.

## The one thing that must not go wrong

Choosing the wrong memory, or the wrong session, is worse than storing nothing.

Storing nothing is recoverable: the conversation is still on screen, the code is
still in git. Storing in the wrong place is not, because it fails **silently**.
Nothing errors. Everything looks like it worked. The cost arrives months later,
when one search returns four near-identical answers from four places and nothing
on any of them says which is current — and nobody ever goes back and audits a
memory.

So: **never guess, and never proceed on silence.**

- **No memory named in the arguments → list them and ask.** Every time. Not "the
  obvious one", not "the only one that mentions code".
- **Anything less than certain about which session → list what exists and ask.**
- **A name you were handed that `memory_list` does not show → ask.** Do not
  create it, and do not quietly fall back to one with a similar name.

**How to ask.** If you have a tool for putting a question with options to the
user — an ask/question/choice tool, an elicitation, anything that renders
choices — use it. The answer comes back unambiguous and the user does not have to
type. If you have no such tool, print numbered options and wait for a number:

    Which memory should this go in?
      1. projects — how the codebases here work: what each service does, how,
         and why it was built that way
      2. pouriya  — one person: preferences, corrections, how they want to be
         worked with
      3. company  — policies, ownership, what was tried before
      4. none of these — stop, and tell me what to create instead

Never ask an open question where options will do. "Which memory should I use?"
hands the user the work you just did with `memory_list`. And always offer the
last option: a user who cannot say *no* picks the least wrong answer, and the
least wrong answer is still wrong.

## 1. Find borhan

In this order, and stop at the first one that answers:

1. **Your MCP tools.** If `memory_list`, `memory_search` or `memory_outline` is
   among them, use MCP for everything below. Prefer it: the tool schemas carry
   the argument rules, so you are reading the current ones rather than the ones
   in this file.
2. **The command line.** Run `borhan --version`. If it answers, use the CLI.
3. **Neither.** Stop. Tell the user that borhan is not reachable from this
   session and that a survey has nowhere to go — they can install it, or start
   the server and add it to your MCP configuration. Do not write the survey
   somewhere else, do not offer to keep it in a Markdown file in the repo, and
   do not go on to step 2. A file in the repo is the thing they already have.

## 2. Check that it will accept the writes, before reading any code

A survey needs two operations, `add` and `replace`, and a server can be
configured to refuse either.

**Over MCP**, read your tool list:

- `memory_add` missing → this server does not store. Say so and stop.
- `memory_replace` missing → you can write a first survey but cannot correct an
  existing one. This is only fatal if step 5 finds a survey already on file. Note
  it and carry on; raise it there.

**Over the CLI**, a server may be answering behind it, in which case a write
comes back as `refused: this server does not do add`.

Same conclusion.

### A refusal is an answer, not an obstacle

**It is somebody's decision, already made, and your job is to report it — not to
undo it.** A server that refuses `replace` is one whose operator has said that
stored text is not overwritten here.

So when you hit one:

- **Do not retry.** Nothing about the request was wrong; the second attempt is
  refused identically.
- **Do not go looking for `server.toml`,** do not edit it, do not restart
  anything, and do not reach around the server to the files underneath. None of
  that is yours to do, and an agent that starts down that path spends the rest of
  the session on it. It will not be reading the code, which is what it was asked
  for.
- **Tell the user, in one line:** which operation is refused, and what that costs
  them here — *this server refuses `replace`, so I can write a new survey but
  cannot correct the one already on file.*
- **Then let them choose.** If they tell you to change the configuration, do it;
  that is a different instruction and it is theirs to give. Until they do, the
  refusal stands.

Do not begin reading the project until this passes. And if you have to stop, do
not paste a survey you already wrote into the chat as a consolation — the user
asked for something searchable, and a wall of text they now have to file by hand
is not a smaller version of that.

## 3. Choose the memory, with the user

Always call `memory_list` first, even when the arguments named a memory. You need
the descriptions: they are the only thing that says what each memory is *for*,
and a name alone is a guess.

A codebase belongs in a memory about projects and their reasoning. Some stores
keep one memory per project, some keep one for all of them; the descriptions tell
you which, and the counts tell you whether the memory is already carrying other
projects.

**If no memory fits, do not create one silently.** Propose one, and let the user
write the description — every later caller chooses between memories by reading
it, so it is the user's sentence, not yours. Offer a draft to react to:

> There is no memory here for code. Shall I create `projects`, described as:
> "How the codebases in this organisation actually work: what each service does,
> how it does it and why it was built that way, one session per project. Not the
> code itself, which git holds, and not the tickets."
>
> A description should say what is in it *and what is not* — correct mine.

Then **confirm the memory before you scan**, even when an argument named it, and
confirm it as a choice rather than as a yes/no — see *The one thing that must not
go wrong* above. If the arguments named a memory, it is the default option, not
the decision.

A survey is expensive and hard to unpick once it is in the wrong place. One
question now costs a sentence; the alternative costs a memory that nobody can
trust the shape of.

## 4. Name the session, and check the name where it matters

One project is one session. The session id is what every hit prints, so it should
be the thing a person recognises: the project's name.

**Check that name against the memory, not against the filesystem.** Run:

    memory_outline { memory }              # MCP
    borhan memory outline <memory>         # CLI

That lists every session already there. The collision that hurts is inside the
memory, not on disk — two clones of one repository, or the same project surveyed
from a laptop and from a server, produce the same name while each looks perfectly
unique in its own directory. Looking at sibling directories on disk finds a
collision that will probably never happen and misses the one that will.

**If the name is not obvious, ask — with options.** A project whose directory,
repository and package all disagree about its name has no single right answer,
and picking one silently is how the same project ends up surveyed twice under two
names. Offer the two or three candidates you found, say where each came from, and
include *none of these*.

If the plain name is taken by a **different** project, add the smallest thing
that distinguishes them — usually the parent directory, `acme-api` rather than
`api`. Do not use the absolute path as the session id. A path tokenizes badly,
it is printed on every result line, and it stops being true the moment the
project moves. Put the path *inside* the first message instead, where it is
searchable prose rather than an identifier.

## 5. If it has been surveyed before, reconcile — never wipe

**`outline` lists every session in the memory, not only surveys.** If
`borhan-remember` has been used on this memory, its conversation sessions are in
that list too — `2026-09-02-auth-rewrite` beside `acme-api`. They are not stale
surveys and this skill must never touch them. A conversation session is somebody's
record of what happened on a day; replacing a message in one would overwrite
history with a description of code, and nothing would say it had happened. If you
cannot tell which kind a session is, read one message from it with
`memory_cursor` before you assume, and if it is still unclear, ask.

If step 4 found this project's session already there, read what is in it:

    memory_outline { memory, session }          # MCP
    borhan memory outline <memory> <session>    # CLI

That gives every message id, its size and when it was written, without any
bodies. Read the ones you need with `memory_cursor`.

There are exactly three answers, and most messages get the third:

- **New feature, no message for it** → add it. Nothing else to decide.
- **Feature still there, description no longer true** → `memory_replace`, same
  message id. The message keeps its place and its author; only the text and the
  time change. Adding a corrected copy beside the old one instead is the one
  genuinely bad outcome here: a search then returns both, and nothing on either
  says which is current.
- **Feature unchanged** → leave it alone. Do not rewrite a description to phrase
  it better. A replace costs a reindex and gains nothing.

**Never delete the session and start over,** and do not offer to. A survey is
dated: what a feature looked like six months ago is a real answer to a real
question, and git holds the diff but not the explanation. Replacing only what is
wrong keeps that and costs less.

Before any `memory_replace`, tell the user what you are about to overwrite and
why — the old body is not kept anywhere. List them together rather than asking
once per message:

> 3 of the 11 descriptions on file are now wrong: `auth` (tokens became JWTs),
> `indexing` (the queue moved to Redis), `deploy` (nomad → kubernetes). 2 new
> features to add: `rate-limiting`, `audit-log`. The other 8 are still accurate
> and I will leave them untouched. Replace those 3?

If `memory_replace` was missing back in step 2, say so here: you can add the new
features but not correct the stale ones, and adding corrections beside them would
make the memory worse. Ask whether to add only the new ones or to stop.

## 6. The scan

This is the work. Everything above was setting up where it goes.

### Read before you claim

An agent asked to describe a whole repository will fill the gaps it did not read,
and a memory is the worst possible place for that: months later the invention
comes back with a confident score and nothing distinguishes it from a fact.

So: describe what you have actually opened. Name the files you read in the body —
it makes the claim checkable and it makes the paths searchable. Where you have
not read something, say so in the overview message ("the `legacy/` tree is not
covered"). A survey with a stated hole is worth far more than one that quietly
fills it.

Do not try to cover every file. Cover the **features**: the things somebody would
ask about later. Ten good messages beat sixty thin ones.

### One message per feature

Pick the message id from the feature, in lowercase with hyphens — `auth-tokens`,
`pdf-ingestion`, `rate-limiting`. It is printed on every hit, and it is the
handle a later survey uses to replace this message, so it has to still make sense
in a year. Not `part-1`, not a filename, not a date.

Start the session with one **overview** message, id `overview`:

- what the project is, in one sentence a stranger could use
- where it lives (the absolute path) and what it is written in
- how it is built, run and tested
- the commit it was surveyed at
- what this survey does *not* cover

Then one message per feature. Each should answer:

- **what** it does, in the vocabulary someone would search for
- **how** it does it — the actual mechanism, not a restatement of the name
- **where** it lives, by path
- **why** it is that way, when the code or its comments say — this is the part
  that cannot be recovered from the repository later, and the most valuable
  thing in the whole memory
- **what it deliberately does not do**, when that was a decision

### Write for the paragraph, not for the message

**Search returns one paragraph, not the message it came from.** This shapes
everything about how you write here, and it is the instruction most easily
skipped.

A paragraph arrives at a future reader alone, with no title above it and nothing
before it. So a paragraph reading:

> It does this by calling `handle()`, which retries three times.

is useless as a result: it names neither the feature nor the file, and matches
nothing anyone would search for. Written to stand alone:

> Retrying in the webhook receiver is done by `handle()` in
> `src/webhooks/receive.rs`, which retries three times with exponential backoff
> before dropping the event to the dead-letter table.

Concretely:

- **Blank lines between paragraphs.** They are what splits a message into units.
  A wall of text is one enormous unit that matches everything weakly and reads
  as noise. A heading, a list item, a table row and a fenced code block are each
  their own unit too.
- **Name the feature and the file in every paragraph.** Repetition looks clumsy
  in the message read top to bottom, and it is what makes each paragraph findable
  on its own. Write for the search, not for the read-through.
- **Lead with the words someone would search for.** "Rate limiting is…", not
  "There is a mechanism which…".
- **No line numbers, no code dumps.** Line numbers rot on the next commit, and
  the code is already in git — a memory holding a copy of it is a memory that is
  wrong as soon as anyone edits. Name the function and the file.
- **If the memory's languages include `fa`**, name key terms in both languages
  the first time they appear, so both queries find the unit.

### Roles and authorship

A survey is *your* description of someone else's code, and it should say so:

| what it is | `role` | `author` |
|---|---|---|
| a description you wrote from reading the code | `assistant` | your agent or model name |
| something the user told you about the project | `user` | the name they go by |
| output worth keeping from a build or test run | `tool` | the tool's name |

Never file your own reading of the code under `role: user`. A later search
filtered to `user` is asking what a person actually said about this project, and
your inference sitting there answers it wrongly and confidently. If the user
explained something during the survey that the code does not say, that is worth
its own message under their name — it is the most valuable kind of entry here.

Use `ts` of now. Everything in a survey was written now, whatever the code's age.

### The calls

    borhan memory add projects \
        "Rate limiting lives in \`src/limits/mod.rs\` and is applied per API
        token, not per IP: the gateway already terminates TLS for several
        clients behind one address, so an IP bucket would have throttled a
        whole customer for one noisy caller.

        The rate limiter keeps counters in Redis under \`limit:<token>:<minute>\`
        with a 120-second TTL, so a restart loses at most one window rather than
        resetting every client's budget." \
        --session acme-api \
        --message rate-limiting \
        --role assistant \
        --author claude \

    memory_add     { memory, session, message, body, role, author }
    memory_replace { memory, session, message, body }

Both are refused if the message id is already in the session — that is the point:
a re-run fails on what is already stored rather than duplicating it. A refusal
means skip that one and carry on. Do not renumber around it, and do not switch to
`memory_replace` to force it through unless step 5 decided that message was
stale.

## 7. Verify, then report

Search the memory for two or three things you just filed, in the words a stranger
would use rather than the ones you wrote. If a feature you described does not
come back, its paragraphs were written for the message and not for the paragraph
— fix those and re-file them.

Then tell the user, plainly:

- which memory and which session id
- how many messages added, how many replaced, how many left alone
- what you deliberately did not cover, and why

That last line is the one that makes the memory trustworthy later. A survey
honest about its holes can be extended; one that hid them cannot be believed
anywhere.

Then say what comes next, because the survey is the beginning of the workflow and
not the end of it: from here, `/borhan-remember` at the end of each working
session on this project files what was decided, in its own session, and a later
`/borhan-survey` reconciles the feature descriptions this work makes untrue.

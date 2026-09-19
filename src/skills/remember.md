---
name: borhan-remember
description: "At the end of a working session, decide what from the conversation is worth keeping and store it in a borhan memory. Use when the user says they are finished, asks you to remember or save what happened, invokes /borhan-remember, or when a long session is about to be closed or compacted. Reads the memories that exist, asks the user which to write to with options rather than guessing, and files each turn under the right session, role and author. Pairs with borhan-survey: that one describes a project's code in a session named after the project, this one records what happened in a session named after the conversation — same memory, never the same session."
license: MIT
---

# borhan-remember

The end of a conversation is the last moment at which what happened is still
known. This skill spends that moment: read back over the session, pick the few
things worth keeping, and file them in a borhan memory so that a search months
from now finds them.

It is not a transcript dump. borhan will store every turn you hand it, and a
memory holding every turn is one where nothing ranks.

**Arguments to this skill are memory names separated by spaces** —
`/borhan-remember pouriya project_borhan`. They say *where*, not *what*, and
they do not remove the confirmation in step 3.

Work through the steps in order. The first two are cheap and can fail the whole
thing, so they come before you spend any effort on selection.

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

1. **MCP.** If your tool list holds `memory_list`, `memory_search`,
   `memory_add` and friends from a borhan server, use them. Prefer this: the
   tool schemas carry the argument rules where you will actually read them.
2. **The command line.** Otherwise run `borhan --version`. If that works, every
   operation below has a command form.
3. **Neither.** Stop. Tell the user that borhan is not reachable from here —
   neither an MCP server nor a `borhan` on `PATH` — and that they need to
   install it and either put it on `PATH` or point this agent's MCP
   configuration at a running `borhan serve` before there is anywhere to store
   anything. Do not write what you had in mind somewhere else, do not offer to
   keep it in a file, and do not go on to step 2. There is nothing further to
   do.

## 2. Check that it will accept a write, before composing anything

A borhan server declares what it will do by what it offers. One configured not
to store **does not list a `memory_add` tool at all** — absence is the whole of
the message. So:

- **MCP:** if `memory_search` is there and `memory_add` is not, this server does
  not store.
- **Command line:** the CLI routes through the server whenever one answers on
  the address in `server.toml`, so the same server refuses a local command too,
  with `refused: this server does not do add`.

### A refusal is an answer, not an obstacle

**Say so to the user and do nothing else.** It is somebody's decision, already
made, and reporting it is the whole of your job here.

- **Do not retry.** The request was not malformed; the second attempt is refused
  identically.
- **Do not go looking for `server.toml`,** do not edit it, do not restart
  anything, and do not reach around the server to the storage underneath. An
  agent that starts down that path spends the rest of the session on it instead
  of on what it was asked for.
- **Do not paste what you would have stored into the chat as a consolation.**
  The user asked for something kept; a wall of text they now have to file by
  hand is not a smaller version of that.
- **Then let them choose.** If they tell you to change the configuration, do it
  — that is a different instruction and it is theirs to give. Until they do, the
  refusal stands.

## 3. Read the memories, then ask

Always run `memory_list` (`borhan memory list`). Never skip it, even when the
arguments named a memory. Two reasons: a name you were handed may not exist, and
each memory's description says what belongs in it and what does not — which is
the thing you are about to decide.

**A memory is a subject, not a bucket.** A store usually holds several, and they
divide by what a thing is *about* rather than where it came from:

| memory | what belongs in it |
|---|---|
| `company` | how the organisation works: who owns which service, why a policy exists, what was tried before you arrived |
| `pouriya` | one person: what they do, how they want to be worked with, corrections they have made that will still hold next month |
| `project_borhan` | one project: its constraints, its decisions and the reasons behind them, its dead ends |
| `rfcs` | a document corpus, ingested rather than conversed |

Those four are illustrations. Read the real descriptions from `memory_list` and
route by them.

**One conversation usually splits across more than one memory.** A session in
which the user corrected your approach, settled an architectural question and
told you which staging cluster to deploy against has one item for their own
memory, one for the project's and one for the company's. Doing that split is
most of the work here. Putting all three wherever the first argument pointed is
how a store turns into a bucket, and a bucket is the thing that ranks nothing.

Then ask the user, in one question, and ask it as a **choice** rather than as an
open question — see *The one thing that must not go wrong* above for how:

- which memory or memories to write to, and which item goes where when they
  differ;
- with the items you propose to store listed, short enough to read at a glance;
- with *none of these — do not store any of it* as an option.

When the arguments named memories, they are the default answer — present them
selected and ask the user to confirm the *contents and the split*. Being handed a
memory name is not the same as being told what belongs in it. When they named a
memory that `memory_list` does not show, say so and ask; **do not create it.** A
new memory needs a description that every later caller will rely on, and that is
the user's sentence to write, not yours.

Write nothing until you have an answer. Silence is not an answer, and neither is
a message about something else.

## 4. Decide what is worth keeping

Keep what a future search would be glad to find:

- a decision, **with the reason** — the reason is the part that does not survive
  anywhere else;
- a correction the user made, and what it was correcting;
- a constraint or preference that will still be true next month;
- a fact about the environment, the data or the people that no repository
  records;
- a dead end and why it was dead, which is what stops it being tried again.

Leave out what is already written down somewhere with a better claim to it:
code and diffs (git has them), file listings, command output that can be
regenerated, the project's own documentation, and anything that is only true
inside this conversation. If you catch yourself storing something because it was
hard to produce rather than because it will be looked for, drop it.

Shape it for the way borhan splits text. **A message is stored as Markdown and
cut into units at paragraph boundaries, and a unit — not a message — is what
search scores and returns.** So:

- one idea per paragraph, blank line between; a wall of text with no blank lines
  is a single unit and a useless hit;
- open each paragraph with the words someone would search for, not with "as
  discussed above";
- if the memory is tagged `fa,en`, name the key terms in both languages at least
  once, or a Persian note stays invisible to an English query;
- keep the user's own wording for anything they were precise about. Your
  paraphrase is what will rank, and it should not be a worse version of what
  they said.

### If this session changed how the code works

Then some description in the project's **survey** session is now wrong, and it
will keep coming back on searches as though it were current.

You cannot fix that from here. This skill appends; correcting a survey means
replacing a specific message in the project session, which is `/borhan-survey`'s
job. So: note it in your report, name the features affected, and offer to run the
survey.

Do not "fix" it by writing a corrected description into this conversation's
session. It would sit in the memory beside the stale one with nothing saying which
is current — which is the exact failure these two skills are arranged to avoid.

## 5. File it under the right identity

This is where remembering most often goes wrong, because it is the part with no
visible consequence until someone searches.

**A conversation is a session.** One conversation, one session identifier,
forever. Use the host's own id for this conversation if you can name one;
otherwise a stable slug like `2026-09-02-skills-subcommand`. If you come back to
the same conversation later, reuse the identifier rather than opening a second
session — `memory_cursor` reads the units either side of a hit, and those
neighbours are only meaningful inside one session.

**Never file into a project's survey session.** A memory holding surveys has
sessions named after projects — `acme-api`, `project_borhan` — and they belong to
`/borhan-survey`, one message per feature. Writing a conversation into one buries
the feature descriptions among decisions nobody was looking for, and the two can
no longer be told apart on a result line. `borhan memory outline <memory>` shows
you the sessions if you are unsure which is which. Your session is named after
this conversation, always, even when everything in it is about one project.

If you cannot settle on a session identifier — no host id, and no obviously right
slug — that is a question for the user, with options, not a guess. A conversation
stored under two different ids is the same duplicate problem in miniature.

**You and the user are not the same author, and not the same role.**

| what it is | `role` | `author` |
|---|---|---|
| what the user said, decided or asked for | `user` | the name they go by |
| what you concluded, found or built | `assistant` | your agent or model name |
| output worth keeping from a command or tool | `tool` | the tool's name |

Never file your own summary of the user's decision under `role: user`. A later
search filtered to `user` is asking *what did the person actually say*, and a
paraphrase sitting there answers it wrongly and confidently. If you do not know
the name the user goes by, ask, or fall back to `user` — but do not invent one,
and do not store their email address.

**Message identifiers are your protection against storing twice.** Each message
carries the feeder's own id, and borhan refuses a second message with the same
id in the same session. Number them in order — `01`, `02`, `03` — so a second
run over the same conversation fails on what is already there instead of
doubling it. When a write is refused for that reason, that item is stored
already: skip it, and do not renumber to force it in.

One item, both call forms. Note that the memory is `pouriya` because the thing
being kept is about the person, not about the code it came up in:

    borhan memory add pouriya \
        "Prefers a second converter script over a flag on the existing one when
        the existing one documents a narrow safety claim — merging them would
        make neither claim checkable." \
        --session 2026-09-02-skills-subcommand \
        --message 01 \
        --role user \
        --author pouriya

    memory_add { memory, session, message, role, author, text }

## 6. Verify, then report

Search the memory for two or three distinctive words you just stored and check
that they come back. It costs one call and it is the difference between "stored"
and "reported as stored".

Then tell the user, in a few lines: which memory or memories, which session
identifier, how many messages went where, and what you deliberately left out.
The last part matters most — it is their only chance to say you dropped the one
thing they wanted kept.

And if step 4 found that this session made a survey description untrue, end with
that: which features, and the offer to run `/borhan-survey` to reconcile them.

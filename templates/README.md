# dmem templates

Starter content for a fresh memory. Nothing here is loaded automatically; you choose
what to apply. Each file is plain markdown with a small frontmatter header (`kind`,
`namespace`, `title`) that tells `dmem import` what it is.

| File | Kind | What it does |
|---|---|---|
| `persona.md` | persona | The agent's identity, injected at session start. **Edit the placeholders before importing.** |
| `save-discipline.md` | protocol | When and how the agent captures memory (drives the typed saves and the nudges). Generic, good as-is. |
| `behavioral-discipline.md` | protocol | How the agent works (recall first, verify before done, etc.). Generic, good as-is. |

## Use them

```bash
dmem template export ~/dmem-templates   # write these files somewhere to edit
# edit ~/dmem-templates/persona.md (fill in the <PLACEHOLDERS>)
dmem import ~/dmem-templates/persona.md # load one
dmem import ~/dmem-templates/           # or load the whole set
```

`dmem setup` also offers to apply the defaults, edit them, or import your own.

## What is generic vs yours

The two **protocols** are generic on purpose: they describe good memory hygiene and good
agent behavior for anyone, and they are what make the auto-capture and nudges useful. Keep
them as-is or trim to taste.

The **persona** ships as a skeleton with `<PLACEHOLDERS>`, not a real identity. Fill it in,
or replace it entirely with your own markdown file (same frontmatter). Only one persona is
active per tenant; importing a new one replaces it.

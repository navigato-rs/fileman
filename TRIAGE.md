# Organization triage

`.github/workflows/org-triage.md` is the source; its SHA-pinned gh-aw v0.88.2
lockfile is checked in. It runs from Fileman because an organization `.github`
repository was not accessible when this was implemented. Do not install a second
copy in Starcom or Sunset. The designated summary is Fileman issue **#81**.

## Activation

The workflow is **disabled by default**, including model calls. It is not enough
to merge the files. In Fileman's Actions configuration, provide:

- Secret `COPILOT_GITHUB_TOKEN`: a token with Copilot inference access. No repository
  write scope is needed by the model. Inference may incur provider charges.
- Variable `NAVIGATO_TRIAGE_APP_ID` and secret
  `NAVIGATO_TRIAGE_APP_PRIVATE_KEY`: a dedicated GitHub App installed only on
  Fileman, Starcom and Sunset. Give it Issues read/write and metadata read, not
  contents write, pull-request write or administration. Only deterministic safe
  output jobs receive this credential. The agent uses the read-only Actions token.
- Variable `NAVIGATO_TRIAGE_ENABLED=true`, after reviewing a staged trial. With it
  unset/false, the read gate returns without network requests or model activation.

Create any missing allowed labels (`bug`, `enhancement`, `needs-info`, `regression`,
`performance`) before enabling automatic labelling. The agent is told to use only
existing labels, never remove human labels or create labels. To preview writes,
compile with `--staged` in a review branch; inspect the generated job summary.

The one daily schedule is 10:23 UTC, with a manual dispatch too. Actions scheduling
is best effort. Watch the last successful **agent** run and failed-workflow
notifications; an empty/disabled successful run is not proof of a functioning
triage agent. Public-repository inactivity can disable schedules. No new failure
issues are created by the workflow.

## Boundaries

The Python gate reads only open issue metadata and comment metadata from the three
explicit repositories. It ignores bot-only updates and its summary issue, catches
edited human replies, uses the previous successful agent run's *start* as its
watermark, and fails closed on API errors or pagination/batch limits. Disabled and
no-work runs never advance that watermark. Its outputs contain only validated issue
references, not titles/body text interpolated into shell commands. No candidates
means no model request. A backlog over 30 changed issues requires maintainer review
rather than silently dropping work. Title-only edits and label-only changes do not
trigger a new review.

The agent has GitHub read tools, no shell or file-writing tools, and a network
sandbox. Inputs are untrusted. It cannot execute examples, inspect private email,
merge, close, assign, commit, create issues or post per-issue comments. Its safe
outputs can add only the five allowed labels across the three repositories and
update only the body of issue #81 (also guarded by its exact title prefix). A
prompt-injected label suggestion is still possible within those limits; review
bot output as advice. The summary suggests concrete follow-up questions without
spamming reporters. GitHub/provider logs may contain public issue text; no private
mail or app reports are supplied to this workflow.

## Validation and updating

```sh
python3 -m unittest discover -s scripts -p 'test_triage_gate.py'
gh aw compile org-triage --no-check-update --action-mode action \
  --action-tag 9271a1804551c0dc4fb0085a97979950aa2f8489
```

Use gh-aw **v0.88.2**, whose Linux compiler SHA-256 is pinned in CI. CI recompiles
and rejects a changed lockfile. Every action is pinned by commit and every runtime
container by digest. Updating gh-aw requires reviewing the generated manifest,
permissions, tools, targets and secrets again. The current private key is consumed
only by GitHub's token action in safe-output/conclusion jobs; Copilot credentials
are isolated to the inference pipeline. Standard optional gh-aw OTLP variables are
not configured by this change. Leave them unset unless separately approved.

A future move to `navigato-rs/.github` must disable this copy first and deliberately
update the read gate, summary target, credentials and documentation. Do not infer
success from compilation: a paid live agent run requires the maintainer's tokens
and enable switch, and has not been performed during implementation.

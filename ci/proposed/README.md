# Proposed workflows

Changes to `.github/workflows/` are written here and applied by the owner. The
route is LinkCtrl's, as adopted by TradeShop and Mustur; their `ci/README.md`
and `ci/proposed/README.md` carry the longer argument.

## Why a proposal and not a commit

The token the agent building this fork holds is a fine-grained PAT without the
`Workflows` permission, and that is deliberate: a workflow file is code that
runs with `GITHUB_TOKEN`, whose own `permissions:` block overrides the
repository's default. GitHub refuses the **push** of any branch touching
`.github/workflows/`, so a workflow change cannot even arrive as a pull request
from the agent. It arrives as a file here, at a path that is not
`.github/workflows/`.

Upstream's checks live in the workflow itself, so unlike the other repositories
there is no script layer a check can be added to without a proposal. Workflow
changes are rare here for the same reason: this fork changes what CI *is* only
when its cost, not its content, is the problem.

## Applying a proposal

```sh
git mv -f ci/proposed/ci.yml .github/workflows/ci.yml
git commit
```

A proposal whose content already matches the live file is a second copy free to
drift from the one that runs: delete it.

## Product impact

<!-- What changes for a user or operator? Keep this to the product outcomes. -->

## Verification

- [ ] `make ci`
- [ ] Rendered the touched overlays with `kubectl kustomize`
- [ ] Checked the deployment specialization is the only home of deployment-specific values

## Repo hygiene

- [ ] The project stays consumer-agnostic: no deployment-specific names in code, config, manifests, scripts, comments, or commit messages.
- [ ] No secrets or live deployment state are committed.

# parts/ — this experiment's own loadables

Local parts live here: a guest bundle, a container context, a config layer this experiment
needs and nothing else does. `parts.yaml` references them by relative path
(`./parts/my-bundle`), which is a *local* ref and needs no digest — it is content-hashed
into the results archive instead.

Keeping them inside the sandbox is what makes the experiment shareable: a workspace whose
parts live three directories up is not something anyone can unzip and run.

A part that outgrows one experiment belongs in `vxparts`, published and digest-pinned.
That is a promotion, not a move: swap the relative ref for the published one.

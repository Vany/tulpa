# TODO

- **A1:** Run blocking `ssh -G` resolution through `spawn_blocking` with a timeout so a stuck resolver cannot hang routing. This is intentionally not implemented with the multi-hop ProxyJump rejection fix.

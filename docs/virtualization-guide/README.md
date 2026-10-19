# Bedrock virtualization field guide

The standalone guide is [index.html](index.html). It has no build step or
runtime dependency beyond a browser. Interactive diagrams work without any
external JavaScript packages; if Google Fonts is unavailable, local fallback
fonts are used.

From this worktree, run:

```sh
./docs/virtualization-guide/serve.sh 8765
```

The server listens only on `127.0.0.1`. On your laptop, forward the port over
SSH to the development host:

```sh
ssh -L 8765:127.0.0.1:8765 ubuntu@dev-dancline
```

Then open `http://127.0.0.1:8765/docs/virtualization-guide/` locally. If
your SSH hostname differs, replace `dev-dancline` with the host you normally
use. The server needs to serve the repository root so the article's relative
links to code and performance evidence resolve.

This is documentation only. It does not build or load a kernel module.
